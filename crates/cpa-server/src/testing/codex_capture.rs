//! A capturing local Codex upstream for route tests.
//!
//! One loopback base URL serves both transports a Codex credential uses: HTTP requests
//! and Responses WebSocket upgrades. It records each request body and each client text
//! frame exactly as received, so a test can compare what reached the upstream with a
//! `cpa_exec::codex_contract` fixture byte for byte. Replies come from a [`Script`] in
//! order. The server binds `127.0.0.1` only. Point a test credential's `base_url` at
//! [`CaptureUpstream::base_url`].
//!
//! The server never negotiates permessage-deflate, so the frames it records are the
//! payloads the client wrote.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::FromRequestParts;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

/// One HTTP request as the upstream received it.
#[derive(Debug, Clone)]
pub struct Exchange {
    pub method: Method,
    pub uri: Uri,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// One accepted WebSocket: its handshake and the text frames the client sent.
#[derive(Debug, Clone)]
pub struct Socket {
    pub uri: Uri,
    pub headers: HeaderMap,
    pub frames: Vec<Bytes>,
}

/// A scripted HTTP reply.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl Reply {
    /// `200 text/event-stream` with `body` as the stream.
    pub fn sse(body: impl Into<Bytes>) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: body.into(),
        }
    }

    /// `status` with an `application/json` body.
    pub fn json(status: u16, body: impl Into<Bytes>) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.into(),
        }
    }

    fn into_response(self) -> Response {
        let mut response = (StatusCode::from_u16(self.status).unwrap(), self.body).into_response();
        for (name, value) in self.headers {
            response.headers_mut().append(
                HeaderName::try_from(name).unwrap(),
                HeaderValue::try_from(value).unwrap(),
            );
        }
        response
    }
}

/// What the upstream answers.
#[derive(Debug, Clone, Default)]
pub struct Script {
    /// Replies to HTTP requests, in order. Once exhausted, a request gets a 500.
    pub http: Vec<Reply>,
    /// Extra headers on every `101 Switching Protocols` response.
    pub ws_handshake: Vec<(String, String)>,
    /// Refuse every upgrade with this reply instead (for example 426, to exercise HTTP
    /// fallback). The refused handshake is still recorded in [`CaptureUpstream::sockets`].
    pub ws_reject: Option<Reply>,
    /// For each client text frame, in order across all sockets, the frames to send back.
    /// Once exhausted, a further client frame gets no reply.
    pub ws: Vec<Vec<Bytes>>,
}

#[derive(Default)]
struct State {
    http_replies: Mutex<VecDeque<Reply>>,
    ws_replies: Mutex<VecDeque<Vec<Bytes>>>,
    ws_handshake: Vec<(String, String)>,
    ws_reject: Option<Reply>,
    exchanges: Mutex<Vec<Exchange>>,
    sockets: Mutex<Vec<Socket>>,
}

/// A running capture upstream. It stops with the test's Tokio runtime.
pub struct CaptureUpstream {
    /// `http://127.0.0.1:<port>`, for a credential's `base_url`.
    pub base_url: String,
    state: Arc<State>,
}

impl CaptureUpstream {
    pub async fn start(script: Script) -> Self {
        let state = Arc::new(State {
            http_replies: Mutex::new(script.http.into()),
            ws_replies: Mutex::new(script.ws.into()),
            ws_handshake: script.ws_handshake,
            ws_reject: script.ws_reject,
            ..State::default()
        });
        let shared = state.clone();
        let app = axum::Router::new().fallback(move |req: axum::extract::Request| handle(shared.clone(), req));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self { base_url, state }
    }

    /// HTTP requests received so far.
    pub fn http(&self) -> Vec<Exchange> {
        self.state.exchanges.lock().unwrap().clone()
    }

    /// WebSocket handshakes received so far, with the frames each socket carried.
    pub fn sockets(&self) -> Vec<Socket> {
        self.state.sockets.lock().unwrap().clone()
    }
}

async fn handle(state: Arc<State>, req: axum::extract::Request) -> Response {
    let (mut parts, body) = req.into_parts();
    if parts.headers.contains_key("sec-websocket-key") {
        let index = {
            let mut sockets = state.sockets.lock().unwrap();
            sockets.push(Socket {
                uri: parts.uri.clone(),
                headers: parts.headers.clone(),
                frames: Vec::new(),
            });
            sockets.len() - 1
        };
        if let Some(reject) = state.ws_reject.clone() {
            return reject.into_response();
        }
        let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
            Ok(upgrade) => upgrade,
            Err(rejection) => return rejection.into_response(),
        };
        let extra = state.ws_handshake.clone();
        let mut response = upgrade.on_upgrade(move |socket| relay(state, index, socket));
        for (name, value) in extra {
            response.headers_mut().append(
                HeaderName::try_from(name).unwrap(),
                HeaderValue::try_from(value).unwrap(),
            );
        }
        response
    } else {
        let body = axum::body::to_bytes(body, usize::MAX).await.unwrap_or_default();
        state.exchanges.lock().unwrap().push(Exchange {
            method: parts.method,
            uri: parts.uri,
            headers: parts.headers,
            body,
        });
        let reply = state.http_replies.lock().unwrap().pop_front();
        match reply {
            Some(reply) => reply.into_response(),
            None => (StatusCode::INTERNAL_SERVER_ERROR, "capture upstream: no scripted reply").into_response(),
        }
    }
}

async fn relay(state: Arc<State>, index: usize, mut socket: WebSocket) {
    while let Some(Ok(message)) = socket.recv().await {
        let frame = match message {
            Message::Text(text) => Bytes::copy_from_slice(text.as_str().as_bytes()),
            Message::Close(_) => break,
            _ => continue,
        };
        state.sockets.lock().unwrap()[index].frames.push(frame);
        let replies = state.ws_replies.lock().unwrap().pop_front().unwrap_or_default();
        for reply in replies {
            // A scripted frame that is not UTF-8 is a test bug: fail loudly.
            let text = String::from_utf8(reply.to_vec()).expect("scripted WebSocket frame is not UTF-8");
            if socket.send(Message::Text(text.into())).await.is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
#[path = "codex_capture_tests.rs"]
mod tests;
