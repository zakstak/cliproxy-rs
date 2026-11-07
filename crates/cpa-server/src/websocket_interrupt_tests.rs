//! `response.interrupt` acknowledgements through the whole handler, as current Codex
//! (`codex-rs/codex-api/src/endpoint/responses_websocket.rs`, e0aa043) waits for them:
//! after it interrupts a response it reads until the response's terminal event and a
//! `response.interrupt.accepted` (or `.failed`) with the same `response_id` both
//! arrived. Loopback upstreams, fake credentials, a private auth-dir.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message as AxMessage, WebSocket as AxSocket, WebSocketUpgrade};
use cpa_core::config::Config;
use cpa_exec::Executors;
use futures_util::StreamExt;
use wreq::ws::message::Message as WsMessage;

use crate::router;

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("127.0.0.1:{}", addr.port())
}

/// A WebSocket upstream answering the n-th client frame with `script[n]`; it records
/// the frames and counts dials.
async fn ws_upstream(script: Vec<Vec<&'static str>>) -> (String, Arc<Mutex<Vec<String>>>, Arc<Mutex<usize>>) {
    let frames: Arc<Mutex<Vec<String>>> = Arc::default();
    let dials: Arc<Mutex<usize>> = Arc::default();
    let script = Arc::new(Mutex::new(std::collections::VecDeque::from(script)));
    let state = (frames.clone(), dials.clone(), script);
    let url = serve(axum::Router::new().fallback(move |ws: WebSocketUpgrade| {
        let (frames, dials, script) = state.clone();
        async move {
            *dials.lock().unwrap() += 1;
            ws.on_upgrade(move |mut socket: AxSocket| async move {
                while let Some(Ok(AxMessage::Text(text))) = socket.recv().await {
                    frames.lock().unwrap().push(text.as_str().to_owned());
                    let reply = script.lock().unwrap().pop_front().unwrap_or_default();
                    for event in reply {
                        if socket.send(AxMessage::Text(event.into())).await.is_err() {
                            return;
                        }
                    }
                }
            })
        }
    }))
    .await;
    (url, frames, dials)
}

struct Proxy {
    socket: wreq::ws::WebSocket,
    /// Event types read so far, for failure messages.
    seen: Vec<String>,
    dir: std::path::PathBuf,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The proxy with one Codex API key (`websockets` as given) and a client connected.
async fn proxy(upstream: &str, websockets: bool) -> Proxy {
    let dir = std::env::temp_dir().join(format!("cpa-ws-interrupt-ack-{}", uuid::Uuid::new_v4()));
    let ws = if websockets { "    websockets: true\n" } else { "" };
    let cfg = Config::parse(&format!(
        "auth-dir: {}\ncodex-api-key:\n  - api-key: sk-FAKE\n    base-url: http://{upstream}\n{ws}    models:\n      - name: gpt-fixture\n",
        dir.join("auths").display()
    ))
    .unwrap();
    let credentials = cpa_core::config::credentials::from_config(&cfg);
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let proxy = serve(router(Arc::new(crate::testing::runtime(cfg, credentials, executors)))).await;
    let socket = wreq::Client::new()
        .websocket(format!("ws://{proxy}/v1/responses"))
        .send()
        .await
        .unwrap()
        .into_websocket()
        .await
        .unwrap();
    Proxy {
        socket,
        seen: Vec::new(),
        dir,
    }
}

impl Proxy {
    async fn send(&mut self, text: &str) {
        self.socket.send(WsMessage::text(text.to_owned())).await.unwrap();
    }

    /// The next text frame; Codex's own drain deadline is one second, so a frame later
    /// than three is as good as missing.
    async fn read(&mut self) -> String {
        let text = match tokio::time::timeout(Duration::from_secs(3), self.socket.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => text.as_str().to_owned(),
            other => panic!("no text frame after {:?}: {other:?}", self.seen),
        };
        self.seen.push(text.clone());
        text
    }

    async fn read_kind(&mut self) -> String {
        let text = self.read().await;
        gjson::get(&text, "type").str().to_owned()
    }
}

const CREATE: &str = r#"{"type":"response.create","model":"gpt-fixture","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#;
const CREATE_R2: &str = r#"{"type":"response.create","model":"gpt-fixture","previous_response_id":"r1","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"next"}]}]}"#;
const INTERRUPT_R1: &str = r#"{"type":"response.interrupt","response_id":"r1","mode":"discard_partial_items"}"#;
const ACCEPTED_R1: &str = r#"{"type":"response.interrupt.accepted","response_id":"r1"}"#;
const CREATED_R1: &str = r#"{"type":"response.created","response":{"id":"r1","status":"in_progress","output":[]}}"#;
const COMPLETED_R1: &str = r#"{"type":"response.completed","response":{"id":"r1","status":"completed","output":[],"usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}}}"#;
const INTERRUPTED_R1: &str = r#"{"type":"response.incomplete","response":{"id":"r1","status":"incomplete","incomplete_details":{"reason":"interrupted"},"output":[],"usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}}}"#;
const CREATED_R2: &str = r#"{"type":"response.created","response":{"id":"r2","status":"in_progress","output":[]}}"#;
const COMPLETED_R2: &str = r#"{"type":"response.completed","response":{"id":"r2","status":"completed","output":[],"usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}}}"#;

/// The acknowledgement upstream sends after the terminal event reaches the client, and
/// the next request on the connection reuses the socket without seeing it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_upstream_acknowledgement_after_the_terminal_event_reaches_the_client() {
    let (upstream, frames, dials) = ws_upstream(vec![
        vec![CREATED_R1],
        vec![INTERRUPTED_R1, ACCEPTED_R1],
        vec![CREATED_R2, COMPLETED_R2],
    ])
    .await;
    let mut client = proxy(&upstream, true).await;
    client.send(CREATE).await;
    assert_eq!(client.read_kind().await, "response.created");
    client.send(INTERRUPT_R1).await;
    assert_eq!(client.read_kind().await, "response.incomplete");
    assert_eq!(client.read().await, ACCEPTED_R1, "the acknowledgement is unchanged");
    client.send(CREATE_R2).await;
    assert_eq!(client.read_kind().await, "response.created");
    assert_eq!(client.read_kind().await, "response.completed");
    let frames = frames.lock().unwrap().clone();
    assert_eq!(frames.len(), 3, "{frames:?}");
    assert_eq!(frames[1], INTERRUPT_R1, "the interrupt reaches upstream unchanged");
    assert_eq!(*dials.lock().unwrap(), 1, "one socket");
}

/// The interrupt arrives after the proxy finished the response: it goes upstream, and
/// upstream's acknowledgement reaches the client before the next request runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_interrupt_gets_upstreams_acknowledgement() {
    let (upstream, frames, dials) = ws_upstream(vec![
        vec![CREATED_R1, COMPLETED_R1],
        vec![ACCEPTED_R1],
        vec![CREATED_R2, COMPLETED_R2],
    ])
    .await;
    let mut client = proxy(&upstream, true).await;
    client.send(CREATE).await;
    assert_eq!(client.read_kind().await, "response.created");
    assert_eq!(client.read_kind().await, "response.completed");
    client.send(INTERRUPT_R1).await;
    assert_eq!(client.read().await, ACCEPTED_R1);
    client.send(CREATE_R2).await;
    assert_eq!(client.read_kind().await, "response.created");
    assert_eq!(client.read_kind().await, "response.completed");
    let frames = frames.lock().unwrap().clone();
    assert_eq!(frames.len(), 3, "{frames:?}");
    assert_eq!(frames[1], INTERRUPT_R1);
    assert_eq!(*dials.lock().unwrap(), 1, "one socket");
}

/// An HTTP turn has no upstream socket: the proxy cancels it and is the one that accepts
/// the interrupt, before the interrupted `response.incomplete`. An interrupt for the
/// response it just finished is accepted too instead of rejected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_http_turn_acknowledges_its_local_interrupt() {
    let calls: Arc<Mutex<usize>> = Arc::default();
    let counter = calls.clone();
    let upstream = serve(axum::Router::new().fallback(move || {
        let call = {
            let mut calls = counter.lock().unwrap();
            *calls += 1;
            *calls
        };
        async move {
            let first = concat!(
                "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\",\"status\":\"in_progress\",\"output\":[]}}\n\n",
                "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"id\":\"msg_partial\",\"content\":[{\"type\":\"output_text\",\"text\":\"partial\"}]}}\n\n",
            );
            let second = concat!(
                "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r2\",\"status\":\"in_progress\",\"output\":[]}}\n\n",
                "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r2\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":2,\"total_tokens\":5}}}\n\n",
            );
            let head = futures_util::stream::once(async move {
                Ok::<_, std::io::Error>(axum::body::Bytes::from(if call == 1 { first } else { second }))
            });
            // The first response never ends on its own.
            let body = if call == 1 {
                axum::body::Body::from_stream(head.chain(futures_util::stream::pending()))
            } else {
                axum::body::Body::from_stream(head)
            };
            ([("content-type", "text/event-stream")], body)
        }
    }))
    .await;
    let mut client = proxy(&upstream, false).await;
    client.send(CREATE).await;
    assert_eq!(client.read_kind().await, "response.created");
    assert_eq!(client.read_kind().await, "response.output_item.done");
    client.send(INTERRUPT_R1).await;
    assert_eq!(client.read().await, ACCEPTED_R1, "accepted before the terminal event");
    let incomplete = client.read().await;
    assert_eq!(
        gjson::get(&incomplete, "type").str(),
        "response.incomplete",
        "{incomplete}"
    );
    assert_eq!(gjson::get(&incomplete, "response.id").str(), "r1");
    client.send(INTERRUPT_R1).await;
    assert_eq!(client.read().await, ACCEPTED_R1, "the finished response's interrupt");
    client.send(CREATE_R2).await;
    assert_eq!(client.read_kind().await, "response.created");
    assert_eq!(client.read_kind().await, "response.completed");
    assert_eq!(*calls.lock().unwrap(), 2);
}
