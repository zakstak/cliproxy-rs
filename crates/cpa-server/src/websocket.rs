//! Responses WebSocket: `GET /v1/responses` and `GET /backend-api/codex/responses`
//! (sdk/api/handlers/openai/openai_responses_websocket.go, _forward.go).
//!
//! One task per downstream connection. It takes one request, runs it to completion and
//! only then takes the next, so a connection holds at most one turn, one upstream stream
//! and the transcript of its last turn. Nothing polls: between turns the task waits on the
//! client and on the executor's upstream-loss signal; during a turn on the stream and an
//! optional keep-alive deadline. When the client goes away the task ends and releases the
//! executor session (`Executors::close_session`).
//!
//! One reader task owns the client's half of the socket for the whole connection
//! (`readResponsesWebsocketInput`): requests go through a bounded queue, and a client that
//! goes away cancels the turn in flight. A `response.interrupt` frame skips the queue: it
//! goes unchanged to the Codex socket of the turn in progress, or ends an HTTP turn in
//! flight locally with an interrupted `response.incomplete`; anything else is rejected
//! with an error event.
//!
//! Turns go upstream in one of two modes, decided by the credential that served the last
//! turn:
//! - WebSocket (Codex credential with `websockets: true`): the client's frames pass
//!   through with `previous_response_id` and incremental input, pinned to that credential's
//!   socket. A continuation that cannot run there closes with 1012 so the client replays.
//! - HTTP: the handler keeps the transcript and sends each turn as a full request.
//!
//! Response steering (`codex.response-steering`): a turn served by a Codex credential in
//! WebSocket mode runs full duplex. The executor keeps the upstream socket for the rest of
//! the connection and, after the first `response.created`, takes the client's later frames
//! (`response.steer`, more creates) from the connection's frame queue. That turn ends only
//! with the connection; its usage is recorded per upstream response as each one settles
//! (the executor detaches the turn's record), not when the turn ends.
//!
//! Handshake metadata: this 101 precedes the first request, which chooses the credential
//! and the model, so the upstream socket's handshake comes later. A turn that opens a
//! Codex socket passes its `x-codex-turn-state` and `openai-model` on as a
//! `response.metadata` event before its first event (`handshake_metadata_event`). A
//! later request on that socket gets only the socket's `openai-model` the same way; turn
//! state is never repeated.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Extension;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use cpa_common::gostr::GoStr;
use cpa_common::rawjson;
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_core::exec::{Caller, ExecError, ExecSession, ExecStream, Operation};
use cpa_core::format::Format;
use cpa_exec::codex::{CodexExecutor, InterruptError, SteeringInput};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{FutureExt, SinkExt, StreamExt};
use tokio::sync::{Notify, mpsc, watch};

use crate::registry::provider_key;
use crate::runtime::Runtime;
use crate::websocket_requests::{
    self as requests, APPEND, CREATE, Turn, WsError, delete, error_payload, field, payloads_from_chunk, set_raw,
    set_str,
};
use crate::websocket_tools::{self as tools, Retained, TurnCache};
use crate::{classify, dispatch};

pub fn routes() -> Router<Arc<Runtime>> {
    Router::new()
        .route("/v1/responses", get(upgrade))
        .route("/backend-api/codex/responses", get(upgrade))
}

/// `wsCloseReasonMaxBytes`.
const CLOSE_REASON_MAX: usize = 123;
/// Close code 1012 (service restart): the client must replay over HTTP.
const CLOSE_SERVICE_RESTART: u16 = 1012;
/// Close code 1009 (message too big).
const CLOSE_MESSAGE_TOO_BIG: u16 = 1009;
const TURN_STATE: &str = "x-codex-turn-state";

/// The error a `generate: false` prewarm gets under Home before this connection's first
/// pick, when no credential or transport is known yet.
const HOME_PREWARM_UNSUPPORTED: &str = "response.create with generate: false (prewarm) is not supported before Home selects a credential for this connection: the serving transport is unknown, and the proxy does not invent a response. Send the first request without generate: false.";

/// The error a `generate: false` prewarm gets when only the HTTP fallback can serve it.
const CODEX_PREWARM_UNSUPPORTED: &str = "response.create with generate: false (prewarm) is not supported for Codex credentials without websockets: there is no upstream socket to prewarm, and the proxy does not invent a response. Enable websockets on the Codex credential, or send the request without generate: false.";
// ponytail: gorilla reads messages of any size; 64 MiB (the HTTP body cap) bounds
// per-connection memory instead.
const MAX_MESSAGE: usize = crate::MAX_REQUEST_BYTES;
/// Bound on a terminal close or error write. Go closes the socket at once when a data
/// write is still in flight (`closeForUpstreamError`); here the write may have been
/// interrupted with bytes still queued, and a client that stopped reading would hold the
/// flush, the connection task and its executor session forever.
// ponytail: one fixed best-effort bound instead of Go's in-flight-writer check.
const TERMINAL_WRITE: Duration = Duration::from_secs(1);
/// `readResponsesWebsocketInput`'s queue: client frames backpressure the client.
const INPUT_QUEUE: usize = 16;

/// The connection's write half, shared with the reader, which writes the error for a
/// rejected `response.interrupt` (Go's `responsesWebsocketWriter`).
type Writer = Arc<tokio::sync::Mutex<SplitSink<WebSocket, Message>>>;
/// The connection's request-log timeline, shared with the reader for its interrupt
/// diagnostics. `None` when request logging is off.
type Timeline = Option<Arc<crate::request_logging::WebsocketLog>>;

/// The connection's write half (reads go through [`Client`]) and its request-log
/// timeline (Go `websocketTimelineLog`).
struct Sink {
    inner: Writer,
    timeline: Timeline,
    /// Go's `wsTerminateErr`: why the connection ended, the final `websocket.disconnect`.
    terminated: Option<String>,
}

/// Go `websocketTimelineLog.Append`: one `formatWebsocketTimelineEvent` part, which the
/// log keeps scrubbed of `sent` (#25).
fn timeline_event(
    timeline: Option<&crate::request_logging::WebsocketLog>,
    kind: &str,
    payload: &[u8],
    sent: &cpa_core::exec::SentCredentials,
) {
    let Some(timeline) = timeline else {
        return;
    };
    let payload = payload.trim_ascii();
    if payload.is_empty() {
        return;
    }
    let mut part = format!(
        "Timestamp: {}\nEvent: websocket.{kind}\n",
        crate::request_logging::timestamp(chrono::Local::now())
    )
    .into_bytes();
    part.extend_from_slice(payload);
    part.push(b'\n');
    // Go only warns when a timeline part cannot be written.
    let _ = timeline.append_part(&part, sent);
}

/// gorilla's text for the error after this side closed the connection.
const CLOSE_SENT: &str = "websocket: close sent";

impl Sink {
    fn event(&self, kind: &str, payload: &[u8]) {
        self.upstream_event(kind, payload, &Default::default());
    }

    /// [`Sink::event`] for a payload taken from an upstream answer whose request or
    /// socket carried `sent`.
    fn upstream_event(&self, kind: &str, payload: &[u8], sent: &cpa_core::exec::SentCredentials) {
        timeline_event(self.timeline.as_deref(), kind, payload, sent);
    }

    async fn send(&self, message: Message) -> Result<(), axum::Error> {
        self.inner.lock().await.send(message).await
    }

    fn terminate(&mut self, reason: &str) {
        self.terminated = Some(reason.to_owned());
    }

    /// The connection's end: the deferred `appendWebsocketTimelineDisconnect`, the socket
    /// closed at once (a pending close reply gets `TERMINAL_WRITE`, gorilla's bounded
    /// control write), the reader finished so its interrupt diagnostics are in, and only
    /// then the timeline's delivery, which can wait on disk or Home forwarding (Go closes
    /// TCP before its deferred log write).
    async fn finish(mut self, client: Client) {
        if let Some(reason) = self.terminated.take() {
            self.event("disconnect", reason.as_bytes());
        }
        let Self { inner, timeline, .. } = self;
        let _ = tokio::time::timeout(TERMINAL_WRITE, async { inner.lock().await.flush().await }).await;
        client.join().await;
        drop(inner);
        // The reader has ended, so this is the last reference.
        if let Some(timeline) = timeline.and_then(Arc::into_inner) {
            timeline.close().await;
        }
    }
}

/// gorilla `returnError` for a request that is not a websocket handshake: plain status
/// text and the supported version.
pub(crate) fn upgrade_rejected() -> Response {
    let mut response = (StatusCode::BAD_REQUEST, "Bad Request\n").into_response();
    let h = response.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert(header::SEC_WEBSOCKET_VERSION, HeaderValue::from_static("13"));
    response
}

async fn upgrade(
    State(rt): State<Arc<Runtime>>,
    Extension(caller): Extension<Caller>,
    peer: dispatch::Peer,
    matched: axum::extract::MatchedPath,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let Ok(ws) = ws else {
        return upgrade_rejected();
    };
    // `websocketUpgradeHeaders`: keep sticky turn state across reconnects.
    let turn_state = headers
        .get(TURN_STATE)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .and_then(|v| HeaderValue::from_str(v).ok());
    let mut connection = Connection::new(rt, caller, headers);
    // Go `request_path` metadata: gin FullPath of the upgrade route.
    connection.request_path = matched.as_str().to_owned();
    connection.peer = dispatch::peer(peer);
    // The upgrade request's log: Go writes the WebSocket timeline and every turn's
    // upstream capture into the gin context of the upgrade.
    if let Some(log) = crate::request_logging::current() {
        connection.capture = log.capture_sink();
        connection.request_id = Some(log.request_id()).filter(|id| !id.is_empty());
        connection.timeline = log.detach_websocket();
    }
    let mut response = ws
        .max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_MESSAGE)
        .on_upgrade(move |socket| connection.run(socket));
    if let Some(turn_state) = turn_state {
        response.headers_mut().insert(TURN_STATE, turn_state);
    }
    response
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Unknown,
    Websocket,
    Http,
}

/// How a turn ended.
enum Flow {
    Next,
    End,
}

/// A terminal turn failure and how the downstream connection learns about it.
struct Failure {
    status: u16,
    text: String,
    /// The raw upstream `error` event, sent as-is when exposed.
    payload: Option<String>,
    replay: bool,
    /// Go `ErrorMessage.Addon`: copied into the error event.
    addon: Option<Box<HeaderMap>>,
    /// The client-safe subscription-limit event a native Codex client gets
    /// (`websocket_limit`), when the failure is one.
    limit: Option<String>,
    /// The provider's own failure frame a native Codex turn delivers as sent
    /// ([`ExecError::delivered_event`]); never set for an account failure.
    delivered: Option<String>,
    /// A native Codex failure whose frame, and so its `text`, can name the pooled account
    /// ([`cpa_core::exec::Delivery::Withheld`]) or carries the upstream credential
    /// ([`cpa_core::exec::Delivery::Confidential`]): the client gets `limit` (never for a
    /// confidential failure) or a silent close, nothing else.
    withheld: bool,
    /// The values a confidential failure carries, which the timeline keeps `text`
    /// scrubbed of ([`cpa_core::exec::Delivery::Confidential`]).
    sent: cpa_core::exec::SentCredentials,
}

impl Failure {
    fn from_exec(e: &ExecError) -> Self {
        let text = String::from_utf8_lossy(&e.body).into_owned();
        Self {
            limit: exec_limit(e.status, &text, e),
            status: e.status,
            text,
            payload: None,
            replay: e.is_replay_required(),
            addon: e.addon.clone(),
            delivered: delivered(e),
            withheld: withheld(e),
            sent: sent(e),
        }
    }

    fn from_dispatch(failure: &dispatch::Failure) -> Self {
        match failure {
            dispatch::Failure::Exec(e) => {
                let (status, text) = (classify::response_status(e), classify::error_text(e));
                Self {
                    limit: exec_limit(status, &text, e),
                    status,
                    text,
                    payload: None,
                    replay: e.is_replay_required(),
                    addon: e.addon.clone(),
                    delivered: delivered(e),
                    withheld: withheld(e),
                    sent: sent(e),
                }
            }
            // A trusted control-plane rejection keeps its Go rendering; only the limit
            // event is added.
            // Also an upstream executor error that a Home round ended with a retry hint:
            // a native turn's frame still reaches the client as sent.
            dispatch::Failure::Remote { error: e, .. } => Self {
                limit: exec_limit(classify::response_status(e), &classify::error_text(e), e),
                delivered: delivered(e),
                withheld: withheld(e),
                sent: sent(e),
                ..Self::local(failure.status(), &failure.text())
            },
            dispatch::Failure::Cooldown {
                wait,
                cause: Some(cause),
                ..
            } => Self {
                limit: crate::websocket_limit::cooldown(cause, *wait, unix_now()),
                ..Self::local(failure.status(), &failure.text())
            },
            other => Self::local(other.status(), &other.text()),
        }
    }

    /// A wrapped upstream `error` event that reached the stream.
    fn from_event(status: u16, payload: &str) -> Self {
        Self {
            status,
            text: payload.to_owned(),
            payload: Some(payload.to_owned()),
            replay: false,
            addon: None,
            limit: crate::websocket_limit::from_error_event(payload, unix_now()),
            delivered: None,
            withheld: false,
            sent: Default::default(),
        }
    }

    fn local(status: u16, text: &str) -> Self {
        Self {
            status,
            text: text.into(),
            payload: None,
            replay: false,
            addon: None,
            limit: None,
            delivered: None,
            withheld: false,
            sent: Default::default(),
        }
    }
}

fn delivered(e: &ExecError) -> Option<String> {
    e.delivered_event()
        .map(|frame| String::from_utf8_lossy(frame).into_owned())
}

fn withheld(e: &ExecError) -> bool {
    e.withheld()
}

fn sent(e: &ExecError) -> cpa_core::exec::SentCredentials {
    match &e.delivery {
        cpa_core::exec::Delivery::Confidential(sent) => sent.clone(),
        _ => Default::default(),
    }
}

/// The limit event for an executor error: the frame's headers (addon) first, then the
/// upstream response's. None for a failure that carries an upstream authentication value
/// ([`cpa_core::exec::Delivery::Confidential`]): its allowlisted fields come from that
/// failure too.
fn exec_limit(status: u16, text: &str, e: &ExecError) -> Option<String> {
    if e.confidential() {
        return None;
    }
    let maps: Vec<&HeaderMap> = e.addon.as_deref().into_iter().chain([&*e.headers]).collect();
    crate::websocket_limit::event(status, text, &crate::websocket_limit::Headers::Maps(&maps), unix_now())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

struct Connection {
    rt: Arc<Runtime>,
    caller: Caller,
    headers: HeaderMap,
    /// An official Codex client (`User-Agent` or `originator`): it gets subscription-limit
    /// failures as the structured event Codex classifies, where Go closes silently.
    codex_client: bool,
    /// Go `request_path` metadata for payload rules.
    request_path: String,
    /// The downstream peer (usage records).
    peer: Option<std::net::SocketAddr>,
    /// Execution session id (`passthroughSessionID`).
    session: String,
    /// Tool-cache key (`websocketDownstreamSessionKey`).
    tool_key: String,
    last_request: String,
    last_output: String,
    last_id: String,
    /// Remains set until a generating request commits after a local warm-up.
    pending_prewarm: String,
    pending_calls: Vec<String>,
    pinned: String,
    /// Upstream credential affinity per provider: (credential id, model key).
    pinned_by_provider: HashMap<String, (String, String)>,
    passthrough_model: String,
    mode: Mode,
    upstream_auth: String,
    /// Response steering is configured: the client's frames go through the shared queue.
    steering: bool,
    /// The turn's selected credential runs full duplex (`codexDuplexStream`); its stream
    /// owns the connection's closure.
    duplex: Arc<AtomicBool>,
    /// The turn is a native continuation that attempted its pinned credential, so a
    /// 401/429 makes the client replay over HTTP (1012) rather than close on the loss.
    replays: Arc<AtomicBool>,
    /// The turn's request got its stream: an attempt was served. Until then a session loss
    /// belongs to a socket the turn's own attempt already gave up (its failure is the
    /// turn's to fail over or to deliver), and it does not end the turn.
    serving: Arc<AtomicBool>,
    /// The Codex session's socket losses ([`cpa_exec::codex::SessionLosses`]), sharing
    /// what the connection's loss watch took: a turn's failure renders only a loss of the
    /// latest socket that nobody took yet. Set when the connection runs.
    losses: Option<cpa_exec::codex::SessionLosses>,
    /// The upgrade request's log: upstream capture for every turn and its middleware ID.
    capture: cpa_core::exec::CaptureSink,
    request_id: Option<String>,
    /// The upgrade request's WebSocket timeline, detached before the 101 (None when
    /// request logging is off).
    timeline: Option<crate::request_logging::WebsocketLog>,
    /// The Home pick the session's pooled upstream socket keeps between turns.
    home: Arc<crate::home_session::SessionHome>,
    /// Lets the reader end the HTTP turn in flight (`responsesLocalInterrupt`).
    local: Arc<LocalInterrupt>,
}

/// The downstream reader task and the queue it fills.
struct Client {
    /// The client's requests in arrival order; a duplex turn shares the queue.
    frames: cpa_exec::codex::ClientFrames,
    /// Why the read ended (gorilla's text, Go's `wsTerminateErr`), set just before the
    /// reader exits. The reader holds the sender, so its exit is Go's request
    /// cancellation.
    ended: watch::Receiver<Option<String>>,
    stop: Arc<Notify>,
    reader: tokio::task::JoinHandle<()>,
}

impl Client {
    fn spawn(stream: SplitStream<WebSocket>, control: Control) -> Self {
        let (tx, rx) = mpsc::channel(INPUT_QUEUE);
        let (ended_tx, ended) = watch::channel(None);
        let stop = Arc::new(Notify::new());
        let reader = tokio::spawn(read_input(stream, tx, ended_tx, stop.clone(), control));
        Self {
            frames: Arc::new(tokio::sync::Mutex::new(rx)),
            ended,
            stop,
            reader,
        }
    }

    /// The next text or binary frame; `None` once the client is gone. A frame queued
    /// before the client left would start a turn that the cancellation ends at once;
    /// skip it.
    async fn next(&mut self) -> Option<Bytes> {
        let Self { frames, ended, .. } = self;
        tokio::select! {
            biased;
            () = gone(ended) => None,
            frame = async { frames.lock().await.recv().await } => frame,
        }
    }

    /// Why the read ended (Go's `wsTerminateErr`).
    fn close_text(&self) -> Option<String> {
        self.ended.borrow().clone()
    }

    /// Resolves once the client went away.
    async fn gone(&mut self) {
        gone(&mut self.ended).await;
    }

    /// Stops reading: the socket closes once the connection drops its write half.
    fn stop(&self) {
        self.stop.notify_one();
    }

    /// Waits for the reader to finish an interrupt it is handling, at most
    /// `TERMINAL_WRITE` (Go closes the socket under it instead).
    async fn join(mut self) {
        if tokio::time::timeout(TERMINAL_WRITE, &mut self.reader).await.is_err() {
            self.reader.abort();
            let _ = self.reader.await;
        }
    }
}

async fn gone(ended: &mut watch::Receiver<Option<String>>) {
    // `changed` fails once the reader dropped the sender.
    while ended.changed().await.is_ok() {}
}

/// `readResponsesWebsocketInput`: the only downstream reader. Interrupts are handled
/// at once, so they do not wait behind the response in flight; the bounded queue
/// backpressures the client instead of retaining unlimited input.
async fn read_input(
    mut stream: SplitStream<WebSocket>,
    frames: mpsc::Sender<Bytes>,
    ended: watch::Sender<Option<String>>,
    stop: Arc<Notify>,
    control: Control,
) {
    let cause = loop {
        let message = tokio::select! {
            biased;
            () = stop.notified() => return,
            message = stream.next() => message,
        };
        // gorilla's `ReadMessage` error: the peer's close frame, else an abnormal closure.
        // The close ends the read at once; the reply is sent within `TERMINAL_WRITE`
        // when the connection ends.
        let frame = match message {
            Some(Ok(Message::Text(text))) => Bytes::from(text),
            Some(Ok(Message::Binary(bytes))) => bytes,
            Some(Ok(Message::Close(Some(frame)))) => break gorilla_close(frame.code, frame.reason.as_str()),
            Some(Ok(Message::Close(None))) => break gorilla_close(1005, ""),
            Some(Ok(_)) => continue,
            Some(Err(_)) | None => break gorilla_close(1006, "unexpected EOF"),
        };
        match control.interrupt(&frame).await {
            Some(Ok(())) => continue,
            Some(Err(write)) => break write,
            None => {}
        }
        tokio::select! {
            biased;
            () = stop.notified() => return,
            sent = frames.send(frame) => if sent.is_err() {
                return;
            },
        }
    };
    ended.send_replace(Some(cause));
}

/// `responsesLocalInterrupt`: the reader hands an interrupt to the turn in flight when
/// no upstream socket can take it, which ends an HTTP turn locally.
#[derive(Default)]
struct LocalInterrupt {
    state: std::sync::Mutex<LocalState>,
    ready: Notify,
}

#[derive(Default)]
struct LocalState {
    /// A turn is forwarding.
    forwarding: bool,
    /// The first interrupt delivered to it.
    pending: Option<String>,
    /// The response the connection last sent a terminal event for.
    finished: String,
}

/// Where [`LocalInterrupt::deliver`] put an interrupt.
#[derive(PartialEq, Eq)]
enum Local {
    /// The turn in flight ends with it.
    Delivered,
    /// It names the response the connection already finished: there is nothing left to
    /// stop, so the reader acknowledges it at once.
    Finished,
    /// No turn takes it.
    Refused,
}

impl LocalInterrupt {
    fn state(&self) -> std::sync::MutexGuard<'_, LocalState> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The turn starts forwarding; the returned guard ends it.
    fn begin(&self) -> LocalTurn<'_> {
        self.state().forwarding = true;
        LocalTurn(self)
    }

    /// The connection sent the terminal event of response `id`.
    fn finish(&self, id: &str) {
        id.clone_into(&mut self.state().finished);
    }

    fn deliver(&self, payload: &str) -> Local {
        let id = field(payload, "response_id");
        let mut state = self.state();
        if !state.finished.is_empty() && state.finished == id {
            return Local::Finished;
        }
        if !state.forwarding {
            return Local::Refused;
        }
        if state.pending.is_none() {
            state.pending = Some(payload.to_owned());
            self.ready.notify_one();
        }
        #[cfg(test)]
        gate::reached("deliver", payload);
        Local::Delivered
    }

    async fn next(&self) -> String {
        loop {
            let ready = self.ready.notified();
            if let Some(payload) = self.state().pending.take() {
                return payload;
            }
            ready.await;
        }
    }
}

/// A forwarding turn; dropping it discards an interrupt the turn did not take.
struct LocalTurn<'a>(&'a LocalInterrupt);

impl Drop for LocalTurn<'_> {
    fn drop(&mut self) {
        let mut state = self.0.state();
        state.forwarding = false;
        state.pending = None;
    }
}

/// The acknowledgement current Codex waits for after interrupting a response the proxy
/// itself stops or already finished (docs/DIFFERENCES-FROM-GO.md).
fn interrupt_accepted(response_id: &str) -> String {
    set_str(r#"{"type":"response.interrupt.accepted"}"#, "response_id", response_id)
}

/// What the reader needs for `response.interrupt` (Go
/// `forwardResponsesWebsocketInterrupt` and the reader's control-frame branch).
struct Control {
    rt: Arc<Runtime>,
    session: String,
    home: Arc<crate::home_session::SessionHome>,
    local: Arc<LocalInterrupt>,
    writer: Writer,
    timeline: Timeline,
}

impl Control {
    /// `None` when `frame` is not a `response.interrupt` (valid JSON, string `type`);
    /// otherwise its outcome, `Err` with the write error when its rejection cannot be
    /// written. The frame itself bypasses request normalization and the timeline's
    /// request parts, and goes upstream as received: Go's `json.Valid` accepts any
    /// bytes inside a string, so a binary frame need not be UTF-8.
    async fn interrupt(&self, frame: &[u8]) -> Option<Result<(), String>> {
        if !cpa_common::json::std_valid(frame) {
            return None;
        }
        if &*cpa_common::json::get(frame, "type").bytes() != b"response.interrupt" {
            return None;
        }
        self.diagnostic(frame, "");
        let outcome = match self.forward(frame).await {
            Ok(None) => "handled",
            // Upstream acknowledged an interrupt for a response that had already ended.
            Ok(Some(ack)) => {
                // Upstream's text frame, so valid UTF-8: passed on unchanged.
                let ack = String::from_utf8_lossy(&ack).into_owned();
                let sent = self.writer.lock().await.send(Message::Text(ack.into())).await;
                if let Err(error) = sent {
                    return Some(Err(error.to_string()));
                }
                "handled"
            }
            Err(InterruptError::NoActive) => match self.local.deliver(&String::from_utf8_lossy(frame)) {
                Local::Delivered => "local_http",
                Local::Finished => {
                    let ack = interrupt_accepted(&field(&String::from_utf8_lossy(frame), "response_id"));
                    let sent = self.writer.lock().await.send(Message::Text(ack.into())).await;
                    if let Err(error) = sent {
                        return Some(Err(error.to_string()));
                    }
                    "handled"
                }
                Local::Refused => return Some(self.reject(frame, InterruptError::NoActive).await),
            },
            Err(error) => return Some(self.reject(frame, error).await),
        };
        self.diagnostic(frame, outcome);
        Some(Ok(()))
    }

    /// The error event for a rejected interrupt; `Err` when it cannot be written.
    async fn reject(&self, frame: &[u8], error: InterruptError) -> Result<(), String> {
        self.diagnostic(frame, "rejected");
        // Only the sanitized outcome is logged: transport errors may carry credentials or
        // URLs. The error event is not a timeline part.
        let event = error_payload(400, &error.to_string(), None);
        let sent = self.writer.lock().await.send(Message::Text(event.into())).await;
        sent.map_err(|e| e.to_string())
    }

    async fn forward(&self, payload: &[u8]) -> Result<Option<Bytes>, InterruptError> {
        let id = cpa_common::json::get(payload, "response_id");
        if id.kind != cpa_common::json::Kind::String || cpa_common::gostr::trim_space(&id.bytes()).is_empty() {
            return Err(InterruptError::Rejected(
                "response.interrupt requires response_id".into(),
            ));
        }
        // `WithWebsocketAuthCheck`: a Home credential lives on this session only.
        let (rt, home) = (&self.rt, &self.home);
        let enabled = |id: &str| session_auth(rt, home, id).is_some_and(|(credential, _)| !credential.disabled);
        self.rt
            .executors
            .codex
            .interrupt_session(&self.session, payload, enabled)
            .await
    }

    /// `appendResponsesInterruptDiagnostic`: the response ID hashed, every other field
    /// (mode, extensions) left out.
    fn diagnostic(&self, payload: &[u8], outcome: &str) {
        use sha2::Digest;
        let Some(timeline) = self.timeline.as_deref() else {
            return;
        };
        let hash = sha2::Sha256::digest(cpa_common::json::get(payload, "response_id").bytes());
        let hex: String = hash[..8].iter().map(|b| format!("{b:02x}")).collect();
        let mut data = format!(r#"{{"type":"response.interrupt","response_id_hash":"{hex}""#);
        if !outcome.is_empty() {
            data.push_str(&format!(r#","outcome":"{outcome}""#));
        }
        data.push('}');
        let kind = if outcome.is_empty() {
            "interrupt"
        } else {
            "interrupt.outcome"
        };
        timeline_event(Some(timeline), kind, data.as_bytes(), &Default::default());
    }
}

/// Go `sessionAuthByIDWithSource`: the store's current view first, then a Home
/// credential the session kept (with its availability snapshot).
fn session_auth(
    rt: &Runtime,
    home: &crate::home_session::SessionHome,
    id: &str,
) -> Option<(Arc<Credential>, Option<Arc<crate::remote::Availability>>)> {
    if id.is_empty() {
        return None;
    }
    match rt.store().get(id) {
        Some(credential) => Some((credential, None)),
        None => home
            .auth(id)
            .map(|(credential, availability)| (credential, Some(availability))),
    }
}

/// A turn's stream once its first event arrived.
struct Started {
    first: Option<Bytes>,
    stream: ExecStream,
    /// The serving attempt's upstream response headers: a Codex socket's handshake when
    /// the attempt opened one, empty when it reused one.
    headers: HeaderMap,
}

/// Per-turn facts the attempt and forward steps share.
struct TurnCtx {
    native: bool,
    requires_current: bool,
    native_request: bool,
    pinned_attempted: bool,
    last_attempted: String,
    attempted_mode: Mode,
    preserve_output: bool,
    /// The serving credential is a Codex credential on an upstream socket.
    codex_socket: bool,
    /// The attempt that answered ran on the native WebSocket route (#24): its events are
    /// the upstream's frames, relayed as they are.
    native_served: bool,
    tool_turn: Option<TurnCache>,
}

impl TurnCtx {
    /// `replayPinnedAuthFailure`: a continuation cannot rotate credentials in place.
    fn suppress(&self, status: u16) -> bool {
        self.native && self.requires_current && self.pinned_attempted && matches!(status, 401 | 429)
    }
}

enum Forwarded {
    Completed {
        output: String,
        id: String,
        pending: Vec<String>,
    },
    /// A pinned continuation's 401/429: [`Connection::turn`] decides between the replay
    /// close and the final limit.
    Suppressed(Failure),
    End,
}

impl Connection {
    fn new(rt: Arc<Runtime>, caller: Caller, headers: HeaderMap) -> Self {
        Self {
            tool_key: tools::session_key(&headers),
            codex_client: cpa_common::codex_client::is_codex_responses_client(&headers),
            rt,
            caller,
            headers,
            request_path: String::new(),
            peer: None,
            session: uuid::Uuid::new_v4().to_string(),
            last_request: String::new(),
            last_output: "[]".into(),
            last_id: String::new(),
            pending_prewarm: String::new(),
            pending_calls: Vec::new(),
            pinned: String::new(),
            pinned_by_provider: HashMap::new(),
            passthrough_model: String::new(),
            mode: Mode::Unknown,
            upstream_auth: String::new(),
            steering: false,
            duplex: Arc::default(),
            replays: Arc::default(),
            serving: Arc::default(),
            losses: None,
            capture: Default::default(),
            request_id: None,
            timeline: None,
            home: Arc::default(),
            local: Arc::default(),
        }
    }

    async fn run(mut self, socket: WebSocket) {
        let _tools = Retained::new(self.tool_key.clone());
        let (inner, stream) = socket.split();
        let mut sink = Sink {
            inner: Arc::new(tokio::sync::Mutex::new(inner)),
            timeline: self.timeline.take().map(Arc::new),
            terminated: None,
        };
        let mut client = Client::spawn(
            stream,
            Control {
                rt: self.rt.clone(),
                session: self.session.clone(),
                home: self.home.clone(),
                local: self.local.clone(),
                writer: sink.inner.clone(),
                timeline: sink.timeline.clone(),
            },
        );
        if CodexExecutor::response_steering_configured(&self.rt.config()) {
            let store = self.rt.store().clone();
            // `WithWebsocketAuthCheck`. ponytail: Go falls back to the execution session's
            // snapshot of a credential removed since; a missing credential counts as enabled.
            let enabled = move |id: &str| store.get(id).is_none_or(|c| !c.disabled);
            self.rt
                .executors
                .codex
                .attach_steering(&self.session, SteeringInput::new(client.frames.clone(), enabled));
            self.steering = true;
        }
        let (duplex, replays, serving) = (self.duplex.clone(), self.replays.clone(), self.serving.clone());
        // Each socket's loss fires once. One a duplex or replaying turn owns, or one of a
        // socket a bootstrapping turn already gave up, is taken and never fires again; a
        // later socket's loss still does.
        let mut lost = self.rt.executors.session_losses(&self.session);
        self.losses = Some(lost.codex().clone());
        loop {
            let frame = tokio::select! {
                frame = client.next() => frame,
                loss = lost.next() => {
                    close_for_upstream_loss(&mut sink, &loss.error, self.codex_client).await;
                    break;
                }
            };
            let Some(frame) = frame else {
                // Go's read error ends the connection.
                if let Some(reason) = client.close_text() {
                    sink.terminate(&reason);
                }
                break;
            };
            sink.event("request", &frame);
            // The turn wins ties so frames already received reach the client before a
            // simultaneous upstream-loss signal closes the connection. A duplex turn owns
            // closure: it drains acknowledgements and pending events in order first. A
            // client that went away cancels the turn (Go's reader cancels the request).
            let flow = {
                let turn = self.turn(&mut sink, frame);
                tokio::pin!(turn);
                tokio::select! {
                    biased;
                    flow = &mut turn => Ok(flow),
                    loss = lost.next() => {
                        let error = loss.error;
                        // A pinned continuation's own 401/429 lost the socket: the turn
                        // sends the replay close (1012), or the final limit, instead.
                        let replay = replays.load(Ordering::Acquire) && matches!(error.status, 401 | 429);
                        // Before an attempt served the request, the loss is of a socket the
                        // turn's attempt already failed on: the turn fails over to another
                        // credential or delivers that failure itself. Ending it here would
                        // drop an unserved request mid-failover (#25).
                        let bootstrapping = !serving.load(Ordering::Acquire);
                        // A socket the current attempt did not acquire (a retained socket
                        // while the turn runs over HTTP or another provider) serves nothing
                        // of this turn: its loss does not end it (#25 R3).
                        let elsewhere = !loss.acquired;
                        if duplex.load(Ordering::Acquire) || replay || bootstrapping || elsewhere {
                            tokio::select! {
                                biased;
                                flow = &mut turn => Ok(flow),
                                () = client.gone() => Err(None),
                            }
                        } else {
                            Err(Some(error))
                        }
                    }
                    () = client.gone() => Err(None),
                }
            };
            // A finished turn wakes idle work such as the heap trim (one atomic load).
            cpa_common::idle::activity();
            match flow {
                Ok(Flow::Next) => {}
                Ok(Flow::End) => break,
                Err(Some(error)) => {
                    close_for_upstream_loss(&mut sink, &error, self.codex_client).await;
                    break;
                }
                // The client left mid-turn: Go's forward returns the cancelled context.
                Err(None) => {
                    sink.terminate("context canceled");
                    break;
                }
            }
        }
        // The reader holds the other half: stop it so the socket closes.
        client.stop();
        self.rt.executors.close_session(&self.session);
        // The kept Home pick ends with the connection, before timeline delivery can wait.
        self.home.close();
        sink.finish(client).await;
    }

    /// One client frame. `frame` keeps the client's bytes for a native attempt (#24),
    /// which reads them through the request boundary; the fallback path reads a lossy
    /// UTF-8 copy, as before.
    async fn turn(&mut self, socket: &mut Sink, frame: Bytes) -> Flow {
        let payload = String::from_utf8_lossy(&frame).into_owned();
        let cfg = self.rt.config();
        let explicit_model = field(&payload, "model");
        let mut request_model = explicit_model.clone();
        if request_model.is_empty() {
            request_model.clone_from(&self.passthrough_model);
        }
        if request_model.is_empty() {
            request_model = field(&self.last_request, "model");
        }
        self.revalidate_pin(&request_model);

        let mut use_upstream_ws = self.uses_upstream_websocket(&request_model);
        if let Some(pinned) = self.credential(&self.pinned)
            && CodexExecutor::upstream_websocket(&pinned)
        {
            use_upstream_ws = matches!(pinned.provider.to_ascii_lowercase().as_str(), "codex" | "xai");
        }
        let native = self.mode == Mode::Websocket
            && use_upstream_ws
            && !self.pinned.is_empty()
            && self.pinned == self.upstream_auth;
        let previous_id = field(&payload, "previous_response_id");
        let requires_current = !previous_id.is_empty() || field(&payload, "type") == APPEND;
        if self.mode == Mode::Websocket && !native && requires_current {
            // A continuation of upstream state cannot move to another transport.
            close_with_code(socket, CLOSE_SERVICE_RESTART, "upstream requires HTTP replay").await;
            socket.terminate(&replay_text());
            return Flow::End;
        }
        if !explicit_model.is_empty() && !use_upstream_ws {
            self.passthrough_model.clear();
        }
        // ponytail: Go also tracks observed compaction per route/plugin; with only Codex
        // credentials on this route the pinned/all-Codex rule below gives the same answer.
        let allow_bypass = !native
            && match self.credential(&self.pinned) {
                Some(pinned) => pinned.provider.eq_ignore_ascii_case("codex"),
                None => {
                    let available = self.available(&request_model);
                    !available.is_empty() && available.iter().all(|c| c.provider.eq_ignore_ascii_case("codex"))
                }
            };

        let is_prewarm = !use_upstream_ws && requests::is_local_prewarm(&payload);
        let home_unknown = self.rt.remote_dispatch().is_some() && self.credential(&self.pinned).is_none();
        if is_prewarm && (home_unknown || self.codex_may_serve(&request_model)) {
            // Go answers this prewarm locally with a made-up response. A Codex credential
            // without an upstream socket has no provider prewarm to forward, and a Codex
            // client must never receive output no provider produced (#24). Under Home,
            // before the first pick, the serving credential and its transport are unknown,
            // so the prewarm cannot be forwarded or answered either. The request fails;
            // the connection stays open.
            let message = if home_unknown {
                HOME_PREWARM_UNSUPPORTED
            } else {
                CODEX_PREWARM_UNSUPPORTED
            };
            let frame = error_payload(400, message, None);
            return match send_text(socket, frame).await {
                true => Flow::Next,
                false => Flow::End,
            };
        }
        let kind = field(&payload, "type");
        let replacement = |payload: &str, last: &str| {
            let input = rawjson::get(payload, "input");
            if input.exists() && input.kind() != rawjson::Kind::Array {
                return Err(WsError::bad("websocket request requires array field: input"));
            }
            requests::normalize_create(&requests::transcript_replacement(payload, last))
        };
        let normalized = if !self.pending_prewarm.is_empty() && !previous_id.is_empty() {
            if previous_id != self.pending_prewarm {
                Err(WsError::previous_not_found())
            } else {
                requests::prewarm_followup(&payload, &self.last_request)
            }
        } else if (is_prewarm && previous_id.is_empty()) || (!self.pending_prewarm.is_empty() && kind == CREATE) {
            replacement(&payload, &self.last_request)
        } else if native {
            requests::normalize_passthrough(&payload, &request_model).map(|r| (r, String::new()))
        } else if self.last_request.is_empty() && !previous_id.is_empty() {
            Err(WsError::previous_not_found())
        } else {
            requests::normalize(
                &payload,
                &self.last_request,
                &self.last_output,
                &self.last_id,
                &self.pending_calls,
                false,
                allow_bypass,
            )
        };
        let (mut request, updated, fallback_error) = match normalized {
            Ok((request, updated)) => (request, updated, None),
            // A fallback-only refusal (such as a string `input`, which only the HTTP
            // fallback cannot convert) cannot decide a native attempt: the request
            // boundary judges the client's frame there, and the provider the rest
            // (`docs/codex/CONTRACT.md` "Native WebSocket route"). Any attempt that is not
            // native still ends with this error, before anything is sent. A missing
            // previous response is the connection's state, not the frame's: it stays local.
            Err(error) if kind == CREATE && !is_prewarm && error.status != 409 => {
                (payload.clone(), self.last_request.clone(), Some(error))
            }
            Err(error) => {
                // Request-shape errors keep the connection open.
                let frame = error_payload(error.status, &error.message, None);
                return match send_text(socket, frame).await {
                    true => Flow::Next,
                    false => Flow::End,
                };
            }
        };
        // Go prepares multi-agent v2 tools and orphan delegation outputs here, before the
        // prewarm and dispatch decisions.
        let client = cpa_common::codex_client::Settings::from_config(&cfg);
        // The spawn_agent model list comes from the Codex client catalog.
        // Cost: one header check, then one atomic load, per turn.
        if cpa_common::codex_client::multi_agent_client(&self.headers, client.optimize_multi_agent_v2) {
            crate::model_updater::codex_client_catalog_wanted();
        }
        if fallback_error.is_none() && (client.optimize_multi_agent_v2 || client.orphan_delegation) {
            let prepared =
                cpa_common::codex_client::prepare_responses_request(&self.headers, request.as_bytes(), &client);
            request = String::from_utf8(prepared).unwrap_or(request);
        }

        if is_prewarm {
            request = delete(&request, "generate");
            self.last_request = delete(&updated, "generate");
            self.last_output = "[]".into();
            self.last_id.clear();
            self.pending_calls.clear();
            let id = format!("resp_prewarm_{}", uuid::Uuid::new_v4());
            let created_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64);
            for frame in requests::prewarm_payloads(&request, &id, created_at) {
                if !send_text(socket, frame).await {
                    return Flow::End;
                }
            }
            self.pending_prewarm = id;
            return Flow::Next;
        }

        let mut ctx = TurnCtx {
            native,
            requires_current,
            native_request: lite_request(&payload, &self.headers),
            pinned_attempted: false,
            last_attempted: self.pinned.clone(),
            attempted_mode: Mode::Http,
            preserve_output: false,
            codex_socket: false,
            native_served: false,
            tool_turn: None,
        };
        let next_last_request = if native || fallback_error.is_some() {
            let model = field(&request, "model");
            if !model.is_empty() {
                self.passthrough_model = model;
            }
            self.last_request.clone()
        } else {
            let (repaired, tool_turn) = tools::prepare_fallback_turn(&self.tool_key, request);
            request = repaired;
            ctx.tool_turn = tool_turn;
            request.clone()
        };
        let model = rawjson::get(&request, "model").str().to_owned();
        let started = self
            .attempt(&frame, &request, &model, fallback_error.as_ref(), &mut ctx)
            .await;
        let forwarded = match started {
            Ok(started) => self.forward(socket, started, &cfg, &mut ctx).await,
            // No native attempt served the frame: the fallback's refusal stands, as an
            // error frame on a connection that stays open.
            Err(_)
                if !ctx.native_served
                    && let Some(error) = &fallback_error =>
            {
                let frame = error_payload(error.status, &error.message, None);
                return match send_text(socket, frame).await {
                    true => Flow::Next,
                    false => Flow::End,
                };
            }
            Err(error) => {
                let failure = Failure::from_dispatch(&error);
                // A pinned continuation's credential failure also lost the session's
                // socket; the client still needs the replay signal, not the loss close.
                if ctx.suppress(failure.status) {
                    Forwarded::Suppressed(failure)
                } else if let Some(loss) = self.upstream_loss() {
                    close_for_upstream_loss(socket, &loss, self.codex_client).await;
                    Forwarded::End
                } else {
                    close_for_failure(socket, &failure, self.codex_client).await;
                    Forwarded::End
                }
            }
        };
        let (output, id, pending) = match forwarded {
            Forwarded::Completed { output, id, pending } => (output, id, pending),
            Forwarded::Suppressed(failure) if self.limit_is_final(&failure, &ctx.last_attempted, &model) => {
                close_for_failure(socket, &failure, true).await;
                return Flow::End;
            }
            Forwarded::Suppressed(_) => {
                close_with_code(socket, CLOSE_SERVICE_RESTART, "upstream requires HTTP replay").await;
                socket.terminate(&replay_text());
                return Flow::End;
            }
            Forwarded::End => return Flow::End,
        };

        if let Some(tool_turn) = ctx.tool_turn.take() {
            tool_turn.commit();
        }
        self.pending_prewarm.clear();
        self.mode = ctx.attempted_mode;
        if self.mode == Mode::Websocket {
            self.upstream_auth.clone_from(&ctx.last_attempted);
            if !ctx.last_attempted.is_empty() {
                self.remember_pin(&ctx.last_attempted, &model);
            }
            self.passthrough_model = model;
            self.last_request.clear();
            self.last_output = "[]".into();
            self.last_id.clear();
            self.pending_calls.clear();
        } else {
            self.upstream_auth.clear();
            self.last_request = next_last_request;
            self.last_output = output;
            self.last_id = id.trim().to_owned();
            self.pending_calls = pending;
        }
        Flow::Next
    }

    /// Whether a Codex credential may serve a turn for `model`: the pinned credential is
    /// one, or nothing is pinned and the model's pool holds one.
    fn codex_may_serve(&self, model: &str) -> bool {
        let codex = |c: &Credential| c.provider.trim().eq_ignore_ascii_case("codex");
        match self.credential(&self.pinned) {
            Some(pinned) => codex(&pinned),
            None => self.available(model).iter().any(|c| codex(c)),
        }
    }

    fn credential(&self, id: &str) -> Option<Arc<Credential>> {
        self.session_credential(id).map(|(credential, _)| credential)
    }

    /// Go `sessionAuthByIDWithSource`: the store's current view first, then a Home
    /// credential this session kept (the flag).
    fn session_credential(&self, id: &str) -> Option<(Arc<Credential>, Option<Arc<crate::remote::Availability>>)> {
        session_auth(&self.rt, &self.home, id)
    }

    /// `responsesWebsocketProviderSetForModel`: providers that registered the model
    /// (`auto` resolved, thinking suffix stripped) and the model key.
    fn providers(&self, model: &str) -> (Vec<String>, String) {
        let registry = self.rt.registry();
        let resolved = resolve_auto(&self.rt, &registry, model);
        // Go trims the base after splitting off the suffix: `gpt-5(high) ` has no suffix.
        let key = cpa_common::thinking::parse_suffix(&resolved)
            .model_name
            .trim()
            .to_owned();
        let mut providers = registry.providers(&key);
        if providers.is_empty() && key != resolved {
            providers = registry.providers(&resolved);
        }
        let key = if key.is_empty() {
            resolved.trim().to_owned()
        } else {
            key
        };
        (providers, key)
    }

    /// Go's disconnect notifier closes the client as soon as the session's upstream socket
    /// is lost, ahead of the turn's own error (whose 1009 body is a fixed message rather
    /// than the upstream's close reason). A duplex stream owns its closure instead.
    ///
    /// The loss replaces the failure only when it is the loss of the socket the turn's
    /// current attempt acquired and the connection did not take it already
    /// ([`cpa_exec::codex::SessionLosses::pending`]). An attempt that failed before it
    /// served on a socket (over HTTP, on another provider, or refused while it prepared
    /// its request) fails with its own error, whatever an earlier attempt's socket
    /// reported (#25 R3).
    fn upstream_loss(&self) -> Option<ExecError> {
        if self.duplex.load(Ordering::Acquire) {
            return None;
        }
        self.losses.as_ref()?.pending()
    }

    /// A pinned continuation reached a subscription limit and no other credential can
    /// take the conversation over: a native Codex client gets the limit instead of the
    /// replay close, which would only send the whole transcript back to exhausted
    /// accounts. The continuation itself never moves to another credential.
    fn limit_is_final(&self, failure: &Failure, pinned: &str, model: &str) -> bool {
        self.codex_client && failure.limit.is_some() && self.available(model).iter().all(|c| c.id == pinned)
    }

    /// `responsesWebsocketAuthAvailableForModel`: enabled and not cooling for the model.
    fn usable(&self, credential: &Credential, key: &str) -> bool {
        !credential.disabled && !self.rt.store().blocked(credential, key)
    }

    /// `responsesWebsocketAvailableAuthsForModel`.
    fn available(&self, model: &str) -> Vec<Arc<Credential>> {
        let (providers, key) = self.providers(model);
        if providers.is_empty() {
            return Vec::new();
        }
        let registry = self.rt.registry();
        self.rt
            .store()
            .snapshot()
            .into_iter()
            .filter(|c| {
                providers.contains(&provider_key(c))
                    && (key.is_empty() || registry.client_supports(&c.id, &key))
                    && self.usable(c, &key)
            })
            .collect()
    }

    /// `responsesWebsocketUsesUpstreamWebsocketPassthrough`: every credential for the
    /// model keeps upstream state on a socket.
    fn uses_upstream_websocket(&self, model: &str) -> bool {
        if model.trim().is_empty() {
            return false;
        }
        let available = self.available(model);
        !available.is_empty() && available.iter().all(|c| self.rt.executors.session_upstream(c))
    }

    /// `responsesWebsocketPinnedAuthMatchesModel`. A Home credential (`home` holds the
    /// model key it was pinned for) is in no local registry or scheduler: its snapshot
    /// decides availability, and it matches while pinned for the same model key.
    /// ponytail: provider membership reads `provider_key`, which honours a `provider_key`
    /// attribute; Go lower-cases `auth.Provider`. They agree for Codex and xAI snapshots.
    fn pin_matches(
        &self,
        credential: &Credential,
        model: &str,
        home: Option<(&str, &crate::remote::Availability)>,
    ) -> bool {
        let (providers, key) = self.providers(model);
        match home {
            // A Home pick: Home's pool is remote, so the local registry need not list its
            // provider or model. The pick itself proves the route: it is still kept (the
            // credential resolves only while its lease lives), it was granted for this
            // model (`pinned_key`), and Home's availability snapshot still allows it.
            Some((pinned_key, snapshot)) => {
                crate::remote::available_for_model(credential, snapshot, &key, SystemTime::now())
                    && pinned_key.trim().go_eq_fold(key.trim())
            }
            None => {
                providers.contains(&provider_key(credential))
                    && self.usable(credential, &key)
                    && self.rt.registry().client_supports(&credential.id, &key)
            }
        }
    }

    /// Drops a pin whose credential no longer serves the model, or restores the pin
    /// remembered for the model's only provider.
    fn revalidate_pin(&mut self, model: &str) {
        let keep = self.session_credential(&self.pinned).is_some_and(|(pinned, home)| {
            self.pinned_by_provider
                .get(&provider_key(&pinned))
                .is_some_and(|(id, key)| {
                    *id == self.pinned && self.pin_matches(&pinned, model, home.as_deref().map(|a| (key.as_str(), a)))
                })
        });
        if !keep {
            self.pinned.clear();
        }
        if !self.pinned.is_empty() {
            return;
        }
        if let [provider] = self.providers(model).0.as_slice() {
            let remembered = self.pinned_by_provider.get(provider).cloned();
            match remembered.and_then(|(id, key)| self.session_credential(&id).map(|c| (id, key, c))) {
                Some((id, key, (c, home)))
                    if self.pin_matches(&c, model, home.as_deref().map(|a| (key.as_str(), a))) =>
                {
                    self.pinned = id
                }
                _ => {
                    self.pinned_by_provider.remove(provider);
                }
            }
        }
    }

    /// `rememberPinnedAuth`.
    fn remember_pin(&mut self, id: &str, model: &str) {
        let Some(credential) = self.credential(id) else {
            return;
        };
        self.pinned = id.to_owned();
        let provider = provider_key(&credential);
        if !provider.is_empty() {
            let key = self.providers(model).1;
            self.pinned_by_provider.insert(provider, (id.to_owned(), key));
        }
    }

    /// One turn through the shared dispatch loop (`ExecuteStreamWithAuthManager` with the
    /// pinned auth, the selected-auth callback and the execution session), including
    /// bootstrap retries. Returns once the stream yielded its first event. `frame` is the
    /// client's frame as it arrived: a native attempt (#24) sends it instead of `request`,
    /// the frame this connection prepared for the legacy path.
    async fn attempt(
        &self,
        frame: &Bytes,
        request: &str,
        model: &str,
        fallback_error: Option<&WsError>,
        ctx: &mut TurnCtx,
    ) -> Result<Started, dispatch::Failure> {
        /// What `WithSelectedAuthIDCallback` learned; the last call is the serving one.
        struct Selected {
            last: String,
            pinned_attempted: bool,
            mode: Mode,
            preserve_output: bool,
            codex_socket: bool,
        }
        let selected = Arc::new(std::sync::Mutex::new(Selected {
            last: ctx.last_attempted.clone(),
            pinned_attempted: false,
            mode: ctx.attempted_mode,
            preserve_output: ctx.preserve_output,
            codex_socket: ctx.codex_socket,
        }));
        self.duplex.store(false, Ordering::Release);
        self.replays.store(false, Ordering::Release);
        self.serving.store(false, Ordering::Release);
        let on_selected: dispatch::OnSelected = {
            let (selected, rt, pinned) = (selected.clone(), self.rt.clone(), self.pinned.clone());
            let session = self.session.clone();
            let native_request = ctx.native_request;
            let continuation = ctx.native && ctx.requires_current;
            let (duplex, replays, steering) = (self.duplex.clone(), self.replays.clone(), self.steering);
            Box::new(move |credential: &Credential| {
                let mut s = selected.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                // A new attempt owns no socket until it acquires one, and a socket an
                // earlier attempt already lost reports nothing more (#25 R3).
                rt.executors.session_attempt(&session);
                s.last.clone_from(&credential.id);
                s.pinned_attempted |= !pinned.is_empty() && credential.id == pinned;
                replays.store(continuation && s.pinned_attempted, Ordering::Release);
                s.mode = if rt.executors.session_upstream(credential) {
                    Mode::Websocket
                } else {
                    Mode::Http
                };
                let codex = credential.provider.eq_ignore_ascii_case("codex");
                s.preserve_output = native_request && codex;
                s.codex_socket = codex && s.mode == Mode::Websocket;
                // Shared since v8.0.35: API-key credentials steer too.
                let full_duplex = steering && codex && s.mode == Mode::Websocket;
                duplex.store(full_duplex, Ordering::Release);
            })
        };
        let turn = Arc::new(dispatch::SessionTurn {
            session: ExecSession {
                id: self.session.clone(),
                continuation: ctx.native && ctx.requires_current,
                lease: None,
            },
            pinned: (!self.pinned.is_empty()).then(|| self.pinned.clone()),
            on_selected: Some(on_selected),
            home: Some(self.home.clone()),
        });
        let native = dispatch::NativeRequest::new(frame.clone(), self.headers.clone(), self.request_path.clone())
            .with_fallback_error(fallback_error.map(|e| (e.status, e.message.clone())));
        let call = dispatch::Call {
            entry: Format::OpenAIResponse,
            response: Format::OpenAIResponse,
            operation: Operation::Generate,
            model: model.to_owned(),
            body: Bytes::from(request.to_owned()),
            stream: true,
            alt: None,
            headers: self.headers.clone(),
            caller: self.caller.clone(),
            forced_provider: None,
            selection_model: None,
            execution_session: Some(self.session.clone()),
            request_path: self.request_path.clone(),
            peer: self.peer,
            native: Some(native.clone()),
            turn: Some(turn),
            media: None,
        };
        // The upgrade's log captures every turn's upstream traffic under its request ID.
        let trace = dispatch::Trace::with_request_id(self.request_id.clone()).with_capture(self.capture.clone());
        let result = dispatch::run_with_bootstrap_retries(&self.rt, call, &trace).await;
        {
            let s = selected.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            ctx.last_attempted.clone_from(&s.last);
            ctx.pinned_attempted = s.pinned_attempted;
            ctx.attempted_mode = s.mode;
            ctx.native_served = native.served();
            ctx.preserve_output = s.preserve_output || ctx.native_served;
            ctx.codex_socket = s.codex_socket;
        }
        let done = result?;
        self.serving.store(true, Ordering::Release);
        match done {
            dispatch::Done::Stream { headers, first, rest } => Ok(Started {
                first,
                stream: rest,
                headers,
            }),
            dispatch::Done::Buffered { headers, body } => Ok(Started {
                first: Some(body),
                stream: futures_util::stream::empty().boxed(),
                headers,
            }),
        }
    }

    /// `forwardResponsesWebsocket`.
    async fn forward(&self, socket: &mut Sink, started: Started, cfg: &Config, ctx: &mut TurnCtx) -> Forwarded {
        let Started { first, stream, headers } = started;
        // Metadata from the handshake of the socket serving this turn, ahead of its first
        // event. The executor returns turn state only to the turn that opened the socket;
        // a later turn gets that socket's `openai-model` alone, or nothing.
        let mut metadata = ctx
            .codex_socket
            .then(|| cpa_exec::codex::handshake_metadata_event(&headers))
            .flatten();
        // `stream` completes its lease as it ends (dispatch).
        let mut stream = futures_util::stream::iter(first.map(Ok))
            .chain(stream)
            .boxed()
            .peekable();
        let keepalive = keepalive_interval(cfg);
        let mut deadline = keepalive.map(|k| tokio::time::Instant::now() + k);
        let mut turn = Turn::default();
        let (mut completed, mut completed_output, mut completed_id) = (false, String::from("[]"), String::new());
        // A duplex stream takes the client's later frames from the shared queue and owns
        // the connection's end.
        let duplex = self.duplex.load(Ordering::Acquire);
        let mut duplex_established = false;
        // The last payload sent was an upstream context-window `response.failed`, which
        // the executor follows with its own classified error (`preserved_failure`).
        let mut preserved_event = false;
        let _local = self.local.begin();
        loop {
            let item = tokio::select! {
                biased;
                // Dropping the stream cancels the upstream request. A turn that already
                // gave its client a context-window `response.failed` has its terminal
                // event; an interrupt cannot add a second one.
                payload = self.local.next(), if !preserved_event => {
                    if !completed {
                        return interrupted(socket, &self.local, &payload, &turn).await;
                    }
                    // The terminal event already went out: only the acknowledgement is left.
                    if !send_text(socket, interrupt_accepted(&field(&payload, "response_id"))).await {
                        return Forwarded::End;
                    }
                    continue;
                }
                item = stream.next() => item,
                () = sleep_until(deadline) => {
                    if let Err(error) = socket.send(Message::Ping(Bytes::new())).await {
                        socket.terminate(&error.to_string());
                        return Forwarded::End;
                    }
                    deadline = keepalive.map(|k| tokio::time::Instant::now() + k);
                    continue;
                }
            };
            let chunk = match item {
                // A duplex stream ends with its socket, not with a response: close the
                // connection without an error.
                None if duplex => {
                    socket.terminate(CLOSE_SENT);
                    return Forwarded::End;
                }
                None if completed => {
                    return Forwarded::Completed {
                        output: completed_output,
                        id: completed_id,
                        pending: turn.pending(),
                    };
                }
                // `stream closed before response.completed`: 408, closed silently.
                // Closed without an error event or a timeline reason.
                None => {
                    socket.terminate(CLOSE_SENT);
                    return Forwarded::End;
                }
                Some(Err(error)) => return self.failed(socket, ctx, &error, preserved_event).await,
                Some(Ok(chunk)) => chunk,
            };
            deadline = keepalive.map(|k| tokio::time::Instant::now() + k);
            // A native turn's chunk is one upstream frame, relayed as it arrived.
            let payloads = if ctx.native_served {
                let frame = String::from_utf8_lossy(&chunk).into_owned();
                if frame.trim().is_empty() {
                    Vec::new()
                } else {
                    vec![frame]
                }
            } else {
                payloads_from_chunk(&chunk)
            };
            for mut payload in payloads {
                // A native turn's executor already decided what each frame does: whether
                // the response ends on it and whether it fails the turn (`codex_ws`). A
                // decoded event ends the turn here only when the pinned WebSocket reader
                // ends it (`cpa_exec::codex_event::Outcome::ends_turn`).
                let mut native_terminal = None;
                // A decoded native event's `response.id` as the pinned `Value` reads it
                // (the last of a repeated `id`), the response the executor settles.
                let mut native_id = None;
                let mut native_context_failure = false;
                let kind = if ctx.native_served {
                    // The upstream event boundary (`cpa_exec::codex_event`): a frame the
                    // pinned client skips is relayed as sent, and nothing here reads it:
                    // no type, completion, failure or tool call (#25).
                    match cpa_exec::codex_event::decode(&payload) {
                        Some(event) => {
                            native_terminal = Some(event.outcome().ends_turn());
                            native_id = Some(event.response_id().unwrap_or_default());
                            native_context_failure = event.is_context_window_failure();
                            event.kind.into_owned()
                        }
                        None => {
                            preserved_event = false;
                            if let Some(metadata) = metadata.take()
                                && !send_text(socket, metadata).await
                            {
                                return Forwarded::End;
                            }
                            if !send_text(socket, payload).await {
                                return Forwarded::End;
                            }
                            continue;
                        }
                    }
                } else {
                    rawjson::get(&payload, "type").str().to_owned()
                };
                if kind == "response.created" {
                    duplex_established = duplex;
                    completed = false;
                    turn.reset();
                }
                turn.collect(&payload);
                let completion = kind == "response.completed" || kind == "response.done";
                if completion && !ctx.preserve_output {
                    payload = turn.restore_completion(payload);
                }
                match ctx.tool_turn.as_mut() {
                    Some(tool_turn) => tool_turn.record_response(&payload),
                    None => tools::record_calls(&self.tool_key, &payload),
                }
                turn.track_pending(&payload);
                // Once the duplex's first response starts or ends, an error event is
                // recoverable: the client may correct the request on this socket.
                // Stream errors still close it. A native turn's failures arrive as the
                // stream's error, so an `error` event it relays is one the pinned reader
                // ignores (a wrapped notice) or a recoverable steering failure.
                if kind == "error" && !duplex_established && !ctx.native_served {
                    // `responsesWebsocketErrorMessageFromPayload`.
                    let mut status = rawjson::get(&payload, "status").i64();
                    if status <= 0 {
                        status = rawjson::get(&payload, "status_code").i64();
                    }
                    let status = u16::try_from(status).ok().filter(|s| *s > 0).unwrap_or(500);
                    let failure = Failure::from_event(status, payload.trim());
                    if ctx.suppress(status) {
                        return Forwarded::Suppressed(failure);
                    }
                    close_for_failure(socket, &failure, self.codex_client).await;
                    return Forwarded::End;
                }
                // Codex acknowledges `response.interrupt` with an interrupted
                // `response.incomplete`; the connection stays open for the next request.
                if native_terminal.unwrap_or(completion || kind == "response.incomplete") {
                    // The executor also admits later frames after a first terminal
                    // without `response.created` (`Duplex::first_terminal`). Keep the
                    // relay's recoverable-error state in step with it.
                    duplex_established = duplex;
                    completed = true;
                    completed_output = turn.completed_output(&payload);
                    completed_id = native_id.take().unwrap_or_else(|| field(&payload, "response.id"));
                }
                preserved_event = if ctx.native_served {
                    native_context_failure
                } else {
                    cpa_common::codex_client::is_context_window_failure(payload.as_bytes())
                };
                // The executor follows such an event with its classified error at once
                // (cpa-exec `Frame::Failed`): taken before the write, it reaches the
                // scheduler even when the client interrupts or leaves during the write,
                // including the write of handshake metadata ahead of it.
                let committed = if preserved_event {
                    std::pin::Pin::new(&mut stream)
                        .next_if(Result::is_err)
                        .now_or_never()
                        .flatten()
                        .and_then(Result::err)
                } else {
                    None
                };
                if let Some(metadata) = metadata.take()
                    && !send_text(socket, metadata).await
                {
                    return Forwarded::End;
                }
                if !send_text(socket, payload).await {
                    return Forwarded::End;
                }
                if let Some(error) = committed {
                    return self.failed(socket, ctx, &error, true).await;
                }
                if completed {
                    self.local.finish(&completed_id);
                }
            }
        }
    }

    /// The turn's stream failed with `error`; `preserved_event`: the last payload sent was
    /// an upstream context-window `response.failed`.
    async fn failed(&self, socket: &mut Sink, ctx: &TurnCtx, error: &ExecError, preserved_event: bool) -> Forwarded {
        let failure = Failure::from_exec(error);
        // Replay before the session-loss close, as at the turn's start.
        if ctx.suppress(failure.status) {
            return Forwarded::Suppressed(failure);
        }
        // The client already has the upstream's event for this failure (a context-window
        // overflow, which Codex compacts on); a generic error event after it would replace
        // that meaning. Go v8.0.35 differs (docs/DIFFERENCES-FROM-GO.md). Any other error,
        // including one after a recoverable steering failure, closes as before.
        if preserved_event && preserved_failure(&failure) {
            socket.terminate(CLOSE_SENT);
            return Forwarded::End;
        }
        if let Some(loss) = self.upstream_loss() {
            close_for_upstream_loss(socket, &loss, self.codex_client).await;
            return Forwarded::End;
        }
        close_for_failure(socket, &failure, self.codex_client).await;
        Forwarded::End
    }
}

/// `completeResponsesWebsocketLocalInterrupt`: ends the turn in flight with an
/// interrupted `response.incomplete` carrying the output collected so far. The turn
/// completes with that output, so the transcript records it. Current Codex also waits
/// for `response.interrupt.accepted`, which the proxy sends first: it is the one that
/// stops this turn (docs/DIFFERENCES-FROM-GO.md).
async fn interrupted(socket: &mut Sink, local: &LocalInterrupt, payload: &str, turn: &Turn) -> Forwarded {
    let id = field(payload, "response_id");
    if !send_text(socket, interrupt_accepted(&id)).await {
        return Forwarded::End;
    }
    let output = turn.completed_output(r#"{"response":{"output":[]}}"#);
    let incomplete = r#"{"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"interrupted"}}}"#;
    let incomplete = set_raw(&set_str(incomplete, "response.id", &id), "response.output", &output);
    if !send_text(socket, incomplete).await {
        return Forwarded::End;
    }
    local.finish(&id);
    Forwarded::Completed {
        output,
        id,
        pending: turn.pending(),
    }
}

/// Go `getRequestDetails` `auto` resolution, keeping the thinking suffix.
fn resolve_auto(rt: &Runtime, registry: &crate::registry::Registry, model: &str) -> String {
    let base = match model.rfind('(') {
        Some(i) if model.ends_with(')') => &model[..i],
        _ => model,
    };
    if base != "auto" {
        return model.to_owned();
    }
    let first = registry
        .resolve_auto(|client, m| rt.suspension(client, m))
        .unwrap_or_else(|| "auto".into());
    format!("{first}{}", &model[base.len()..])
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// `requests.streaming.keepalive-seconds`: WebSocket pings while a turn streams; off by
/// default.
fn keepalive_interval(cfg: &Config) -> Option<Duration> {
    let seconds = ["requests", "streaming", "keepalive-seconds"]
        .iter()
        .try_fold(&cfg.document, |v, k| v.get(*k))
        .and_then(serde_yaml_ng::Value::as_i64)
        .unwrap_or(0);
    (seconds > 0).then(|| Duration::from_secs(seconds as u64))
}

/// `util.IsCodexResponsesLiteRequest`.
fn lite_request(payload: &str, headers: &HeaderMap) -> bool {
    let header = headers
        .get("x-openai-internal-codex-responses-lite")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if header.trim().eq_ignore_ascii_case("true") {
        return true;
    }
    let value = rawjson::get(
        payload,
        "client_metadata.ws_request_header_x_openai_internal_codex_responses_lite",
    );
    value.kind() == rawjson::Kind::True
        || (value.kind() == rawjson::Kind::String && value.str().trim().eq_ignore_ascii_case("true"))
}

/// `writeResponsesWebsocketPayload`: the frame joins the timeline, then goes out.
async fn send_text(socket: &mut Sink, payload: String) -> bool {
    socket.event("response", payload.as_bytes());
    #[cfg(test)]
    gate::pass("write", &payload).await;
    match socket.send(Message::Text(payload.into())).await {
        Ok(()) => true,
        Err(error) => {
            // ponytail: the write error's text is axum's, not net's.
            socket.terminate(&error.to_string());
            false
        }
    }
}

async fn close_with_code(socket: &mut Sink, code: u16, reason: &str) {
    let reason = requests::truncate_reason(reason, CLOSE_REASON_MAX);
    let close = Message::Close(Some(CloseFrame {
        code,
        reason: reason.into(),
    }));
    let _ = tokio::time::timeout(TERMINAL_WRITE, socket.send(close)).await;
}

/// `websocketClosePayloadForUpstreamError`: replay and message-too-big failures map to
/// close codes the client acts on.
fn close_code(failure: &Failure) -> Option<(u16, String)> {
    if failure.replay {
        return Some((CLOSE_SERVICE_RESTART, "upstream requires HTTP replay".into()));
    }
    // Its close reason would come from the withheld frame.
    if failure.status == 413
        && !failure.withheld
        && rawjson::get(&failure.text, "error.code").str() == "message_too_big"
    {
        // An upstream close keeps gorilla's `CloseError.Text` verbatim; a mapped 413's
        // message is trimmed.
        let raw = rawjson::get(&failure.text, "close_reason");
        let reason = if raw.kind() == rawjson::Kind::String {
            raw.str().to_owned()
        } else {
            rawjson::get(&failure.text, "error.message").str().trim().to_owned()
        };
        let reason = if reason.is_empty() {
            "message too big".into()
        } else {
            reason
        };
        return Some((CLOSE_MESSAGE_TOO_BIG, reason));
    }
    None
}

/// The executor's classified error for an upstream context-window `response.failed`
/// (cpa-exec `terminal_failure`: status 400, `context_too_large`).
fn preserved_failure(failure: &Failure) -> bool {
    failure.status == 400 && rawjson::get(&failure.text, "error.code").str() == "context_too_large"
}

/// `shouldExposeResponsesUpstreamError`: only request faults reach the client; credential,
/// quota and transport failures close silently so the client reconnects and resends. A
/// native Codex client's subscription limit (`Failure::limit`) is sent before this check
/// (docs/DIFFERENCES-FROM-GO.md). A withheld native Codex failure is never exposed: its
/// `text` is the provider's error, which can name the pooled account (#6).
// ponytail: Go also exposes terminal upstream-auth errors; no executor reports them yet.
fn exposed(failure: &Failure) -> bool {
    !failure.withheld && classify::is_request_fault(failure.status, &failure.text)
}

/// `closeForUpstreamError` then `writeResponsesWebsocketTerminalError`: a close code,
/// the error event and a TCP close, or a bare TCP close. The socket is dropped by the
/// caller right after. The timeline gets the exposed event, or the hidden reason as a
/// disconnect, and the connection ends with gorilla's "close sent".
async fn close_for_failure(socket: &mut Sink, failure: &Failure, codex_client: bool) {
    if let Some((code, reason)) = close_code(failure) {
        close_with_code(socket, code, &reason).await;
    } else if let Some(limit) = failure.limit.as_ref().filter(|_| codex_client) {
        socket.event("response", limit.as_bytes());
        let _ = tokio::time::timeout(TERMINAL_WRITE, socket.send(Message::Text(limit.clone().into()))).await;
    } else if let Some(frame) = &failure.delivered {
        // A native Codex turn: the provider's frame, as sent (docs/codex/CONTRACT.md
        // "Native WebSocket route").
        socket.event("response", frame.as_bytes());
        let _ = tokio::time::timeout(TERMINAL_WRITE, socket.send(Message::Text(frame.clone().into()))).await;
    } else if exposed(failure) {
        let payload = failure
            .payload
            .clone()
            .unwrap_or_else(|| error_payload(failure.status, &failure.text, failure.addon.as_deref()));
        socket.event("response", payload.as_bytes());
        let _ = tokio::time::timeout(TERMINAL_WRITE, socket.send(Message::Text(payload.into()))).await;
    } else {
        // Keeps the upstream reason although the client only sees the connection close.
        socket.upstream_event("disconnect", failure.text.as_bytes(), &failure.sent);
    }
    socket.terminate(CLOSE_SENT);
}

/// `closeForUpstreamDisconnect`: the session's upstream socket was lost. Go's notifier
/// writes nothing to the timeline.
// ponytail: Go's final disconnect is whatever its blocked read returns once the notifier
// closed the socket; this logs gorilla's "close sent".
async fn close_for_upstream_loss(socket: &mut Sink, error: &ExecError, codex_client: bool) {
    let failure = Failure::from_exec(error);
    if let Some((code, reason)) = close_code(&failure) {
        close_with_code(socket, code, &reason).await;
    } else if let Some(limit) = failure.limit.as_ref().filter(|_| codex_client) {
        let _ = tokio::time::timeout(TERMINAL_WRITE, socket.send(Message::Text(limit.clone().into()))).await;
    } else if exposed(&failure) {
        let payload = error_payload(failure.status, &failure.text, failure.addon.as_deref());
        let _ = tokio::time::timeout(TERMINAL_WRITE, socket.send(Message::Text(payload.into()))).await;
    }
    socket.terminate(CLOSE_SENT);
}

/// Go's `UpstreamWebsocketReplayRequiredError` text, the reason a replay close ends the
/// connection.
fn replay_text() -> String {
    String::from_utf8_lossy(&ExecError::replay_required().body).into_owned()
}

/// gorilla `CloseError.Error()`.
fn gorilla_close(code: u16, reason: &str) -> String {
    let name = match code {
        1000 => " (normal)",
        1001 => " (going away)",
        1002 => " (protocol error)",
        1003 => " (unsupported data)",
        1005 => " (no status)",
        1006 => " (abnormal closure)",
        1007 => " (invalid payload data)",
        1008 => " (policy violation)",
        1009 => " (message too big)",
        1010 => " (mandatory extension missing)",
        1011 => " (internal server error)",
        1015 => " (TLS handshake error)",
        _ => "",
    };
    let mut text = format!("websocket: close {code}{name}");
    if !reason.is_empty() {
        text.push_str(": ");
        text.push_str(reason);
    }
    text
}

/// Test pauses at named points of a connection, armed for payloads that contain a key,
/// so a test can order events that otherwise race (a client frame during a write).
#[cfg(test)]
pub(crate) mod gate {
    use std::sync::{Arc, Mutex};

    use tokio::sync::Semaphore;

    pub(crate) struct Gate {
        point: &'static str,
        key: String,
        /// One permit each time the point is reached.
        pub(crate) reached: Semaphore,
        /// `pass` waits for a permit here.
        pub(crate) release: Semaphore,
    }

    static GATES: Mutex<Vec<Arc<Gate>>> = Mutex::new(Vec::new());

    /// Arms `point` for payloads containing `key` until [`disarm`]; concurrent tests use
    /// unique keys.
    pub(crate) fn arm(point: &'static str, key: &str) -> Arc<Gate> {
        let gate = Arc::new(Gate {
            point,
            key: key.to_owned(),
            reached: Semaphore::new(0),
            release: Semaphore::new(0),
        });
        GATES.lock().unwrap().push(gate.clone());
        gate
    }

    /// Disarms `gate` and lets every waiter through.
    pub(crate) fn disarm(gate: &Arc<Gate>) {
        GATES.lock().unwrap().retain(|g| !Arc::ptr_eq(g, gate));
        gate.release.add_permits(Semaphore::MAX_PERMITS / 2);
    }

    fn find(point: &str, payload: &str) -> Option<Arc<Gate>> {
        let gates = GATES.lock().unwrap();
        gates
            .iter()
            .find(|g| g.point == point && payload.contains(g.key.as_str()))
            .cloned()
    }

    pub(crate) fn reached(point: &str, payload: &str) {
        if let Some(gate) = find(point, payload) {
            gate.reached.add_permits(1);
        }
    }

    pub(crate) async fn pass(point: &str, payload: &str) {
        if let Some(gate) = find(point, payload) {
            gate.reached.add_permits(1);
            let _ = gate.release.acquire().await.map(tokio::sync::SemaphorePermit::forget);
        }
    }
}

#[cfg(test)]
#[path = "websocket_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "websocket_interrupt_tests.rs"]
mod interrupt_tests;
