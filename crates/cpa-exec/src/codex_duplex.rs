//! Response steering: a full-duplex Codex WebSocket (codex_websockets_duplex.go).
//!
//! With `response-steering` on, a downstream Responses WebSocket whose turn runs on a
//! Codex credential in WebSocket mode keeps that upstream socket for the rest of the
//! connection. Once the first `response.created` reached the client, the client's later
//! frames go straight to the socket: `response.steer` raw and `response.create` with
//! only its model routed. `response.append` is refused. A response terminal is not a
//! stream terminal: accepted steering may start an automatic successor or wait for tool results. The
//! stream ends with the socket, and the socket with the stream. There is no redial,
//! credential switch, local acknowledgement or replay here; only upstream owns a
//! steering submission.
//!
//! Go runs a writer and a reader goroutine around shared state. Here one task owns the
//! state and selects fairly between upstream events, client frames and delivery; a
//! separate writer task owns the socket's writes, so a write blocked in the network never
//! stops upstream events (Go's writer blocking in `writeCodexWebsocketMessage`). Writer
//! waits (`waitFor`, queued creates) become state the task re-checks after every step.
//!
//! Usage is recorded per upstream response, not per connection (#25). The attempt that
//! opened the socket accounts for the bootstrap create until its `response.created`; then
//! its accounting moves to that response's own record (`UsageSink::detach`), and the
//! attempt publishes nothing more. Every later create sent upstream, and every automatic
//! successor upstream starts, gets a record of its own (`UsageSink::successor`), bound to
//! its response ID once it starts. A record settles once, as soon as its outcome is
//! observed, while the connection stays open: its terminal event (with the usage that
//! event reported, or none), its own failure, or a rejection before it started. Settled
//! records are never revisited. When the connection ends, only records still open
//! settle: as the connection's failure, or as the proxy's cancellation (`crate::cancel`)
//! when the client left. A frame refused here, or a create still queued here, never went
//! upstream and gets no record.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use bytes::Bytes;
use cpa_common::codex_body::CodexBody;
use cpa_core::credential::Credential;
use cpa_core::exec::{ExecError, ExecRequest, ExecStream, FailureScope, UsageSink};
use cpa_core::format::Format;
use futures_util::{FutureExt, StreamExt};
use tokio::sync::{OwnedMutexGuard, mpsc};

use super::{Read, Turn, Upstream, transport, ws_error};
use crate::codex_request::View;
use crate::codex_response as response;
use crate::rawjson::{self, Kind};

/// Bound on queued and outstanding `response.create` requests.
const MAX_PENDING: usize = 16;
/// Responses whose settings are retained for automatic successors and appends.
const RETAINED_RESPONSES: usize = 16;
/// Frames accepted for the writer and not yet written. Client frames are read only while
/// there is room, so a stalled upstream write backpressures the client as Go's does.
const WRITE_QUEUE: usize = 16;
/// Local items (events, rejections) waiting for the consumer. Upstream is read only when
/// none wait, as Go's reader blocks on its unbuffered `out`.
const OUTBOX: usize = 16;

type Enabled = dyn Fn(&str) -> bool + Send + Sync;

/// The queue of a downstream connection's client frames, in arrival order. A bounded
/// channel backpressures the client; the connection's own loop reads it between turns.
pub type ClientFrames = Arc<tokio::sync::Mutex<mpsc::Receiver<Bytes>>>;

/// A downstream connection's later client frames (Go `WebsocketInput`) and the live
/// account check for its bound credential (Go `WebsocketAuthCheck`).
pub struct SteeringInput {
    frames: ClientFrames,
    enabled: Box<Enabled>,
}

impl SteeringInput {
    /// `frames` is the connection's frame queue, shared with its loop as Go hands both
    /// the same channel; a duplex turn holds it for the rest of the connection.
    /// `enabled(credential_id)` says whether the bound credential may still send.
    pub fn new(frames: ClientFrames, enabled: impl Fn(&str) -> bool + Send + Sync + 'static) -> Self {
        Self {
            frames,
            enabled: Box::new(enabled),
        }
    }
}

/// What the duplex reads of one request (Go `codexWebsocketPrepared`). Every request on
/// a duplex connection is a native frame (#24, #28): sent as the client wrote it, with
/// only `model` routed, and its events reach the client as upstream sent them.
#[derive(Clone, Default)]
pub(super) struct Prepared {
    /// Native Codex request (the Responses Lite marker).
    native: bool,
    /// A usage record of the request, whose facts (model, reasoning effort, service
    /// tier) an automatic successor of its response takes. Reports nothing until the
    /// attempt is detached.
    usage: UsageSink,
}

impl Prepared {
    pub(super) fn new(native: bool) -> Self {
        Self {
            native,
            usage: UsageSink::default(),
        }
    }

    /// The settings a response retains.
    fn retained(&self) -> Self {
        self.clone()
    }
}

/// One upstream response's usage record on this connection: from the create that asked
/// for it (or the `response.created` of an automatic successor) until it settles, once.
/// Dropped unsettled, because the connection ended under it without a cause of its own
/// (the client left), it records the proxy's cancellation (`crate::cancel`).
struct Accounting {
    usage: UsageSink,
    settled: bool,
}

impl Accounting {
    fn new(usage: UsageSink) -> Self {
        Self { usage, settled: false }
    }

    fn observe(&self, payload: &str) {
        if self.usage.enabled() {
            self.usage.response_line(Format::Codex, payload.as_bytes());
        }
    }

    /// The upstream ended the response: the record has the usage its terminal event
    /// reported, or none.
    fn complete(mut self) {
        self.settled = true;
        self.usage.publish();
    }

    /// The response failed, or the connection failed under it.
    fn fail(mut self, error: &ExecError) {
        self.settled = true;
        crate::openai_http::publish_failure(&self.usage, error);
    }

    /// The client left while the response ran or waited to start.
    fn cancel(mut self) {
        self.settled = true;
        self.usage.publish_failure(crate::cancel::STATUS, crate::cancel::BODY);
    }
}

impl Drop for Accounting {
    fn drop(&mut self) {
        if !self.settled {
            self.usage.publish_failure(crate::cancel::STATUS, crate::cancel::BODY);
        }
    }
}

/// A create sent upstream and not yet started or rejected.
struct Pending {
    prepared: Prepared,
    /// `None` for the bootstrap create, which the attempt accounts for until it starts,
    /// and on a connection whose attempt keeps its own accounting.
    accounting: Option<Accounting>,
}

/// A started response whose record has not settled.
struct Open {
    id: String,
    accounting: Accounting,
}

/// Events that carry the `response` object, whose `response.id` names their response.
const RESPONSE_EVENTS: [&str; 7] = [
    "response.created",
    "response.queued",
    "response.in_progress",
    "response.completed",
    "response.incomplete",
    "response.failed",
    "response.done",
];

fn response_terminal(kind: &str) -> bool {
    matches!(kind, "response.completed" | "response.done" | "response.incomplete")
}

/// The duplex stream's state: Go's writer and reader goroutines' shared metadata.
/// An upstream message whose observation is running (Go's reader inside
/// `EmitWebSocketResponseEvent`): the rest of its handling waits for it.
struct Observing {
    observation: futures_util::future::BoxFuture<'static, ()>,
    payload: String,
    kind: String,
    establishing: bool,
}

pub(super) struct Duplex {
    turn: Turn,
    /// While set, no further upstream message is read; client input and the writer
    /// keep going, as Go's writer goroutine does while the reader waits.
    observing: Option<Observing>,
    /// A writer-side failure raised while a message was observed. Go's writer only
    /// records its error; the reader finishes the observer and its message first, then
    /// reports it. Client input and writes stop meanwhile.
    failing: Option<ExecError>,
    frames: OwnedMutexGuard<mpsc::Receiver<Bytes>>,
    input: Arc<SteeringInput>,
    /// The bootstrap request: the template for later creates (Go `req`, `opts`).
    req: ExecRequest,
    credential: Credential,
    /// `multimedia.disable-image-generation`: a later frame carrying the tool it disables
    /// is refused like the bootstrap create.
    image_generation: cpa_common::payload::ImageGeneration,
    initial: Prepared,
    /// The bootstrap request's client model (Go `initial.originalPayload` `model`).
    initial_model: String,
    /// Creates sent upstream and not yet started or rejected, oldest first.
    pending: VecDeque<Pending>,
    /// Started responses whose records have not settled, oldest first; at most
    /// [`RETAINED_RESPONSES`].
    open: VecDeque<Open>,
    /// The attempt's accounting moved to per-response records. Until then, or when the
    /// attempt's observer keeps no such records, every event feeds the attempt's own.
    detached: bool,
    /// The client's frames ended: what still runs is canceled, not failed.
    client_gone: bool,
    /// Parents of steers written and not yet acknowledged.
    unacknowledged: Vec<String>,
    /// Accepted steer id to parent response.
    accepted: HashMap<String, String>,
    current: Prepared,
    response_id: String,
    response_settings: HashMap<String, Prepared>,
    response_order: VecDeque<String>,
    /// Settings pinned by in-flight steering, independent of the retention window.
    steering_settings: HashMap<String, Prepared>,
    /// The response waiting for required input (`response.steer.pending`).
    waiting_parent: String,
    /// Upstream started a successor on its own.
    automatic_active: bool,
    /// Client creates waiting until steering settles.
    queued_creates: VecDeque<String>,
    /// A steer whose parent settings are unknown waits for pending creates to start.
    deferred_steer: Option<(String, String)>,
    /// Client frames are read only after the first `response.created` was delivered.
    input_ready: bool,
    first_response: bool,
    response_active: bool,
    /// Items for the consumer, in order; `out` takes one at a time.
    outbox: VecDeque<Result<Bytes, ExecError>>,
    out: mpsc::Sender<Result<Bytes, ExecError>>,
    /// The writer task's queue and handle; the task ends with the first write error.
    writes: mpsc::Sender<Outgoing>,
    writer: tokio::task::JoinHandle<Result<(), ExecError>>,
    done: bool,
}

impl Drop for Duplex {
    /// The socket ends with the stream; the handler already knows (no notify). Closing
    /// it releases a writer blocked in the network, which then stops (Go joins it).
    fn drop(&mut self) {
        self.turn
            .invalidate(&transport("codex websockets executor: duplex closed"), false);
        self.writer.abort();
    }
}

/// One frame for the writer task.
struct Outgoing {
    frame: String,
}

/// The network half of Go's writer goroutine: frames in order, each after the live
/// account check (`WebsocketAuthEnabled`) right before `writeCodexWebsocketMessage`.
async fn write_loop(
    conn: Arc<Upstream>,
    input: Arc<SteeringInput>,
    credential: String,
    wire: crate::codex_capture::Wire,
    mut queue: mpsc::Receiver<Outgoing>,
) -> Result<(), ExecError> {
    while let Some(Outgoing { frame }) = queue.recv().await {
        if !(input.enabled)(&credential) {
            return Err(local("websocket credential is no longer enabled"));
        }
        wire.ws_frame(&conn.target.url, frame.as_bytes());
        conn.send(frame).await?;
    }
    Ok(())
}

/// A failure of the bound socket: request-scoped, so the credential is not cooled
/// (Go `codexDuplexConnectionError`). Status and body are kept for the handler.
fn connection(mut error: ExecError) -> ExecError {
    error.scope = FailureScope::Request;
    error
}

fn local(message: &str) -> ExecError {
    connection(transport(message))
}

enum Step {
    Upstream(Option<Read>),
    Observed,
    Client(Option<Bytes>),
    Writer(Result<Result<(), ExecError>, tokio::task::JoinError>),
}

impl Duplex {
    /// `streamCodexDuplex`, after the bootstrap `response.create` was written.
    pub(super) async fn start(
        turn: Turn,
        input: Arc<SteeringInput>,
        initial: Prepared,
        req: ExecRequest,
        view: &View<'_>,
        image_generation: cpa_common::payload::ImageGeneration,
    ) -> ExecStream {
        // Turns on a session are serialised, so no other stream holds the frames.
        let frames = input.frames.clone().lock_owned().await;
        let original = CodexBody::parse(&req.original_body).ok();
        let initial_model = original
            .as_ref()
            .and_then(|b| b.get("model"))
            .and_then(|m| m.text())
            .unwrap_or_default()
            .into_owned();
        let (writes, queue) = mpsc::channel(WRITE_QUEUE);
        let writer = tokio::spawn(write_loop(
            turn.conn.clone(),
            input.clone(),
            view.credential.id.clone(),
            turn.wire.clone(),
            queue,
        ));
        // Go's `out` is unbuffered; one slot lets the task hand over and keep reading.
        let (out, chunks) = mpsc::channel(1);
        let duplex = Self {
            turn,
            observing: None,
            failing: None,
            frames,
            input,
            req,
            credential: view.credential.clone(),
            image_generation,
            current: initial.clone(),
            pending: VecDeque::from([Pending {
                prepared: initial.clone(),
                accounting: None,
            }]),
            open: VecDeque::new(),
            detached: false,
            client_gone: false,
            initial,
            initial_model,
            unacknowledged: Vec::new(),
            accepted: HashMap::new(),
            response_id: String::new(),
            response_settings: HashMap::new(),
            response_order: VecDeque::new(),
            steering_settings: HashMap::new(),
            waiting_parent: String::new(),
            automatic_active: false,
            queued_creates: VecDeque::new(),
            deferred_steer: None,
            input_ready: false,
            first_response: true,
            response_active: false,
            outbox: VecDeque::new(),
            out,
            writes,
            writer,
            done: false,
        };
        // The task runs whether or not the consumer polls, as Go's goroutines do, and
        // ends when the consumer drops the stream.
        tokio::spawn(duplex.run());
        futures_util::stream::unfold(chunks, |mut chunks| async move {
            chunks.recv().await.map(|item| (item, chunks))
        })
        .boxed()
    }

    async fn run(mut self) {
        self.serve().await;
        // An observation that already started finishes before the turn (and its
        // session) is released.
        if let Some(observing) = self.observing.take() {
            observing.observation.await;
            // The upstream ended that response, though nobody reads the stream any more.
            self.settle_terminal(&observing.payload, &observing.kind);
        }
        // Records still open settle as the client's cancellation when `self` drops.
    }

    async fn serve(&mut self) {
        loop {
            if self.input_ready && !self.done && self.failing.is_none() {
                self.progress();
            }
            let deliver = !self.outbox.is_empty();
            if self.done && !deliver {
                return;
            }
            // After progress(): a failure it raised stops client input at once.
            let live = !self.done && self.failing.is_none();
            let writable = self.writes.capacity() > 0;
            let read_client =
                live && self.input_ready && self.deferred_steer.is_none() && writable && self.outbox.len() < OUTBOX;
            let await_room = live && self.input_ready && !writable && !self.writes.is_closed();
            let observing = self.observing.is_some();
            let pending_observation = &mut self.observing;
            let step = tokio::select! {
                permit = self.out.reserve(), if deliver => match permit {
                    Ok(permit) => {
                        permit.send(self.outbox.pop_front().expect("an item to deliver"));
                        continue;
                    }
                    Err(_) => return,
                },
                () = self.out.closed(), if !deliver => return,
                read = self.turn.rx.recv(), if !self.done && !deliver && !observing => Step::Upstream(read),
                () = async {
                    if let Some(observing) = pending_observation.as_mut() {
                        (&mut observing.observation).await;
                    }
                }, if observing => Step::Observed,
                frame = self.frames.recv(), if read_client => Step::Client(frame),
                ended = &mut self.writer, if live => Step::Writer(ended),
                // The writer freed room: re-check progress and client reads.
                _ = self.writes.reserve(), if await_room => continue,
            };
            match step {
                Step::Upstream(read) => self.upstream(read),
                Step::Observed => {
                    let Observing {
                        payload,
                        kind,
                        establishing,
                        ..
                    } = self.observing.take().expect("an observation finished");
                    self.observed(&payload, &kind, establishing);
                    // The deferred failure follows the message, unless the message
                    // itself ended the stream.
                    if let Some(error) = self.failing.take()
                        && !self.done
                    {
                        self.fail(error);
                    }
                }
                Step::Client(frame) => self.client(frame),
                Step::Writer(ended) => self.fail(match ended {
                    Ok(Err(error)) => error,
                    Ok(Ok(())) | Err(_) => local("websocket writer stopped"),
                }),
            }
        }
    }

    /// Go's reader reports the writer's failure in place of its own read error (the
    /// writer's cancel is what ended the read).
    fn writer_error(&mut self) -> Option<ExecError> {
        if self.done || !self.writer.is_finished() {
            return None;
        }
        match (&mut self.writer).now_or_never()? {
            Ok(Err(error)) => Some(error),
            Ok(Ok(())) | Err(_) => None,
        }
    }

    fn fail(&mut self, error: ExecError) {
        if self.observing.is_some() {
            self.failing.get_or_insert(error);
            return;
        }
        let error = connection(error);
        if self.client_gone {
            self.cancel_records();
        } else {
            self.fail_records(&error);
        }
        self.outbox.push_back(Err(error));
        self.done = true;
    }

    /// The stream ends with `error`: every response still running, and every create
    /// upstream has not started, fails with it. Settled records stay as they are.
    fn fail_records(&mut self, error: &ExecError) {
        for open in self.open.drain(..) {
            open.accounting.fail(error);
        }
        for pending in &mut self.pending {
            if let Some(accounting) = pending.accounting.take() {
                accounting.fail(error);
            }
        }
    }

    /// The client left: every response still running, and every create upstream has not
    /// started, is canceled. Settled records stay as they are.
    fn cancel_records(&mut self) {
        for open in self.open.drain(..) {
            open.accounting.cancel();
        }
        for pending in &mut self.pending {
            if let Some(accounting) = pending.accounting.take() {
                accounting.cancel();
            }
        }
    }

    /// Feeds one upstream event to the usage record it belongs to: the attempt's own
    /// until the attempt is detached, then the open response the event names (or the
    /// latest). An event of no open response feeds none.
    fn account(&self, payload: &str, kind: &str) {
        if !self.detached {
            if self.turn.usage.enabled() {
                self.turn.usage.response_line(Format::Codex, payload.as_bytes());
            }
            return;
        }
        if let Some(i) = self.open_index(payload, kind) {
            self.open[i].accounting.observe(payload);
        }
    }

    /// The open response an upstream event belongs to: the latest one with the
    /// `response.id` a response event names; any other event, or one without an ID,
    /// belongs to the latest. A response event naming no open response (a duplicate
    /// terminal event, say) belongs to none.
    fn open_index(&self, payload: &str, kind: &str) -> Option<usize> {
        let latest = self.open.len().checked_sub(1)?;
        if !RESPONSE_EVENTS.contains(&kind) {
            return Some(latest);
        }
        let id = rawjson::get(payload, "response.id");
        if id.kind() != Kind::String || id.str().is_empty() {
            return Some(latest);
        }
        self.open.iter().rposition(|open| open.id == id.str())
    }

    /// A terminal event settles its response's record with what it reported.
    fn settle_terminal(&mut self, payload: &str, kind: &str) {
        if !response_terminal(kind) {
            return;
        }
        if let Some(open) = self.open_index(payload, kind).and_then(|i| self.open.remove(i)) {
            open.accounting.complete();
        }
    }

    /// A started response gets its record. Records stay bounded: past
    /// [`RETAINED_RESPONSES`], the oldest response still open, which upstream left
    /// without a terminal event while it started that many others, settles as failed.
    // ponytail: a response upstream never ends counts as failed only once it is that old;
    // a per-response deadline would need a timer this task does not have.
    fn track(&mut self, id: String, accounting: Accounting) {
        if self.open.len() >= RETAINED_RESPONSES
            && let Some(oldest) = self.open.pop_front()
        {
            oldest
                .accounting
                .fail(&local("response superseded without a terminal event"));
        }
        self.open.push_back(Open { id, accounting });
    }

    fn enabled(&self) -> bool {
        (self.input.enabled)(&self.credential.id)
    }

    /// Go's local `reject`: an `error` event; the socket stays usable.
    fn reject(&mut self, message: &str) {
        let message = cpa_common::json::quote(message);
        let mut payload = br#"{"error":{"message":"#.to_vec();
        payload.extend_from_slice(&message);
        payload.extend_from_slice(br#","type":"invalid_request_error"},"status":400,"type":"error"}"#);
        self.outbox.push_back(Ok(Bytes::from(payload)));
    }

    /// `readyForCreate`: no unacknowledged steer, no automatic successor running, and
    /// every accepted steer waits on the response the create continues.
    fn ready_for_create(&self) -> bool {
        self.unacknowledged.is_empty()
            && !self.automatic_active
            && self.accepted.values().all(|parent| *parent == self.waiting_parent)
    }

    /// Writer progress after a state change: a deferred steer once pending creates have
    /// started, then queued creates while steering allows them (`flushPendingCreates`).
    /// Each step needs room in the write queue (Go's writer is blocked meanwhile).
    fn progress(&mut self) {
        if let Some((payload, parent)) = self.deferred_steer.take() {
            if !self.pending.is_empty() || self.writes.capacity() == 0 {
                self.deferred_steer = Some((payload, parent));
                return;
            }
            self.steer(payload, parent);
        }
        while !self.done
            && self.failing.is_none()
            && !self.queued_creates.is_empty()
            && self.ready_for_create()
            && self.writes.capacity() > 0
            && self.outbox.len() < OUTBOX
        {
            let payload = self.queued_creates.pop_front().expect("non-empty");
            self.create(payload);
        }
    }

    /// Hands a frame to the writer task; callers checked for room.
    fn write(&mut self, frame: String) {
        match self.writes.try_send(Outgoing { frame }) {
            Ok(()) => {}
            // The writer stopped on an error it reports through its handle.
            Err(mpsc::error::TrySendError::Closed(_)) => {}
            Err(mpsc::error::TrySendError::Full(_)) => self.fail(local("websocket write queue is full")),
        }
    }

    /// One client frame (the writer's input loop).
    fn client(&mut self, frame: Option<Bytes>) {
        let Some(frame) = frame else {
            // The downstream reader stopped: Go's writer fails with `context.Canceled`,
            // a connection error that does not cool the credential. Responses still
            // running are the client's cancellation, not this error.
            self.client_gone = true;
            self.fail(local("context canceled"));
            return;
        };
        if !self.enabled() {
            // A fresh connection can select an enabled credential; this frame is neither
            // sent on a disabled account nor replayed elsewhere.
            self.fail(local("websocket credential is no longer enabled"));
            return;
        }
        // The native route reads client frames through the request boundary: strict
        // UTF-8 and JSON, the bytes kept as they are.
        let body = match CodexBody::parse(&frame) {
            Ok(body) => body,
            Err(error) => {
                self.reject(&error.to_string());
                return;
            }
        };
        let kind = body.get("type").and_then(|m| m.text()).unwrap_or_default();
        let payload = String::from_utf8_lossy(&frame).into_owned();
        match kind.as_ref() {
            "response.steer" => {
                let parent = body
                    .get("previous_response_id")
                    .and_then(|m| m.text())
                    .unwrap_or_default()
                    .into_owned();
                if !self.response_settings.contains_key(&parent) && !self.pending.is_empty() {
                    // `waitFor(len(pending) == 0)`: its settings may belong to a create
                    // upstream has not started yet.
                    self.deferred_steer = Some((payload, parent));
                    return;
                }
                self.steer(payload, parent);
            }
            "response.create" | "response.append" => {
                if self.queued_creates.is_empty() && self.ready_for_create() {
                    self.create(payload);
                } else if self.queued_creates.len() >= MAX_PENDING {
                    self.fail(local("too many outstanding response.create requests"));
                } else {
                    self.queued_creates.push_back(payload);
                }
            }
            other => self.reject(&format!("unsupported websocket request type: {other}")),
        }
    }

    /// A steer goes upstream as the client wrote it. The pinned client never sends one;
    /// OpenAI documents `response.steer` for the GPT-6 family on the API, and nothing
    /// documents it on the ChatGPT backend. The upstream decides; the proxy adds nothing.
    fn steer(&mut self, payload: String, parent: String) {
        if !self.image_tool_allowed(&payload) {
            return;
        }
        if let Some(settings) = self.response_settings.get(&parent).cloned() {
            self.steering_settings.insert(parent.clone(), settings);
        }
        self.unacknowledged.push(parent);
        self.write(payload);
    }

    /// Whether `payload` may go upstream under `multimedia.disable-image-generation`; a
    /// frame carrying the disabled tool gets a local error event instead.
    fn image_tool_allowed(&mut self, payload: &str) -> bool {
        match cpa_common::payload::disabled_image_tool(
            self.image_generation,
            &self.req.request_path,
            payload.as_bytes(),
        ) {
            None => true,
            Some(field) => {
                let error = crate::codex_request::image_tool_refused(field);
                self.reject(&String::from_utf8_lossy(&error.body));
                false
            }
        }
    }

    /// A `response.create` on the native route, sent while the connection's duplex
    /// stream runs. The pinned client steers this way: it interrupts the running response
    /// and sends a new create (`codex-api/src/endpoint/responses_websocket.rs:310-325`,
    /// 824-839). The frame goes upstream as the client wrote it; only `model` is routed to
    /// this socket's upstream model. A create for another model cannot use this socket
    /// and asks the client to replay, as on the legacy path.
    fn create(&mut self, payload: String) {
        let body = match CodexBody::parse(payload.as_bytes()) {
            Ok(body) => body,
            Err(error) => {
                self.fail(error.into());
                return;
            }
        };
        if body.get("type").and_then(|m| m.text()).as_deref() == Some("response.append") {
            self.reject(crate::codex::native::APPEND_REFUSED);
            return;
        }
        if !self.image_tool_allowed(&payload) {
            return;
        }
        let parent = body
            .get("previous_response_id")
            .and_then(|m| m.text())
            .unwrap_or_default();
        let parent = parent.trim();
        if !self.accepted.is_empty() && parent != self.waiting_parent {
            self.reject("response.create must continue the response waiting for required input");
            return;
        }
        let model = body.get("model").and_then(|m| m.text()).unwrap_or_default();
        let model = model.trim();
        if !model.is_empty() && model != self.req.model && model != self.initial_model {
            self.fail(ExecError::replay_required());
            return;
        }
        let upstream = if self.req.model.is_empty() {
            self.initial_model.trim().to_owned()
        } else {
            self.req.model.trim().to_owned()
        };
        if self.pending.len() >= MAX_PENDING {
            self.fail(local("too many outstanding response.create requests"));
            return;
        }
        // Client facts must precede routing: the alias names what the client selected.
        // Report the reasoning effort from the routed frame, as for the bootstrap.
        let usage = self.current.usage.successor(Some(payload.as_bytes()));
        let frame = match body.routed(&upstream, false) {
            std::borrow::Cow::Borrowed(_) => payload,
            std::borrow::Cow::Owned(bytes) => String::from_utf8(bytes).expect("Codex boundary validated UTF-8"),
        };
        if usage.enabled() {
            usage.request(Format::Codex, frame.as_bytes());
        }
        let accounting = self.detached.then(|| Accounting::new(usage.clone()));
        let prepared = Prepared {
            native: self.initial.native,
            usage,
        };
        self.pending.push_back(Pending { prepared, accounting });
        self.write(frame);
    }

    /// `releaseSteeringSettings`.
    fn release_steering_settings(&mut self, parent: &str) {
        let in_flight = self.unacknowledged.iter().any(|p| p == parent) || self.accepted.values().any(|p| p == parent);
        if !in_flight {
            self.steering_settings.remove(parent);
        }
    }

    /// One upstream frame (the reader goroutine).
    fn upstream(&mut self, read: Option<Read>) {
        let text = match read {
            Some(Read::Text(text)) => text,
            // Go's duplex wraps the raw read error, not `mapCodexWebsocketReadError`'s 413.
            Some(Read::Failed(error)) => {
                let error = self.writer_error().or_else(|| self.turn.loss()).unwrap_or(error);
                return self.fail(error);
            }
            None => {
                let error = self
                    .writer_error()
                    .or_else(|| self.turn.loss())
                    .unwrap_or_else(|| transport("codex websockets executor: session read channel closed"));
                return self.fail(error);
            }
        };
        if text.trim().is_empty() {
            return;
        }
        // A native connection hands each frame on as it arrived.
        let payload = text.as_str();
        let kind = rawjson::get(payload, "type").str().to_owned();
        let establishing = self.first_response && kind == "response.created";
        if kind == "response.created" && !self.response_created(payload) {
            return;
        }
        self.turn.signals(payload);
        self.account(payload, &kind);
        match self.turn.wire.ws_response_observation(payload) {
            Some(observation) => {
                self.observing = Some(Observing {
                    observation,
                    payload: payload.to_owned(),
                    kind,
                    establishing,
                });
            }
            None => self.observed(payload, &kind, establishing),
        }
    }

    /// The rest of an upstream message's handling, once observers saw it.
    fn observed(&mut self, payload: &str, kind: &str, establishing: bool) {
        // Steering acknowledgements, pending notices and failures are opaque: IDs, input,
        // sequence numbers and event types reach the client byte for byte.
        if kind.starts_with("response.steer.") {
            self.steer_event(kind, payload);
            self.outbox.push_back(Ok(Bytes::from(payload.to_owned())));
            return;
        }
        let failure = kind == "error" || kind == "response.failed";
        let cooling = self.turn.cooling;
        let terminal =
            ws_error(payload, cooling).or_else(|| response::terminal_failure(payload, cooling).map(|(e, _)| e));
        // An account failure on a native connection never reaches the client as sent,
        // whichever request it belongs to: its frame can name the pooled account. The
        // error alone ends the stream, and the server delivers the allowlisted
        // subscription-limit event (#6) or closes silently.
        if failure && let Some(error) = terminal.as_ref().filter(|e| super::account_failure(e.status)) {
            self.turn.wire.ws_exec_error("upstream_error", error);
            let error = error.clone();
            self.fail_records(&error);
            self.outbox.push_back(Err(error));
            self.done = true;
            return;
        }
        // A failure before `response.created` rejects the oldest pending create, which
        // leaves the queue; a failure of the running response ends it.
        if !self.first_response && failure {
            let mut failed = rawjson::get(payload, "response.id").str().to_owned();
            if failed.is_empty() {
                failed = rawjson::get(payload, "response_id").str().to_owned();
            }
            // A failure of the running response must not consume a queued create; a
            // rejection before `response.created` belongs to the oldest pending create.
            let current_failure = !failed.is_empty() && failed == self.response_id;
            let ambiguous = failed.is_empty()
                && ((!self.pending.is_empty() && self.response_active) || !self.unacknowledged.is_empty());
            if ambiguous {
                // Guessing could corrupt either request: keep the event, end the socket.
                self.outbox.push_back(Ok(Bytes::from(payload.to_owned())));
                self.fail(local(
                    "cannot associate websocket failure with a response or pending create",
                ));
                return;
            }
            // Both failure kinds always classify; the fallback only keeps the record a
            // failure.
            let error = terminal
                .clone()
                .unwrap_or_else(|| local("upstream failure event without details"));
            match self.pending.pop_front() {
                // The upstream rejected that create before it started.
                Some(rejected) if !current_failure => {
                    if let Some(accounting) = rejected.accounting {
                        accounting.fail(&error);
                    }
                }
                popped => {
                    if let Some(popped) = popped {
                        self.pending.push_front(popped);
                    }
                    self.response_active = false;
                    self.automatic_active = false;
                    // The running response failed: the open one it names, else the latest.
                    let index = if failed.is_empty() {
                        self.open.len().checked_sub(1)
                    } else {
                        self.open.iter().rposition(|open| open.id == failed)
                    };
                    if let Some(open) = index.and_then(|i| self.open.remove(i)) {
                        open.accounting.fail(&error);
                    }
                }
            }
        }
        // Every event reaches the client as upstream sent it: nothing is repaired or
        // restored.
        // Only the first rejection fails the stream (and can enter bootstrap retry);
        // later ones are events the client may correct on this socket.
        if let Some(error) = &terminal {
            self.turn.wire.ws_exec_error("upstream_error", error);
        }
        if self.first_response
            && let Some(mut error) = terminal
        {
            // Later failures already reach the client as events (below); an initial
            // context-window failure does too, so Codex can compact and retry (#5). On a
            // native connection any other initial failure travels on the error as the
            // upstream sent it, for delivery once failover is decided.
            match self
                .turn
                .codex_client
                .then(|| cpa_common::codex_client::context_window_failure_event(payload))
                .flatten()
            {
                Some(event) => self.outbox.push_back(Ok(Bytes::from(event.into_owned()))),
                None => {
                    error.delivered_event = Some(Box::new(Bytes::copy_from_slice(payload.as_bytes())));
                }
            }
            self.outbox.push_back(Err(error));
            self.done = true;
            return;
        }
        if kind == "response.output_item.done" {
            self.turn.items.collect(payload);
        }
        if response_terminal(kind) {
            self.response_active = false;
            self.automatic_active = false;
            // Settled before the event is delivered: the record does not depend on
            // whether the client is still there to receive it.
            self.settle_terminal(payload, kind);
        }
        self.outbox.push_back(Ok(Bytes::from(payload.to_owned())));
        if establishing {
            // Delivered before any locally generated error, so the handler also sees the
            // bootstrap succeed first.
            self.input_ready = true;
        }
    }

    /// `response.created`: which request it serves and what it retains. False ends the
    /// stream (an automatic successor without retained parent settings).
    fn response_created(&mut self, payload: &str) -> bool {
        let mut parent = rawjson::get(payload, "response.previous_response_id").str().to_owned();
        if parent.is_empty() {
            parent.clone_from(&self.response_id);
        }
        let automatic = !self.first_response && self.pending.is_empty();
        if automatic {
            let settings = self
                .steering_settings
                .get(&parent)
                .or_else(|| self.response_settings.get(&parent))
                .cloned();
            let Some(settings) = settings else {
                self.fail(local("automatic successor has no retained parent settings"));
                return false;
            };
            self.current = settings;
        }
        self.accepted.retain(|_, target| *target != parent);
        self.waiting_parent.clear();
        self.automatic_active = automatic;
        let accounting = match self.pending.pop_front() {
            Some(Pending { prepared, accounting }) => {
                self.current = prepared;
                accounting.or_else(|| self.detach())
            }
            // An automatic successor: a record of its own, with the facts of the request
            // whose settings it runs on.
            None => self.detached.then(|| {
                self.current.usage = self.current.usage.successor(None);
                Accounting::new(self.current.usage.clone())
            }),
        };
        self.response_id = rawjson::get(payload, "response.id").str().to_owned();
        if let Some(accounting) = accounting {
            self.track(self.response_id.clone(), accounting);
        }
        self.response_settings
            .insert(self.response_id.clone(), self.current.retained());
        self.response_order.push_back(self.response_id.clone());
        if self.response_order.len() > RETAINED_RESPONSES
            && let Some(oldest) = self.response_order.pop_front()
        {
            self.response_settings.remove(&oldest);
        }
        self.release_steering_settings(&parent);
        self.first_response = false;
        self.response_active = true;
        self.turn.items = Default::default();
        true
    }

    /// The bootstrap create started: the attempt's accounting moves to that response's
    /// own record, and later creates and automatic successors get records of their own.
    /// `None` when the attempt's observer keeps no such records, or already published;
    /// the attempt then keeps its accounting for the whole connection.
    fn detach(&mut self) -> Option<Accounting> {
        let usage = self.turn.usage.detach()?;
        self.detached = true;
        self.current.usage = usage.clone();
        Some(Accounting::new(usage))
    }

    /// `response.steer.*`: acknowledgement bookkeeping.
    fn steer_event(&mut self, kind: &str, payload: &str) {
        let id = rawjson::get(payload, "steer.id").str().to_owned();
        let mut parent = rawjson::get(payload, "steer.previous_response_id").str().to_owned();
        if parent.is_empty() {
            parent.clone_from(&self.response_id);
        }
        let consume = |unacknowledged: &mut Vec<String>| {
            if let Some(i) = unacknowledged.iter().position(|t| *t == parent || t.is_empty()) {
                unacknowledged.remove(i);
            }
        };
        match kind {
            "response.steer.accepted" => {
                consume(&mut self.unacknowledged);
                self.accepted.insert(id, parent);
            }
            "response.steer.failed" => {
                if self.accepted.remove(&id).is_none() {
                    consume(&mut self.unacknowledged);
                }
                self.release_steering_settings(&parent);
            }
            // Tool results may already wait in the queue; no automatic successor can start
            // until an explicit continuation supplies them.
            "response.steer.pending" => self.waiting_parent = parent,
            _ => {}
        }
    }
}
