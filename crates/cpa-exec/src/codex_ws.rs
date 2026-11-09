//! Codex upstream Responses WebSocket (codex_websockets_executor.go, _connection.go,
//! _session.go, _stream.go, _errors.go).
//!
//! One upstream socket per downstream session, keyed by credential, URL and proxy. A
//! single reader task per socket enforces the 5-minute idle deadline (pongs are answered
//! while reading) and hands frames to the turn in progress through a bounded channel;
//! frames that arrive between turns are dropped, as in Go. Turns on one session are
//! serialised. The socket stays pooled after a completed response so the next turn can
//! continue with `previous_response_id` and incremental input.
//!
//! Interrupts: current Codex waits for upstream's `response.interrupt.accepted` or
//! `.failed` after the interrupted response's terminal event, so a turn that forwarded an
//! interrupt keeps reading for that acknowledgement, at most [`ACK_WAIT`], and only
//! acknowledgements of interrupts this socket forwarded reach a turn
//! (docs/DIFFERENCES-FROM-GO.md).
//!
//! Lifecycle: the reader task ends with the socket; [`Pool::close`] (downstream gone)
//! aborts it and sends a close frame. No task outlives its session.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use crate::rawjson::{self, Kind};
use bytes::Bytes;
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_core::exec::{
    ExecError, ExecRequest, ExecResponse, ExecSession, ExecStream, FailureScope, ResponseBody, SessionLease,
};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use http::HeaderMap;
use tokio::sync::{OwnedMutexGuard, mpsc, watch};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};

use deflate::Inflate;

/// The upstream socket: tungstenite over the upgraded connection, with permessage-deflate
/// reads when negotiated.
pub(crate) type WebSocket = WebSocketStream<Inflate<wreq::Upgraded>>;

use crate::codex::CodexExecutor;
use crate::codex_json::{set_raw, set_str};
use crate::codex_quota::{self, QuotaSignals};
use crate::codex_request::{self as request, Settings, View};
use crate::codex_response as response;
use crate::proxy::Proxy;
use crate::ws_cause;

/// `codexResponsesWebsocketIdleTimeout`: read deadline renewed before every read.
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// `codexResponsesWebsocketHandshakeTO`.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound on one upstream frame write (docs/DIFFERENCES-FROM-GO.md). Go sets no write
/// deadline, so a write into a half-open socket held the turn until the 300 s read
/// deadline. A large frame gets as long as it takes at `rate` bytes per second.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WriteLimit {
    pub(crate) floor: Duration,
    pub(crate) rate: usize,
}

// ponytail: a fixed 128 KiB/s floor rate (about 1 Mbit/s); a slower uplink sending a
// multi-megabyte frame times out. Make it configurable if such a link shows up.
pub(crate) const WRITE_LIMIT: WriteLimit = WriteLimit {
    floor: Duration::from_secs(10),
    rate: 128 << 10,
};

impl WriteLimit {
    fn deadline(self, len: usize) -> Duration {
        self.floor
            .max(Duration::from_secs(len.div_ceil(self.rate.max(1)) as u64))
    }
}
/// How long a socket waits for upstream's `response.interrupt.accepted` or `.failed`
/// after the interrupted response ended: Codex e0aa043 gives up after one second from its
/// cancel (`codex-rs/core/src/session/turn.rs`) and retires the connection, so a later
/// acknowledgement helps no one.
pub(crate) const ACK_WAIT: Duration = Duration::from_secs(1);
/// Interrupts a turn tracks for acknowledgement; more are still forwarded, and their
/// acknowledgements are dropped like unsolicited ones.
// ponytail: a fixed cap; Codex sends one interrupt per response. Raise it if a client
// legitimately interrupts many responses within one turn.
const MAX_AWAITED: usize = 64;
/// Go's per-turn read channel capacity.
const TURN_BUFFER: usize = 4096;
/// Bound on a handshake rejection body.
const MAX_HANDSHAKE_BODY: usize = 64 * 1024;
/// Largest upstream frame or message read.
// ponytail: Go sets no read limit; 64 MiB (the HTTP body cap) keeps one socket's memory
// bounded while fitting image-bearing output items.
const MAX_UPSTREAM_MESSAGE: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct Target {
    pub(crate) credential: String,
    pub(crate) url: String,
    /// The proxy to dial with, parsed from `setting`.
    pub(crate) proxy: Proxy,
    /// The trimmed effective setting: the credential's own, else `requests.proxy-url`.
    /// Go compares these strings (`websocketSessionTargetMatches`), not parsed proxies:
    /// `direct` and `none` are different targets.
    setting: String,
    /// The trimmed `requests.proxy-url` when the target was resolved.
    global: String,
    /// Whether `setting` came from `requests.proxy-url` (the credential sets none).
    inherited: bool,
}

impl Target {
    pub(crate) fn new(credential: &Credential, url: String, cfg: &Config) -> Self {
        let own = Proxy::own_url(credential);
        let global = Proxy::global_url(cfg);
        let setting = own.map_or_else(|| global.clone(), str::to_owned);
        Self {
            credential: credential.id.clone(),
            url,
            proxy: Proxy::parse(&setting),
            setting,
            global,
            inherited: own.is_none(),
        }
    }

    /// `websocketSessionTargetMatches` under the current config.
    pub(crate) fn same(&self, want: &Target) -> bool {
        self.credential == want.credential && self.url == want.url && self.setting == want.setting
    }

    /// Whether the live socket dialed for `self` still serves `want`. Go 1e37b52b
    /// (`effectiveConfig`): for the same credential, the global proxy is the one the
    /// socket dialed with, so a reload that changes only `requests.proxy-url` keeps the
    /// socket. New sockets dial with the new setting.
    pub(crate) fn serves(&self, want: &Target) -> bool {
        let setting = if want.inherited { &self.global } else { &want.setting };
        self.credential == want.credential && self.url == want.url && self.setting == *setting
    }
}

pub(crate) enum Read {
    Text(String),
    /// The socket failed; the reader has already invalidated it.
    Failed(ExecError),
}

/// The reader's hand-off to the turn in progress. One lock covers both fields, so a turn
/// that activates after the reader exited sees why instead of waiting forever.
#[derive(Default)]
pub(crate) struct Link {
    /// The turn currently reading, if any.
    active: Option<mpsc::Sender<Read>>,
    /// Why the reader stopped (`upstreamDisconnectError`); set once when it exits.
    pub(crate) lost: Option<ExecError>,
    /// The active turn gives a Codex client a context-window rejection as a
    /// `response.failed` and ends with it (`Turn::invalidate_after_event`); not a duplex turn, whose
    /// connection survives that event.
    preserves: bool,
    /// The loss the reader left unreported because the event it last delivered to the
    /// turn is such a failure: the turn's own error after the event replaces it, and the
    /// turn reports this loss only if it ends some other way.
    deferred: Option<ExecError>,
    /// Response IDs this socket already finished (`terminalResponseIDs`), each with the
    /// activation that was current when the reader saw it: a late `response.interrupt`
    /// for one finished by an earlier turn is done while a later turn reads.
    // ponytail: grows by one ID per finished response for the socket's life, as Go's
    // does; bound it if sockets ever outlive thousands of turns.
    terminal: HashMap<String, u64>,
    /// Counts activations; the turn reading now (if any) is number `turns`.
    turns: u64,
    /// Response IDs whose interrupt the reading turn forwarded and upstream has not
    /// acknowledged yet. Only their acknowledgements reach the turn.
    awaited: std::collections::HashSet<String>,
    /// An interrupt forwarded while no turn reads (the response already ended): its
    /// acknowledgement goes to the waiting [`Pool::interrupt`] call.
    late: Option<(String, tokio::sync::oneshot::Sender<String>)>,
}

/// `response.interrupt.accepted` and `.failed`: the `response_id` they acknowledge, empty
/// when it is missing or not a string. `None` for any other event.
fn acknowledged_id(payload: &str) -> Option<String> {
    let kind = rawjson::get(payload, "type");
    if !matches!(kind.str(), "response.interrupt.accepted" | "response.interrupt.failed") {
        return None;
    }
    let id = rawjson::get(payload, "response_id");
    Some(if id.kind() == Kind::String {
        id.str().to_owned()
    } else {
        String::new()
    })
}

/// `response.completed`, `.done`, `.incomplete` and `.failed` (Go `isTerminalEvent`
/// without `error`) with a non-empty string `response.id`.
fn terminal_response_id(payload: &str) -> Option<String> {
    let kind = rawjson::get(payload, "type");
    if !matches!(
        kind.str(),
        "response.completed" | "response.done" | "response.incomplete" | "response.failed"
    ) {
        return None;
    }
    let id = rawjson::get(payload, "response.id");
    (id.kind() == Kind::String && !id.str().is_empty()).then(|| id.str().to_owned())
}

/// Go `ErrNoActiveUpstreamWebsocket`.
pub const NO_ACTIVE_UPSTREAM: &str = "no active upstream websocket for response.interrupt";

/// Why [`CodexExecutor::interrupt`] did not deliver a `response.interrupt`.
#[derive(Debug, PartialEq, Eq)]
pub enum InterruptError {
    /// No turn is reading on the session's current socket ([`NO_ACTIVE_UPSTREAM`]): an
    /// HTTP turn may still cancel locally.
    NoActive,
    /// The socket was lost, the credential is disabled, or the write failed.
    Rejected(String),
}

impl std::fmt::Display for InterruptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoActive => f.write_str(NO_ACTIVE_UPSTREAM),
            Self::Rejected(text) => f.write_str(text),
        }
    }
}

pub(crate) struct Upstream {
    pub(crate) target: Target,
    pub(crate) sink: tokio::sync::Mutex<SplitSink<WebSocket, Message>>,
    pub(crate) link: Mutex<Link>,
    pub(crate) reader: Mutex<Option<tokio::task::AbortHandle>>,
    /// The Home pick this socket keeps (Go `session.lifecycle`); `true` once the socket
    /// shut down, after which no pick binds.
    lease: Mutex<(bool, Option<SessionLease>)>,
    write: WriteLimit,
    /// The `openai-model` this socket's own upgrade returned, set once by the Codex
    /// executor before the socket is published. Later requests on this socket repeat
    /// it ([`Upstream::later_metadata`]); no other socket's value can reach them.
    upgrade_model: std::sync::OnceLock<UpgradeModel>,
}

/// A socket's upgrade `openai-model` and the upstream model the upgrade was sent for.
struct UpgradeModel {
    /// The routed upstream model of the request that dialed the socket.
    requested: String,
    /// The upgrade's `openai-model`, trimmed and not empty.
    served: String,
}

impl UpgradeModel {
    /// `None` when the upgrade has no usable `openai-model`.
    fn from_handshake(requested: &str, handshake: &HeaderMap) -> Option<Self> {
        let served = metadata_value(handshake, OPENAI_MODEL)?;
        Some(Self {
            requested: requested.to_owned(),
            served: served.to_owned(),
        })
    }
}

impl Upstream {
    /// A socket for `target`, not yet published to a session and without a reader.
    pub(crate) fn new(target: Target, sink: SplitSink<WebSocket, Message>, write: WriteLimit) -> Arc<Self> {
        Arc::new(Self {
            target,
            write,
            sink: tokio::sync::Mutex::new(sink),
            link: Mutex::default(),
            reader: Mutex::default(),
            lease: Mutex::default(),
            upgrade_model: std::sync::OnceLock::new(),
        })
    }

    /// Metadata for a request served on this socket after the request that opened it:
    /// only `openai-model` from this socket's own upgrade. The pinned client re-emits its
    /// socket's upgrade model as `ServerModel` on every request
    /// (`codex-api/src/endpoint/responses_websocket.rs:302-303`), and its startup
    /// prewarm drains the opening request's notifications (`core/src/client.rs:2550-2557`)
    /// before the real request reuses the socket. Turn state is never repeated: it
    /// belongs to the turn that opened the socket, and a later turn's lock is first-wins
    /// (`responses_websocket.rs:934-938`).
    ///
    /// Empty when this request's model was changed by the proxy (`model_kept`), when
    /// the socket was opened for another upstream model, or when the upgrade returned
    /// no model. The model-switch omission is an additional proxy policy: the pinned
    /// client repeats unconditionally. Backend applicability after a switch is unknown,
    /// so omission can hide a still-applicable reroute notice. See CONTRACT.md's
    /// "Handshake metadata" policy and tradeoff.
    pub(crate) fn later_metadata(&self, model: &str, model_kept: bool) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let Some(upgrade) = self.upgrade_model.get() else {
            return headers;
        };
        if !model_kept || !upgrade.requested.eq_ignore_ascii_case(model) {
            return headers;
        }
        if let Ok(value) = http::HeaderValue::from_str(&upgrade.served) {
            headers.insert(OPENAI_MODEL, value);
        }
        headers
    }

    /// The read channel for the next turn. `preserves`: see [`Link::preserves`].
    pub(crate) fn activate(&self, preserves: bool) -> mpsc::Receiver<Read> {
        let (tx, rx) = mpsc::channel(TURN_BUFFER);
        let mut link = self.link.lock().expect("link");
        link.turns += 1;
        link.awaited.clear();
        match &link.lost {
            Some(error) => {
                let _ = tx.try_send(Read::Failed(turn_error(error)));
            }
            None => {
                link.active = Some(tx);
                link.preserves = preserves;
            }
        }
        rx
    }

    pub(crate) fn deactivate(&self) {
        let mut link = self.link.lock().expect("link");
        link.active.take();
        link.awaited.clear();
    }

    /// The turn read the acknowledgement for `id`: no later one is expected.
    fn acknowledged(&self, id: &str) {
        self.link.lock().expect("link").awaited.remove(id);
    }

    /// The turn read the terminal event of response `id`. True when an interrupt for it
    /// still waits for its acknowledgement, so the turn keeps reading; otherwise the turn
    /// stops reading here, in the same lock as the check, so an interrupt arriving now
    /// finds the socket idle and waits for the acknowledgement itself.
    fn finish(&self, id: &str) -> bool {
        let mut link = self.link.lock().expect("link");
        if link.awaited.contains(id) {
            return true;
        }
        link.active.take();
        link.awaited.clear();
        false
    }

    /// `writeCodexWebsocketMessage` + `mapCodexWebsocketWriteError`: a write after the
    /// upstream closed with 1009 reports the request-scoped 413 instead. A write that
    /// outlasts [`WriteLimit`] fails like any other write: the caller invalidates the
    /// socket, so a partly written frame is never followed by another. Cost: one timer
    /// entry while a write is in flight.
    pub(crate) async fn send(&self, frame: String) -> Result<(), ExecError> {
        let len = frame.len();
        self.write_message(Message::text(frame), len).await
    }

    /// [`Self::send`] for a client's own bytes: one text frame holding them unchanged,
    /// UTF-8 or not, as gorilla writes a `TextMessage`.
    pub(crate) async fn send_raw(&self, payload: Bytes) -> Result<(), ExecError> {
        use tokio_tungstenite::tungstenite::protocol::frame::{
            Frame,
            coding::{Data, OpCode},
        };
        let len = payload.len();
        self.write_message(
            Message::Frame(Frame::message(payload, OpCode::Data(Data::Text), true)),
            len,
        )
        .await
    }

    async fn write_message(&self, message: Message, len: usize) -> Result<(), ExecError> {
        let deadline = self.write.deadline(len);
        let sent = tokio::time::timeout(deadline, async { self.sink.lock().await.send(message).await }).await;
        match sent {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(write_error(
                self.link.lock().expect("link").lost.as_ref(),
                "codex websockets executor: write failed",
            )
            .with_diagnostic(ws_cause::socket(&error))),
            Err(_) => Err(write_error(
                self.link.lock().expect("link").lost.as_ref(),
                "codex websockets executor: write timed out",
            )
            .with_diagnostic(ws_cause::write_timeout(deadline, len))),
        }
    }

    /// Go `bindExecutionLifecycle`: the socket keeps the attempt's Home pick until it is
    /// invalidated, replaced or closed, and Home draining the pick invalidates the
    /// socket. A pick bound earlier ends (`target_replaced`).
    pub(crate) fn bind(
        self: &Arc<Self>,
        session: &Arc<Session>,
        lease: Option<&SessionLease>,
    ) -> Result<(), ExecError> {
        let Some(lease) = lease else { return Ok(()) };
        if self
            .lease
            .lock()
            .expect("lease")
            .1
            .as_ref()
            .is_some_and(|l| l.same(lease))
        {
            return Ok(());
        }
        let (weak_session, weak_conn) = (Arc::downgrade(session), Arc::downgrade(self));
        let close = Box::new(move || {
            if let (Some(session), Some(conn)) = (weak_session.upgrade(), weak_conn.upgrade()) {
                session.invalidate(
                    &conn,
                    &transport("codex websockets executor: execution lifecycle ended"),
                    false,
                );
            }
        });
        let unbound = || transport("codex websockets executor: websocket connection closed during lifecycle bind");
        if !lease.0.retain(close) {
            session.invalidate(self, &unbound(), false);
            return Err(unbound());
        }
        let previous = {
            let mut slot = self.lease.lock().expect("lease");
            if slot.0 {
                drop(slot);
                lease.0.end();
                return Err(unbound());
            }
            slot.1.replace(lease.clone())
        };
        if let Some(previous) = previous {
            previous.0.end();
        }
        Ok(())
    }

    /// Stops the reader and closes the socket, ending the Home pick it kept. Idempotent.
    pub(crate) fn shutdown(self: &Arc<Self>) {
        if let Some(reader) = self.reader.lock().expect("reader handle").take() {
            reader.abort();
        }
        self.deactivate();
        self.link.lock().expect("link").late = None;
        let lease = {
            let mut slot = self.lease.lock().expect("lease");
            slot.0 = true;
            slot.1.take()
        };
        if let Some(lease) = lease {
            lease.0.end();
        }
        let this = self.clone();
        tokio::spawn(async move {
            let _ = tokio::time::timeout(Duration::from_secs(1), async {
                let mut sink = this.sink.lock().await;
                let _ = sink.send(Message::Close(None)).await;
                let _ = sink.close().await;
            })
            .await;
        });
    }
}

pub(crate) struct Session {
    /// Serialises turns (`reqMu`).
    pub(crate) turn: Arc<tokio::sync::Mutex<()>>,
    pub(crate) conn: Mutex<Option<Arc<Upstream>>>,
    /// Set once when the session's socket is lost (`notifyUpstreamDisconnect`).
    closed: watch::Sender<Option<ExecError>>,
    /// The downstream connection's client frames when response steering is configured.
    steering: Mutex<Option<Arc<SteeringInput>>>,
    /// The credential of the last socket dialed (Go keeps `sess.authID` after the socket
    /// is gone, and removing that credential still ends the session).
    credential: Mutex<String>,
}

impl Session {
    /// Makes `conn` the session's socket.
    pub(crate) fn publish(&self, conn: Arc<Upstream>) {
        let mut slot = self.conn.lock().expect("session conn");
        conn.target
            .credential
            .clone_into(&mut self.credential.lock().expect("session credential"));
        *slot = Some(conn);
    }

    pub(crate) fn current(&self) -> Option<Arc<Upstream>> {
        self.conn.lock().expect("session conn").clone()
    }

    fn steering(&self) -> Option<Arc<SteeringInput>> {
        self.steering.lock().expect("session steering").clone()
    }

    /// `invalidateUpstreamConn`: only the session's current socket is dropped, so a stale
    /// reader cannot tear down its replacement. `notify` tells the downstream handler.
    pub(crate) fn invalidate(&self, conn: &Arc<Upstream>, error: &ExecError, notify: bool) {
        {
            let mut current = self.conn.lock().expect("session conn");
            if !current.as_ref().is_some_and(|c| Arc::ptr_eq(c, conn)) {
                return;
            }
            *current = None;
        }
        if notify {
            self.notify(error);
        }
        conn.shutdown();
    }

    /// `notifyUpstreamDisconnect`: the first loss is the one reported.
    fn notify(&self, error: &ExecError) {
        self.closed.send_if_modified(|slot| {
            let first = slot.is_none();
            if first {
                *slot = Some(error.clone());
            }
            first
        });
    }
}

/// Upstream sockets by downstream session id.
pub(crate) struct Pool {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// Read deadline for each upstream application message.
    pub(crate) idle: Duration,
    pub(crate) write: WriteLimit,
    /// Bound on waiting for an interrupt acknowledgement after its response ended.
    pub(crate) ack_wait: Duration,
}

impl Default for Pool {
    fn default() -> Self {
        Self {
            sessions: Mutex::default(),
            idle: IDLE_TIMEOUT,
            write: WRITE_LIMIT,
            ack_wait: ACK_WAIT,
        }
    }
}

impl Pool {
    #[cfg(test)]
    pub fn with_idle(idle: Duration) -> Self {
        Self {
            idle,
            ..Self::default()
        }
    }

    #[cfg(test)]
    pub(crate) fn with_ack_wait(ack_wait: Duration) -> Self {
        Self {
            ack_wait,
            ..Self::default()
        }
    }

    #[cfg(test)]
    pub(crate) fn with_write(write: WriteLimit) -> Self {
        Self {
            write,
            ..Self::default()
        }
    }

    pub(crate) fn session(&self, id: &str) -> Arc<Session> {
        self.sessions
            .lock()
            .expect("sessions")
            .entry(id.to_owned())
            .or_insert_with(|| {
                Arc::new(Session {
                    turn: Arc::default(),
                    conn: Mutex::default(),
                    closed: watch::channel(None).0,
                    steering: Mutex::default(),
                    credential: Mutex::default(),
                })
            })
            .clone()
    }

    /// The session, without creating one.
    fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().expect("sessions").get(id).cloned()
    }

    /// `WithWebsocketInput`: binds the downstream connection's frames to its session.
    pub fn attach_steering(&self, id: &str, input: SteeringInput) {
        *self.session(id).steering.lock().expect("session steering") = Some(Arc::new(input));
    }

    /// `CloseExecutionSession`: the downstream connection ended.
    pub fn close(&self, id: &str) {
        let session = self.sessions.lock().expect("sessions").remove(id);
        if let Some(conn) = session.and_then(|s| s.conn.lock().expect("session conn").take()) {
            conn.shutdown();
        }
    }

    /// `closeCodexWebsocketSessionsInStoreForAuthID`: ends the sessions whose last socket
    /// was dialed for `credential` (a removed credential), like [`Self::close`] for each,
    /// and leaves every other session open.
    pub(crate) fn close_credential(&self, credential: &str) {
        self.close_credential_with(credential, |_| {});
    }

    /// [`Self::close_credential`]; `detached(id)` runs under the pool lock as each session
    /// leaves it, so state keyed by the session id goes with it before the id can be
    /// reused (Go deletes xAI's ID state under the session-store mutex). Sockets shut down
    /// after the lock is released.
    pub(crate) fn close_credential_with(&self, credential: &str, mut detached: impl FnMut(&str)) {
        let sessions: Vec<_> = {
            let sessions = self.sessions.lock().expect("sessions");
            sessions.iter().map(|(id, s)| (id.clone(), s.clone())).collect()
        };
        let matching: Vec<_> = sessions
            .into_iter()
            .filter(|(_, s)| *s.credential.lock().expect("session credential") == credential)
            .collect();
        let mut closing = Vec::new();
        {
            let mut sessions = self.sessions.lock().expect("sessions");
            for (id, session) in matching {
                // A session closed or replaced since the snapshot is not this one.
                if !sessions.get(&id).is_some_and(|s| Arc::ptr_eq(s, &session)) {
                    continue;
                }
                sessions.remove(&id);
                detached(&id);
                closing.push(session);
            }
        }
        for session in closing {
            if let Some(conn) = session.conn.lock().expect("session conn").take() {
                conn.shutdown();
            }
        }
    }

    /// The error that lost the session's upstream socket, if it was lost.
    pub fn loss(&self, id: &str) -> Option<ExecError> {
        let session = self.get(id)?;
        session.closed.borrow().clone()
    }

    /// `InterruptExecutionSession`: writes the client's `response.interrupt` unchanged
    /// to the session's socket. It never dials, replays or selects another credential.
    /// `enabled(credential_id)` is the live account check, called without holding a
    /// session lock.
    ///
    /// Current Codex waits for upstream's `response.interrupt.accepted` or `.failed`
    /// after the response's terminal event (docs/DIFFERENCES-FROM-GO.md). An interrupt
    /// for a response of the turn reading now is forwarded and its acknowledgement
    /// reaches that turn. One for a response the socket finished while no turn reads is
    /// forwarded too (Go answers success without sending it), and upstream's
    /// acknowledgement, if it comes within [`Pool::ack_wait`], is returned for the
    /// caller to deliver. One for a response finished before the turn reading now is
    /// done without being forwarded, as in Go.
    pub async fn interrupt(
        &self,
        id: &str,
        payload: &[u8],
        enabled: impl FnOnce(&str) -> bool,
    ) -> Result<Option<Bytes>, InterruptError> {
        let session = Some(id.trim())
            .filter(|id| !id.is_empty())
            .and_then(|id| self.get(id))
            .ok_or(InterruptError::NoActive)?;
        let conn = session.current().ok_or(InterruptError::NoActive)?;
        let auth_enabled = enabled(&conn.target.credential);
        let response_id = cpa_common::json::get(payload, "response_id").bytes();
        let response_id = std::str::from_utf8(&response_id).ok().map(str::to_owned);
        let late = {
            // One snapshot of the socket's state: a completion landing between the
            // checks would otherwise turn a late interrupt into a stale error.
            let current = session.conn.lock().expect("session conn");
            if !current.as_ref().is_some_and(|c| Arc::ptr_eq(c, &conn)) {
                return Err(InterruptError::NoActive);
            }
            let mut link = conn.link.lock().expect("link");
            if let Some(lost) = &link.lost {
                return Err(InterruptError::Rejected(
                    String::from_utf8_lossy(&lost.body).into_owned(),
                ));
            }
            let finished_by = response_id.as_ref().and_then(|id| link.terminal.get(id)).copied();
            let reading = link.active.is_some();
            match finished_by {
                // Finished before the turn reading now: that response is done.
                Some(turn) if reading && turn != link.turns => return Ok(None),
                // A socket kept from an earlier turn, and an ID it never finished: not
                // the current upstream (an HTTP turn may still cancel locally).
                None if !reading => return Err(InterruptError::NoActive),
                _ => {}
            }
            if !auth_enabled {
                return Err(InterruptError::Rejected(
                    "websocket credential is no longer enabled".into(),
                ));
            }
            match (reading, response_id) {
                (true, Some(id)) => {
                    if link.awaited.len() < MAX_AWAITED {
                        link.awaited.insert(id);
                    }
                    None
                }
                (true, None) => None,
                (false, id) => {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    // An earlier waiter, if any, gives up now.
                    link.late = Some((id.unwrap_or_default(), tx));
                    Some(rx)
                }
            }
        };
        // A failed or timed-out write may leave a partial frame: the socket is dropped
        // like after any failed turn write (Go leaves that to its reader).
        if let Err(error) = conn.send_raw(Bytes::copy_from_slice(payload)).await {
            session.invalidate(&conn, &error, true);
            return Err(InterruptError::Rejected(
                String::from_utf8_lossy(&error.body).into_owned(),
            ));
        }
        let Some(rx) = late else { return Ok(None) };
        let acknowledged = tokio::time::timeout(self.ack_wait, rx).await;
        let mut link = conn.link.lock().expect("link");
        if link.late.as_ref().is_some_and(|(_, tx)| tx.is_closed()) {
            link.late = None;
        }
        match acknowledged {
            Ok(Ok(ack)) => Ok(Some(Bytes::from(ack))),
            // The reader stopped: the socket is gone.
            Ok(Err(_)) => match &link.lost {
                Some(lost) => Err(InterruptError::Rejected(
                    String::from_utf8_lossy(&lost.body).into_owned(),
                )),
                None => Ok(None),
            },
            Err(_) => Ok(None),
        }
    }

    /// Resolves with the error that lost the session's upstream socket.
    pub fn closed(&self, id: &str) -> impl std::future::Future<Output = ExecError> + Send + 'static {
        let mut rx = self.session(id).closed.subscribe();
        async move {
            loop {
                if let Some(error) = rx.borrow_and_update().clone() {
                    return error;
                }
                if rx.changed().await.is_err() {
                    return std::future::pending().await;
                }
            }
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.sessions.lock().expect("sessions").len()
    }
}

fn transport(message: &str) -> ExecError {
    ExecError::local(502, FailureScope::Transport, message)
}

/// gorilla/websocket `CloseError.Error()`: what Go returns for an upstream close.
fn close_error_text(code: u16, text: &str) -> String {
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
    let mut out = format!("websocket: close {code}{name}");
    if !text.is_empty() {
        out.push_str(": ");
        out.push_str(text);
    }
    out
}

/// The session's loss on an upstream close 1009: Go's reader notifies the downstream
/// handler with gorilla's raw close error, so the client sees close 1009 with the
/// upstream's reason ("message too big" when empty).
/// `close_reason` keeps gorilla's `CloseError.Text` verbatim for the handler, which only
/// trims the message of a mapped 413.
fn closed_too_big(reason: &str) -> ExecError {
    let message = if reason.is_empty() { "message too big" } else { reason };
    ExecError::local(
        413,
        FailureScope::Request,
        serde_json::json!({
            "error": {"message": message, "type": "invalid_request_error", "code": "message_too_big"},
            "close_reason": reason,
        })
        .to_string(),
    )
}

/// `mapCodexWebsocketReadError` / `mapCodexWebsocketWriteError`: what a turn reports
/// after the upstream closed with 1009, a fixed request-scoped 413.
fn message_too_big() -> ExecError {
    ExecError::local(
        413,
        FailureScope::Request,
        r#"{"error":{"message":"upstream websocket message too big","type":"invalid_request_error","code":"message_too_big"}}"#,
    )
}

/// The error a turn reads from the session's loss: the mapped 413 for a 1009 close (only
/// [`closed_too_big`] makes a 413 loss), the loss itself otherwise.
/// The reader's error for a binary message (Go's `unexpected_binary` stage).
const UNEXPECTED_BINARY: &str = "codex websockets executor: unexpected binary message";

/// A failed or timed-out write: the request-scoped 413 when the reader already saw the
/// upstream close with 1009 (the frame was too big), else a transport error.
pub(crate) fn write_error(lost: Option<&ExecError>, message: &str) -> ExecError {
    lost.filter(|e| e.status == 413)
        .map_or_else(|| transport(message), turn_error)
}

fn turn_error(lost: &ExecError) -> ExecError {
    if lost.status == 413 {
        ExecError {
            diagnostic: lost.diagnostic.clone(),
            ..message_too_big()
        }
    } else {
        lost.clone()
    }
}

/// `buildCodexResponsesWebsocketURL`. Go returns its parse, scheme and empty-host errors
/// as plain errors before dialing.
fn ws_url(http_url: &str) -> Result<String, ExecError> {
    let mut parsed = crate::xai_url::parse(http_url.trim()).map_err(crate::xai_url::statusless)?;
    parsed.scheme = match parsed.scheme.as_str() {
        "http" => "ws".to_owned(),
        "https" => "wss".to_owned(),
        other => {
            return Err(crate::xai_url::statusless(format!(
                "codex websockets executor: unsupported responses websocket URL scheme {}",
                cpa_common::gostr::quote(other)
            )));
        }
    };
    if parsed.host.trim().is_empty() {
        return Err(crate::xai_url::statusless(
            "codex websockets executor: responses websocket URL host is empty",
        ));
    }
    Ok(crate::xai_url::string(&parsed))
}

/// `buildCodexWebsocketErrorPayload` for an `error` frame with a positive status: the
/// status and the body Go classifies (and clears the reasoning replay with).
fn ws_error_body(payload: &str) -> Option<(u16, String)> {
    if rawjson::get(payload, "type").str().trim() != "error" {
        return None;
    }
    let mut status = rawjson::get(payload, "status").i64();
    if status == 0 {
        status = rawjson::get(payload, "status_code").i64();
    }
    // Go accepts any positive status; larger than u16 cannot be represented here.
    let Ok(status) = u16::try_from(status) else {
        return None;
    };
    if status == 0 {
        return None;
    }
    let mut out = set_raw("{}", "status", &status.to_string());
    let body = rawjson::get(payload, "body");
    let error = rawjson::get(payload, "error");
    if body.exists() {
        out = set_raw(&out, "body", body.json());
    }
    if body.exists() && body.get("error").exists() {
        out = set_raw(&out, "error", body.get("error").json());
    } else if error.exists() {
        out = set_raw(&out, "error", error.json());
    } else {
        out = set_str(&out, "error.type", "server_error");
        out = set_str(&out, "error.message", response::go_status_text(status));
    }
    Some((status, out))
}

/// `parseCodexWebsocketErrorWithCooling`: an `error` frame that carries an HTTP status.
pub(crate) fn ws_error(payload: &str, model_level_cooling: bool) -> Option<ExecError> {
    let (status, out) = ws_error_body(payload)?;
    let mut headers = HeaderMap::new();
    let raw_headers = rawjson::get(payload, "headers");
    if raw_headers.kind() == Kind::Object {
        raw_headers.each(|k, v| {
            let value = match v.kind() {
                Kind::String => v.str().trim().to_owned(),
                Kind::Number | Kind::True | Kind::False => v.json().trim().to_owned(),
                _ => String::new(),
            };
            if let (Ok(name), Ok(value)) = (
                http::HeaderName::try_from(k.str().trim()),
                http::HeaderValue::from_str(&value),
            ) && !value.is_empty()
            {
                headers.insert(name, value);
            }
            true
        });
    }
    let connection_limit = [
        "error.code",
        "error.type",
        "body.error.code",
        "body.error.type",
        "code",
        "error",
    ]
    .iter()
    .any(|p| rawjson::get(payload, p).str().trim() == "websocket_connection_limit_reached");
    // Go `statusErrWithHeaders`: the frame's headers reach the client under passthrough.
    let addon = (!headers.is_empty()).then(|| Box::new(headers.clone()));
    let mut error = response::status_error_raw(status, &out, headers, model_level_cooling);
    error.addon = addon;
    if error.retry_after.is_none() && connection_limit {
        error.retry_after = Some(Duration::ZERO);
    }
    Some(error)
}

/// One upstream message, classified.
enum Frame {
    /// Empty text frame (heartbeat).
    Skip,
    Event {
        out: Bytes,
        raw_len: usize,
        bufferable: bool,
        terminal: bool,
        /// The terminal event's `response.id`.
        terminal_id: Option<String>,
        /// The `response_id` an interrupt acknowledgement names.
        ack: Option<String>,
    },
    /// The response failed. `read` means the reader already dropped the socket;
    /// `overload` is a transient capacity rejection another credential may serve.
    /// `event` is the upstream event the client receives unchanged before `error`.
    Failed {
        error: ExecError,
        overload: bool,
        read: bool,
        status_frame: bool,
        event: Option<Bytes>,
    },
}

/// One turn on a pooled socket. Dropping it releases the turn lock and the reader.
struct Turn {
    rx: mpsc::Receiver<Read>,
    conn: Arc<Upstream>,
    session: Arc<Session>,
    _turn: Option<OwnedMutexGuard<()>>,
    /// The observation of the message being read, while it runs.
    observing: Option<crate::ws_observation::Observation>,
    /// A Codex client: it gets an upstream context-window rejection as a `response.failed`
    /// before the error (`request::codex_client`).
    codex_client: bool,
    cooling: bool,
    quota: Arc<QuotaSignals>,
    credential: String,
    /// Handshake headers plus quota headers from this turn's events.
    observed: HeaderMap,
    /// The attempt's usage record.
    usage: cpa_core::exec::UsageSink,
    /// The upstream model, for per-model quota snapshots.
    model: String,
    /// The attempt's wire capture (Go's `api.websocket.*` timeline events).
    wire: crate::codex_capture::Wire,
    /// Bound on waiting for an interrupt acknowledgement after the response ended.
    ack_wait: Duration,
    /// After the terminal event: the interrupted response whose acknowledgement the turn
    /// still waits for, and until when.
    drain: Option<(String, tokio::time::Instant)>,
}

impl Drop for Turn {
    fn drop(&mut self) {
        // An abandoned turn keeps its session until an observation it started returns,
        // as Go's executor holds the session lock through the synchronous observer.
        self.report_deferred();
        let conn = self.conn.clone();
        crate::ws_observation::release_after(self.observing.take(), self._turn.take(), move || conn.deactivate());
    }
}

impl Turn {
    async fn frame(&mut self) -> Frame {
        let frame = self.read().await;
        if let Frame::Failed { error, read: true, .. } = &frame {
            let binary = error.body.as_ref() == UNEXPECTED_BINARY.as_bytes();
            self.wire
                .ws_exec_error(if binary { "unexpected_binary" } else { "read" }, error);
        }
        frame
    }

    async fn read(&mut self) -> Frame {
        let text = match self.rx.recv().await {
            Some(Read::Text(text)) => text,
            Some(Read::Failed(error)) => {
                self.report_deferred();
                return Frame::Failed {
                    error,
                    overload: false,
                    read: true,
                    status_frame: false,
                    event: None,
                };
            }
            // A full channel can drop the reader's terminal error; the session still
            // recorded the loss (Go's reader waits to deliver it).
            None => {
                return Frame::Failed {
                    error: self.loss().map_or_else(
                        || transport("codex websockets executor: session read channel closed"),
                        |l| turn_error(&l),
                    ),
                    overload: false,
                    read: true,
                    status_frame: false,
                    event: None,
                };
            }
        };
        let payload = text.trim();
        if payload.is_empty() {
            return Frame::Skip;
        }
        // The upstream event boundary (`crate::codex_event`): a frame the pinned reader
        // cannot decode is skipped there, unless it is a wrapped status `error`.
        let event = crate::codex_event::decode(&text);
        let wrapped = event.is_none() && crate::codex_event::wrapped_error(&text);
        if event.is_some() || wrapped {
            self.observe(payload);
        }
        crate::ws_observation::observe(&mut self.observing, self.wire.ws_response_observation(payload)).await;
        let raw_len = payload.len();
        let Some(event) = event else {
            if wrapped && let Some(mut error) = ws_error(payload, self.cooling) {
                self.wire.ws_exec_error("upstream_error", &error);
                self.deliver_as_sent(&mut error, &text);
                return Frame::Failed {
                    error,
                    overload: false,
                    read: false,
                    status_frame: true,
                    event: None,
                };
            }
            return undecoded(text, raw_len);
        };
        if let Some(mut error) = ws_error(payload, self.cooling) {
            self.wire.ws_exec_error("upstream_error", &error);
            self.deliver_as_sent(&mut error, &text);
            return Frame::Failed {
                error,
                overload: false,
                read: false,
                status_frame: true,
                event: None,
            };
        }
        if let Some((mut error, body)) = response::terminal_failure(payload, self.cooling) {
            self.wire.ws_exec_error("upstream_error", &error);
            // The context-window `error` a Codex client gets as `response.failed` (#5) is
            // delivered ahead of the error; any other failure of a native turn travels on
            // the error, for delivery once failover is decided.
            let event = self.preserved_event(payload);
            if event.is_none() {
                self.deliver_as_sent(&mut error, &text);
            }
            return Frame::Failed {
                error,
                overload: response::is_overload(&body),
                read: false,
                status_frame: false,
                event,
            };
        }
        let kind = event.kind.into_owned();
        // Every `response.incomplete` is the provider's terminal event, whatever its
        // reason, output or usage, and whether or not anything reached the client yet:
        // first-party Codex classifies its reason as an interrupted end, a content-filter
        // error or a terminal `ResponseIncomplete` (`sse/responses.rs:451-506` at
        // 44ccf394573c). It is delivered as sent (#25).
        let bufferable = response::bufferable(payload);
        let terminal = matches!(
            kind.as_str(),
            "response.completed" | "response.done" | "response.incomplete"
        );
        let terminal_id = terminal
            .then(|| rawjson::get(payload, "response.id"))
            .filter(|id| id.kind() == Kind::String)
            .map(|id| id.str().to_owned());
        let ack = acknowledged_id(payload);
        // Observation above read a copy; the client gets the frame as it arrived.
        Frame::Event {
            raw_len,
            out: Bytes::from(text),
            bufferable,
            terminal,
            terminal_id,
            ack,
        }
    }

    /// Quota headers and usage from one upstream event.
    fn observe(&mut self, payload: &str) {
        self.signals(payload);
        if self.usage.enabled() {
            self.usage
                .response_line(cpa_core::format::Format::Codex, payload.as_bytes());
        }
    }

    /// Interrupt acknowledgements and quota headers from one upstream event; a duplex
    /// stream reports its usage per response itself.
    fn signals(&mut self, payload: &str) {
        if let Some(id) = acknowledged_id(payload) {
            self.conn.acknowledged(&id);
        }
        if let Some(headers) = codex_quota::event_headers(payload) {
            codex_quota::merge(&mut self.observed, &headers);
            self.quota.observe(&self.credential, &self.model, &self.observed);
        }
    }

    fn invalidate(&self, error: &ExecError, notify: bool) {
        self.session.invalidate(&self.conn, error, notify);
    }

    /// The response `id` ended. True when its interrupt still waits for upstream's
    /// acknowledgement: the turn keeps reading, at most [`Pool::ack_wait`], for that
    /// acknowledgement alone. Otherwise the turn stops reading here.
    fn settle(&mut self, id: Option<&str>) -> bool {
        match id {
            Some(id) if self.conn.finish(id) => {
                self.drain = Some((id.to_owned(), tokio::time::Instant::now() + self.ack_wait));
                true
            }
            Some(_) => false,
            None => {
                self.conn.deactivate();
                false
            }
        }
    }

    /// For a failure the client first receives as the upstream's event: drops the socket
    /// without reporting the session's loss, now or later. The turn's own error follows
    /// the event at once and ends the downstream connection (the Responses WebSocket
    /// handler takes it before writing the event, so an interrupt or a client leaving
    /// during that write cannot replace it); a loss reported meanwhile would close the
    /// client with a generic error and cut off a slow write of the event.
    ///
    /// The reader may already have dropped the socket and left its loss unreported for
    /// this (`Link::deferred`); this turn's error replaces it, so a socket that closes
    /// right after the event neither cuts the event off nor masks its classification.
    /// The socket is dropped before the link is updated: a reader exiting in between
    /// either finds the socket gone, or defers a loss that is then discarded here.
    fn invalidate_after_event(&self, error: &ExecError) {
        self.invalidate(error, false);
        let mut link = self.conn.link.lock().expect("link");
        link.preserves = false;
        link.deferred = None;
    }

    /// Reports a loss the reader left to this turn (`Link::deferred`): before the turn
    /// returns the reader's error, or when it ends without the event's own error.
    ///
    /// Also stops further deferral under the same lock: a dropped turn can keep its read
    /// channel until an observation returns (`Drop`), and a reader exiting in that
    /// window must report its own loss rather than leave it to a turn that is gone.
    fn report_deferred(&self) {
        let deferred = {
            let mut link = self.conn.link.lock().expect("link");
            link.preserves = false;
            link.deferred.take()
        };
        if let Some(error) = deferred {
            self.session.notify(&error);
        }
    }

    /// A native turn's failure reaches the client as the upstream sent it
    /// ([`ExecError::delivered_event`]), while `error` keeps the scheduler's
    /// classification. An account failure keeps the classified delivery instead (the
    /// allowlisted subscription-limit event, #6, or a silent close): its frame can name the
    /// pooled account or carry its tokens.
    fn deliver_as_sent(&self, error: &mut ExecError, frame: &str) {
        if !account_failure(error.status) {
            error.delivered_event = Some(Box::new(Bytes::copy_from_slice(frame.as_bytes())));
        }
    }

    /// The event a Codex client receives before this turn's error for an upstream
    /// context-window rejection (`cpa_common::codex_client::context_window_failure_event`).
    fn preserved_event(&self, payload: &str) -> Option<Bytes> {
        if !self.codex_client {
            return None;
        }
        cpa_common::codex_client::context_window_failure_event(payload).map(|e| Bytes::from(e.into_owned()))
    }

    /// The raw error that lost the session's socket, as the reader recorded it (the
    /// duplex wraps gorilla's close error unmapped).
    fn loss(&self) -> Option<ExecError> {
        self.session.closed.borrow().clone()
    }
}

/// The rest of a turn as a stream; ends after the terminal event or the first error
/// (preceded by the upstream event a Codex client receives unchanged), or, when the
/// response was interrupted and upstream has not acknowledged that yet, after the
/// acknowledgement or [`Pool::ack_wait`] (docs/DIFFERENCES-FROM-GO.md).
fn rest(turn: Turn) -> ExecStream {
    enum State {
        // Boxed: one allocation per turn keeps the other states small.
        Reading(Box<Turn>),
        Failing(ExecError),
        Done,
    }
    futures_util::stream::unfold(State::Reading(Box::new(turn)), |state| async move {
        let mut turn = match state {
            State::Reading(turn) => turn,
            State::Failing(error) => return Some((Err(error), State::Done)),
            State::Done => return None,
        };
        loop {
            let frame = match turn.drain.as_ref().map(|(_, until)| *until) {
                None => turn.frame().await,
                Some(until) => match tokio::time::timeout_at(until, turn.frame()).await {
                    Ok(frame) => frame,
                    // No acknowledgement: the response itself already ended.
                    Err(_) => return None,
                },
            };
            match frame {
                Frame::Skip => continue,
                Frame::Event {
                    out,
                    terminal,
                    terminal_id,
                    ack,
                    ..
                } => {
                    let more = if let Some((interrupted, _)) = &turn.drain {
                        // After the terminal event only acknowledgements reach the client.
                        let Some(ack) = ack else { continue };
                        ack != *interrupted
                    } else {
                        !terminal || turn.settle(terminal_id.as_deref())
                    };
                    let next = if more { State::Reading(turn) } else { State::Done };
                    return Some((Ok(out), next));
                }
                Frame::Failed { error, read, event, .. } => {
                    // `event` comes only with an upstream failure event, never with a
                    // read failure (`read`), whose reader already dropped the socket. After
                    // the terminal event (`drain`) the client has its one terminal event, so
                    // the failure only ends the socket.
                    if let Some(event) = event.filter(|_| turn.drain.is_none()) {
                        turn.invalidate_after_event(&error);
                        return Some((Ok(event), State::Failing(error)));
                    }
                    if error.delivered_event.is_some() {
                        // The client gets the provider's frame with the error: a loss
                        // reported first would replace it with a generic close.
                        turn.invalidate_after_event(&error);
                    } else if !read {
                        turn.invalidate(&error, true);
                    }
                    // The response already ended; the session reports the loss.
                    if turn.drain.is_some() {
                        return None;
                    }
                    return Some((Err(error), State::Done));
                }
            }
        }
    })
    .boxed()
}

/// Drops the socket after an upstream failure frame. A failure the client receives as
/// sent ([`ExecError::delivered_event`]) does not report the session's loss, which would
/// close the client with a generic error first.
fn invalidate_failed(turn: &Turn, error: &ExecError) {
    if error.delivered_event.is_some() {
        turn.invalidate_after_event(error);
    } else {
        turn.invalidate(error, true);
    }
}

/// An upstream frame the pinned reader skips (`crate::codex_event`): delivered as sent,
/// as content, never as a terminal event, failure or acknowledgement. A frame that would
/// read as an account failure is withheld instead ([`account_shaped`]).
fn undecoded(text: String, raw_len: usize) -> Frame {
    if account_shaped(text.trim()) {
        return Frame::Skip;
    }
    Frame::Event {
        raw_len,
        out: Bytes::from(text),
        bufferable: false,
        terminal: false,
        terminal_id: None,
        ack: None,
    }
}

/// Whether an undecodable upstream event would be an account failure if it decoded, by
/// the readings a native WebSocket failure gets ([`ws_error`], then
/// `response::terminal_failure`). A native WebSocket client never receives such a frame
/// as sent, because it can name the pooled account (#6). An undecodable frame can name
/// it too, so it is withheld. That is all the result decides: the frame is not
/// classified, the credential does not cool and the response goes on.
pub(crate) fn account_shaped(payload: &str) -> bool {
    ws_error(payload, false)
        .map(|error| error.status)
        .or_else(|| response::terminal_failure(payload, false).map(|(error, _)| error.status))
        .is_some_and(account_failure)
}

/// An account failure: 401, 402, 403 or 429 as classified. Its frame can name the pooled
/// account, so a native route never hands it to the client as sent (#6).
pub(super) fn account_failure(status: u16) -> bool {
    matches!(status, 401 | 402 | 403 | 429)
}

/// Bootstrap buffering over WebSocket messages (codex_websockets_stream.go): every
/// message read counts toward the 48-frame window, held frames are bounded by 1 MiB.
async fn bootstrap(mut turn: Turn, timeout: Option<Duration>, started: Instant) -> Result<ExecStream, ExecError> {
    let mut held: Vec<Result<Bytes, ExecError>> = Vec::new();
    let (mut frames, mut bytes) = (0usize, 0usize);
    loop {
        let frame = turn.frame().await;
        frames += 1;
        let timed_out = timeout.is_some_and(|t| started.elapsed() >= t);
        let window_open = frames <= response::BOOTSTRAP_MAX_FRAMES && !timed_out;
        match frame {
            Frame::Skip if window_open => continue,
            Frame::Skip => break,
            Frame::Failed { error, read: true, .. } => return Err(error),
            Frame::Failed {
                error,
                status_frame: true,
                ..
            } => {
                invalidate_failed(&turn, &error);
                if !timed_out {
                    return Err(error);
                }
                held.push(Err(error));
                return Ok(futures_util::stream::iter(held).boxed());
            }
            Frame::Failed {
                error, overload, event, ..
            } => {
                if overload && !timed_out {
                    // Fail over before anything was shown; the downstream session survives.
                    turn.invalidate(&error, false);
                    let body = String::from_utf8_lossy(&error.body).into_owned();
                    let mut overloaded = response::bootstrap_overload(&body);
                    overloaded.delivered_event = error.delivered_event;
                    return Err(overloaded);
                }
                if let Some(event) = event {
                    turn.invalidate_after_event(&error);
                    held.push(Ok(event));
                    held.push(Err(error));
                    return Ok(futures_util::stream::iter(held).boxed());
                }
                invalidate_failed(&turn, &error);
                held.push(Err(error));
                return Ok(futures_util::stream::iter(held).boxed());
            }
            Frame::Event {
                out,
                raw_len,
                bufferable,
                terminal,
                terminal_id,
                ..
            } => {
                let size = raw_len + out.len();
                if window_open && bufferable && !terminal && bytes + size <= response::BOOTSTRAP_MAX_BYTES {
                    bytes += size;
                    held.push(Ok(out));
                    continue;
                }
                held.push(Ok(out));
                if terminal {
                    if turn.settle(terminal_id.as_deref()) {
                        // An interrupt acknowledgement still follows the terminal event.
                        return Ok(futures_util::stream::iter(held).chain(rest(turn)).boxed());
                    }
                    return Ok(futures_util::stream::iter(held).boxed());
                }
                break;
            }
        }
    }
    Ok(futures_util::stream::iter(held).chain(rest(turn)).boxed())
}

impl CodexExecutor {
    /// Opens (or reuses) the session's socket for `target`. Returns the handshake
    /// headers when a new socket was dialed (`ensureUpstreamConn`).
    /// `wire` records a refused upgrade and a dial failure (the first dial of a turn;
    /// Go's retry records only `dial_retry`).
    async fn ensure(
        &self,
        session: &Arc<Session>,
        target: &Target,
        headers: &HeaderMap,
        model: &str,
        model_level_cooling: bool,
        wire: Option<&crate::codex_capture::Wire>,
    ) -> Result<(Arc<Upstream>, Option<HeaderMap>), ExecError> {
        if let Some(current) = session.current() {
            if current.target.serves(target) {
                return Ok((current, None));
            }
            // target_changed: another credential, URL or proxy now serves this session.
            let mut slot = session.conn.lock().expect("session conn");
            if slot.as_ref().is_some_and(|c| Arc::ptr_eq(c, &current)) {
                *slot = None;
            }
            drop(slot);
            current.shutdown();
        }
        let (socket, handshake) = self.dial(target, headers, model, model_level_cooling, wire).await?;
        let (sink, stream) = socket.split();
        let conn = Upstream::new(target.clone(), sink, self.ws.write);
        if let Some(upgrade) = UpgradeModel::from_handshake(model, &handshake) {
            let _ = conn.upgrade_model.set(upgrade);
        }
        // Publish before the reader runs, so a reader that fails at once still finds its
        // socket current and invalidates it; holding the handle slot keeps a concurrent
        // shutdown from missing the abort handle.
        let mut reader = conn.reader.lock().expect("reader handle");
        session.publish(conn.clone());
        let task = tokio::spawn(read_loop(stream, Arc::downgrade(session), conn.clone(), self.ws.idle));
        *reader = Some(task.abort_handle());
        drop(reader);
        Ok((conn, Some(handshake)))
    }

    async fn dial(
        &self,
        target: &Target,
        request: &HeaderMap,
        model: &str,
        model_level_cooling: bool,
        wire: Option<&crate::codex_capture::Wire>,
    ) -> Result<(WebSocket, HeaderMap), ExecError> {
        // `newProxyAwareWebsocketDialer`: Go's standard dialer, environment proxies
        // included when none is configured, with `EnableCompression`.
        let mut headers = request.clone();
        let key = offer_compression(&mut headers);
        let builder = self
            .transport
            .standard(&target.proxy)
            .websocket(&target.url)
            .headers(headers)
            .accept_key(key.clone());
        let mut rejected = false;
        let attempt = async {
            let mut res = builder
                .send()
                .await
                .map_err(|e| transport("codex websockets executor: dial failed").with_diagnostic(ws_cause::dial(&e)))?;
            let status = res.status().as_u16();
            let handshake = res.headers().clone();
            if status != 101 {
                let inner = std::mem::replace(&mut *res, wreq::Response::from(http::Response::new(Vec::<u8>::new())));
                let mut body = Vec::new();
                let mut stream = inner.bytes_stream();
                while let Some(Ok(chunk)) = stream.next().await {
                    body.extend_from_slice(&chunk[..chunk.len().min(MAX_HANDSHAKE_BODY - body.len())]);
                    if body.len() >= MAX_HANDSHAKE_BODY {
                        break;
                    }
                }
                self.quota_observe(&target.credential, model, &handshake);
                rejected = true;
                if let Some(wire) = wire {
                    wire.upgrade_rejection(&target.url, request, status, &handshake, &body);
                }
                if status == 426 {
                    // Go returns a plain statusErr for 426 on downstream WebSockets.
                    let text = String::from_utf8_lossy(&body).into_owned();
                    return Err(response::status_error_raw(status, &text, handshake, false));
                }
                return Err(response::status_error(status, &body, handshake, model_level_cooling));
            }
            // ponytail: Go reports a failed negotiation (and a bad handshake) as a status
            // error carrying 101; here both are transport failures.
            let socket = upgrade(&mut res, &key, &handshake).await.map_err(|e| match e {
                UpgradeError::Handshake => transport("codex websockets executor: websocket handshake failed"),
                UpgradeError::Compression => {
                    transport("codex websockets executor: websocket: invalid compression negotiation")
                }
            })?;
            Ok((socket, handshake))
        };
        let dialed = tokio::time::timeout(HANDSHAKE_TIMEOUT, attempt)
            .await
            .unwrap_or_else(|_| {
                Err(transport("codex websockets executor: handshake timed out")
                    .with_diagnostic(ws_cause::handshake_timeout(HANDSHAKE_TIMEOUT)))
            });
        // A refused upgrade was recorded as an HTTP attempt; anything else is a dial error.
        if let (Err(error), Some(wire)) = (&dialed, wire)
            && !rejected
        {
            wire.ws_exec_error("dial", error);
        }
        dialed
    }

    fn quota_observe(&self, credential: &str, model: &str, headers: &HeaderMap) {
        self.quota().observe(credential, model, headers);
    }

    /// One Responses turn over the session's upstream socket (`ExecuteStream` on
    /// `CodexWebsocketsExecutor` with a downstream WebSocket).
    pub(crate) async fn stream_ws(
        &self,
        view: &View<'_>,
        settings: &Settings,
        req: ExecRequest,
        exec_session: &ExecSession,
        cfg: &Config,
    ) -> Result<ExecResponse, ExecError> {
        // The native WebSocket route (#24): the client's `response.create` frame goes
        // upstream with only its `model` routed. No shaping, payload rules, prompt-cache
        // key, reasoning replay, ID or namespace edits; nothing is restored on the way
        // back (`docs/codex/CONTRACT.md` "Native WebSocket route").
        if crate::codex::native::route_ws(view, &req).is_none() {
            return Err(crate::codex::native::refused_frame(&req.body));
        }
        let model = crate::codex::native::upstream_model(&req)?;
        let frame = crate::codex::native::routed(&req.body, &model)?;
        settings.refuse_image_tool(&req.request_path, &frame)?;
        // The frame passed the request boundary, so it is UTF-8. It is read in place and
        // sent as these bytes: no copy of the frame.
        let body = std::str::from_utf8(&frame)
            .map_err(|_| ExecError::local(400, FailureScope::Request, "request body is not valid UTF-8"))?;
        let cache = request::prompt_cache_id(&req, body, Some(&exec_session.id), true);
        crate::codex::report_request(&req, cpa_core::format::Format::Codex, body);
        let native = request::is_native(&req);
        let headers = request::ws_headers(
            view,
            settings,
            &req.headers,
            body,
            &model,
            cache.as_deref(),
            native,
            // A native frame carries the caller's own identity (#26).
            crate::codex_identity::Origin::Caller,
        );
        let target = Target::new(view.credential, ws_url(&format!("{}/responses", view.base_url))?, cfg);
        let session = self.ws.session(&exec_session.id);
        // `WebsocketInputFromContext(ctx) != nil && cfg.Codex.ResponseSteering`.
        let steering = settings
            .response_steering
            .then(|| session.steering())
            .flatten()
            .map(|input| (input, duplex::Prepared::new(native)));
        let guard = session.turn.clone().lock_owned().await;
        let model_kept = model_kept(&req.original_body, body);
        // A native frame is already a `response.create`.
        let wire = crate::codex_capture::Wire::new(req.capture(), view.credential)
            .with_websocket(req.websocket_emitter("codex", view.credential));
        wire.ws_request(&target.url, &headers, &frame);
        let (mut conn, mut handshake) = if exec_session.continuation {
            match session.current().filter(|c| c.target.serves(&target)) {
                Some(conn) => (conn, None),
                None => return Err(ExecError::replay_required()),
            }
        } else {
            self.ensure(
                &session,
                &target,
                &headers,
                &model,
                settings.model_level_cooling,
                Some(&wire),
            )
            .await?
        };
        conn.bind(&session, exec_session.lease.as_ref())?;
        if let Some(handshake) = &handshake {
            wire.ws_handshake(101, handshake);
        }
        let started = Instant::now();
        let preserves = steering.is_none() && request::codex_client(&req);
        let mut rx = conn.activate(preserves);
        if let Err(error) = conn.send_raw(frame.clone()).await {
            wire.ws_exec_error("send", &error);
            // `shouldRetryCodexWebsocketSend`: request-scoped failures (413) never retry.
            let retry = error.scope != FailureScope::Request;
            if exec_session.continuation {
                session.invalidate(&conn, &error, false);
                return Err(if retry { ExecError::replay_required() } else { error });
            }
            session.invalidate(&conn, &error, true);
            if !retry {
                return Err(error);
            }
            // Retry once on a fresh socket: upstream may have closed it between turns.
            let (fresh, fresh_handshake) = match self
                .ensure(&session, &target, &headers, &model, settings.model_level_cooling, None)
                .await
            {
                Ok(fresh) => fresh,
                Err(error) => {
                    wire.ws_exec_error("dial_retry", &error);
                    return Err(error);
                }
            };
            fresh.bind(&session, exec_session.lease.as_ref())?;
            rx = fresh.activate(preserves);
            wire.ws_request(&target.url, &headers, &frame);
            if let Some(handshake) = &fresh_handshake {
                wire.ws_handshake(101, handshake);
            }
            if let Err(error) = fresh.send_raw(frame).await {
                wire.ws_exec_error("send_retry", &error);
                session.invalidate(&fresh, &error, true);
                return Err(error);
            }
            conn = fresh;
            handshake = fresh_handshake;
        }
        let observed = handshake.clone().unwrap_or_default();
        if !observed.is_empty() {
            self.quota_observe(&view.credential.id, &model, &observed);
        }
        // Read from the socket that serves the turn, after any retry replaced it.
        let returned = match handshake {
            Some(handshake) => returned_handshake(handshake, model_kept),
            None => conn.later_metadata(&model, model_kept),
        };
        let turn = Turn {
            rx,
            conn,
            session,
            _turn: Some(guard),
            observing: None,
            codex_client: request::codex_client(&req),
            cooling: settings.model_level_cooling,
            quota: self.quota_handle(),
            credential: view.credential.id.clone(),
            observed,
            usage: req.usage.clone(),
            model: model.clone(),
            wire,
            ack_wait: self.ws.ack_wait,
            drain: None,
        };
        if let Some((input, initial)) = steering {
            // The socket now belongs to this connection; bootstrap buffering does not apply.
            let stream = duplex::Duplex::start(turn, input, initial, req, view, settings.image_generation).await;
            return Ok(ExecResponse {
                status: 200,
                headers: returned,
                body: ResponseBody::Stream(stream),
            });
        }
        let stream = if settings.bootstrap_buffering {
            bootstrap(turn, settings.bootstrap_timeout, started).await?
        } else {
            rest(turn)
        };
        Ok(ExecResponse {
            status: 200,
            headers: returned,
            body: ResponseBody::Stream(stream),
        })
    }
}

/// Codex's handshake header naming the model that served the request.
const OPENAI_MODEL: &str = "openai-model";
/// Codex's sticky-routing token for the turn.
const TURN_STATE: &str = "x-codex-turn-state";

/// Whether the frame sent upstream names the model the client asked for. Codex compares
/// `openai-model` with the model it sent, ignoring ASCII case.
fn model_kept(original: &[u8], upstream: &str) -> bool {
    use cpa_common::json::{Kind, get};
    let (original, upstream) = (get(original, "model"), get(upstream.as_bytes(), "model"));
    original.kind == Kind::String && upstream.kind == Kind::String && original.s.eq_ignore_ascii_case(&upstream.s)
}

/// The handshake of a turn that opened its socket, as the server sees it. Codex treats
/// an `openai-model` that differs from the model it sent as a reroute and warns the user
/// that the account was flagged. When this proxy changed the model (an alias or a
/// payload rule), upstream's answer describes that change, so it is dropped. A turn on a
/// reused socket gets [`Upstream::later_metadata`] instead.
fn returned_handshake(mut handshake: HeaderMap, model_kept: bool) -> HeaderMap {
    if !model_kept {
        handshake.remove(OPENAI_MODEL);
    }
    handshake
}

/// Upstream handshake metadata as a `response.metadata` event that a Codex client reads
/// from its `headers` (`codex-api/src/sse/responses.rs:193-222`). The downstream socket
/// was upgraded before the first request chose a credential and a model, so these headers
/// cannot go into its handshake. `headers` is what the executor returned for the turn:
/// the opening upgrade's headers on the turn that opened the socket, which may carry
/// `x-codex-turn-state` and `openai-model`, or only `openai-model` on a later request
/// ([`Upstream::later_metadata`]). Codex reads `x-reasoning-included` only from a
/// handshake, so it is not carried. Only the headers named here are carried; `None`
/// when `headers` has none of them.
pub fn handshake_metadata_event(handshake: &HeaderMap) -> Option<String> {
    let mut headers = serde_json::Map::new();
    for name in [TURN_STATE, OPENAI_MODEL] {
        if let Some(value) = metadata_value(handshake, name) {
            headers.insert(name.to_owned(), value.into());
        }
    }
    if headers.is_empty() {
        return None;
    }
    let event = serde_json::json!({"type": "response.metadata", "headers": headers});
    Some(event.to_string())
}

/// A metadata header's value as the event carries it: valid text, trimmed, not empty.
fn metadata_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

/// tungstenite's reports of a peer that went away without a close frame, which
/// gorilla/websocket reads as close 1006 "unexpected EOF".
fn ends_without_close(error: &tokio_tungstenite::tungstenite::Error) -> bool {
    use tokio_tungstenite::tungstenite::Error;
    use tokio_tungstenite::tungstenite::error::ProtocolError;
    match error {
        Error::Protocol(ProtocolError::ResetWithoutClosingHandshake) => true,
        Error::Io(io) => io.kind() == std::io::ErrorKind::UnexpectedEof,
        _ => false,
    }
}

/// The socket's single reader. Exits on error, idle timeout, or abort.
///
/// Like Go's read deadline, `idle` starts when the read for the next application message
/// begins; control frames answered meanwhile do not extend it.
pub(crate) async fn read_loop(
    mut stream: SplitStream<WebSocket>,
    session: Weak<Session>,
    conn: Arc<Upstream>,
    idle: Duration,
) {
    // The last text delivered is a context-window failure for a turn that preserves it.
    let mut held = false;
    let error = 'read: loop {
        let deadline = tokio::time::Instant::now() + idle;
        let text = loop {
            let message = match tokio::time::timeout_at(deadline, stream.next()).await {
                Err(_) => {
                    break 'read transport("codex websockets executor: read idle timeout")
                        .with_diagnostic(ws_cause::idle_timeout(idle));
                }
                // gorilla/websocket reports a connection that ends without a close frame
                // as close 1006; Go surfaces its text, which marks a lifecycle failure.
                // The diagnostic keeps what actually happened.
                Ok(None) => {
                    break 'read transport(&close_error_text(1006, "unexpected EOF"))
                        .with_diagnostic(ws_cause::stream_ended());
                }
                Ok(Some(Err(error))) if ends_without_close(&error) => {
                    break 'read transport(&close_error_text(1006, "unexpected EOF"))
                        .with_diagnostic(ws_cause::socket(&error));
                }
                Ok(Some(Err(error))) => {
                    break 'read transport("codex websockets executor: read failed")
                        .with_diagnostic(ws_cause::socket(&error));
                }
                Ok(Some(Ok(message))) => message,
            };
            match message {
                Message::Text(text) => break text.as_str().to_owned(),
                Message::Binary(_) => break 'read transport(UNEXPECTED_BINARY),
                Message::Close(frame) => {
                    let code = frame.as_ref().map(|f| u16::from(f.code));
                    let diagnostic = ws_cause::remote_close(code);
                    if let Some(frame) = frame.as_ref().filter(|f| u16::from(f.code) == 1009) {
                        break 'read closed_too_big(frame.reason.as_str()).with_diagnostic(diagnostic);
                    }
                    let text = match frame {
                        Some(frame) => close_error_text(u16::from(frame.code), frame.reason.as_str()),
                        None => close_error_text(1005, ""),
                    };
                    break 'read transport(&text).with_diagnostic(diagnostic);
                }
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
            }
        };
        // Terminal IDs are recorded before delivery: the handler may still be forwarding
        // the turn after its read channel was released.
        let terminal = terminal_response_id(text.trim());
        let acknowledged = acknowledged_id(text.trim());
        let (active, preserves) = {
            let mut link = conn.link.lock().expect("link");
            if let Some(id) = terminal {
                let turn = link.turns;
                link.terminal.insert(id, turn);
            }
            match acknowledged {
                // A late interrupt's caller waits for this one.
                Some(id) if link.late.as_ref().is_some_and(|(late, _)| *late == id) => {
                    if let Some((_, tx)) = link.late.take() {
                        let _ = tx.send(text);
                    }
                    continue 'read;
                }
                // Only the reading turn's own interrupts are acknowledged to it: Codex
                // fails a response on an acknowledgement for another one, and one that
                // outlived its wait must not reach the next turn.
                Some(id) if !link.awaited.contains(&id) => continue 'read,
                _ => (link.active.clone(), link.preserves),
            }
        };
        held = false;
        if let Some(tx) = active {
            let failure = preserves && cpa_common::codex_client::is_context_window_rejection(text.trim().as_bytes());
            // Waits while the turn is busy: backpressure instead of unbounded queueing.
            held = tx.send(Read::Text(text)).await.is_ok() && failure;
        }
    };
    let (active, defer) = {
        let mut link = conn.link.lock().expect("link");
        link.lost = Some(error.clone());
        // A late interrupt's caller stops waiting and finds the loss.
        link.late = None;
        // The turn has yet to give its client the context-window failure it was handed:
        // reporting the loss now would close the client before the event or replace its
        // classified error (Go v8.0.35 replaces the event anyway). The turn decides.
        let defer = held && link.preserves && link.active.is_some();
        if defer {
            link.deferred = Some(error.clone());
        }
        (link.active.clone(), defer)
    };
    // Otherwise the session records the loss before the turn reads its error, so a
    // handler that sees the turn fail also finds the loss (Go's notifier closes the
    // client first).
    match session.upgrade() {
        Some(session) => session.invalidate(&conn, &error, !defer),
        None => conn.deactivate(),
    }
    if let Some(tx) = active {
        let _ = tx.try_send(Read::Failed(turn_error(&error)));
    }
}

/// Why [`upgrade`] refused a 101 answer.
pub(crate) enum UpgradeError {
    /// gorilla `Dial`'s checks failed (Upgrade, Connection, Sec-WebSocket-Accept) or the
    /// connection could not be taken over.
    Handshake,
    /// The permessage-deflate answer does not match the offer.
    Compression,
}

/// gorilla `Dialer` with `EnableCompression`: offers permessage-deflate on the request
/// and returns the `Sec-WebSocket-Key` to send with it. Shared by the Codex and xAI
/// WebSocket executors (Go's xAI dialer also enables compression).
pub(crate) fn offer_compression(headers: &mut HeaderMap) -> String {
    headers.insert(
        http::header::SEC_WEBSOCKET_EXTENSIONS,
        http::HeaderValue::from_static(deflate::OFFER),
    );
    tokio_tungstenite::tungstenite::handshake::client::generate_key()
}

/// gorilla `Dial`'s checks on a 101 answer, then its permessage-deflate agreement;
/// wraps the upgraded connection for [`Upstream`] and [`read_loop`].
pub(crate) async fn upgrade(
    res: &mut wreq::ws::WebSocketResponse,
    key: &str,
    handshake: &HeaderMap,
) -> Result<WebSocket, UpgradeError> {
    let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
    if !token_list_contains(handshake, http::header::UPGRADE, "websocket")
        || !token_list_contains(handshake, http::header::CONNECTION, "upgrade")
        || handshake.get(http::header::SEC_WEBSOCKET_ACCEPT).map(|v| v.as_bytes()) != Some(accept.as_bytes())
    {
        return Err(UpgradeError::Handshake);
    }
    let compressed = deflate::negotiated(handshake).map_err(|()| UpgradeError::Compression)?;
    let response = std::mem::replace(&mut **res, wreq::Response::from(http::Response::new(Vec::<u8>::new())));
    let upgraded = response.upgrade().await.map_err(|_| UpgradeError::Handshake)?;
    let config = WebSocketConfig::default()
        .max_frame_size(Some(MAX_UPSTREAM_MESSAGE))
        .max_message_size(Some(MAX_UPSTREAM_MESSAGE));
    Ok(WebSocketStream::from_raw_socket(
        Inflate::new(upgraded, compressed, MAX_UPSTREAM_MESSAGE),
        Role::Client,
        Some(config),
    )
    .await)
}

/// gorilla `tokenListContainsValue`: a comma-separated token list holds `value`
/// (ASCII case-insensitive).
fn token_list_contains(headers: &HeaderMap, name: http::HeaderName, value: &str) -> bool {
    headers.get_all(name).iter().any(|v| {
        v.to_str().is_ok_and(|v| {
            v.split(',')
                .any(|t| t.trim_matches([' ', '\t']).eq_ignore_ascii_case(value))
        })
    })
}

#[path = "codex_ws_deflate.rs"]
pub(crate) mod deflate;
#[path = "codex_duplex.rs"]
mod duplex;
pub use duplex::{ClientFrames, SteeringInput};

#[cfg(test)]
#[path = "codex_ws_tests.rs"]
mod tests;

#[cfg(test)]
mod close_text_tests {
    use super::close_error_text;

    /// gorilla/websocket `CloseError.Error()` texts, which Go returns unchanged.
    #[test]
    fn close_errors_read_like_gorilla() {
        assert_eq!(close_error_text(1000, ""), "websocket: close 1000 (normal)");
        assert_eq!(close_error_text(1001, "bye"), "websocket: close 1001 (going away): bye");
        assert_eq!(
            close_error_text(1006, "unexpected EOF"),
            "websocket: close 1006 (abnormal closure): unexpected EOF"
        );
        assert_eq!(close_error_text(4000, "x"), "websocket: close 4000: x");
    }
}
