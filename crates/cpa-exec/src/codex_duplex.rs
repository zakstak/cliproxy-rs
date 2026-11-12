//! Response steering: a full-duplex Codex WebSocket (codex_websockets_duplex.go).
//!
//! With `response-steering` on, a downstream Responses WebSocket whose turn runs on a
//! Codex credential in WebSocket mode keeps that upstream socket for the rest of the
//! connection. Once the first `response.created` reached the client (or the first
//! response's terminal event, when upstream ends it without one), the client's later
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
//! Go keeps steers in flight without a bound. Here at most [`MAX_STEERS`] are
//! unacknowledged, or accepted and awaiting release. A client steer past it ends the
//! connection before it is written; so does an upstream acceptance past it, after the
//! client received that event.
//!
//! Usage is recorded per upstream response, not per connection (#25). The attempt that
//! opened the socket accounts for the bootstrap create until its `response.created`; then
//! its accounting moves to that response's own record (`UsageSink::detach`), and the
//! attempt publishes nothing more. Every later create sent upstream, and every automatic
//! successor upstream starts, gets a record of its own (`UsageSink::successor`), bound to
//! its response ID once it starts. A record settles once, as soon as its outcome is
//! observed, while the connection stays open: its terminal event (with the usage that
//! event reported, or none), its own failure, or a rejection before it started. A failure
//! naming a response the socket saw start is that response's, however many others run or
//! wait. With complete response history, an unmatched ID rejects the oldest pending
//! create as inherited proxy policy. After history eviction it is ambiguous instead.
//! Settled records are never revisited. A message whose observation was running when the
//! consumer left still settles what it carries, undelivered. When the connection ends,
//! only records still open settle: as the connection's failure, or as the proxy's
//! cancellation (`crate::cancel`) when the client left. A frame refused here, or a create
//! still queued here, never went upstream and gets no record.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use bytes::Bytes;
use cpa_common::codex_body::CodexBody;
use cpa_core::credential::Credential;
use cpa_core::exec::{ExecError, ExecRequest, ExecStream, FailureScope, UsageSink};
use cpa_core::format::Format;
use futures_util::{FutureExt, StreamExt};
use tokio::sync::{OwnedMutexGuard, mpsc};

use super::{Authority, Read, Reading, Turn, Upstream, transport};
use crate::codex_request::View;
use crate::rawjson;

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
/// Steers in flight: written and not yet acknowledged, or accepted and not yet released
/// by their successor's `response.created` or a `response.steer.failed`. Their pinned
/// settings are at most one per such steer. A local resource ceiling, not a provider
/// limit: no first-party or documented source bounds steering, and Go keeps both
/// collections without one. This ceiling is separate from write backpressure: socket
/// buffers and frame sizes determine which limit a stalled upstream reaches first.
/// It also stops an upstream that reads every steer and never releases one.
const MAX_STEERS: usize = 64;
const _: () = assert!(MAX_STEERS > WRITE_QUEUE + 1);

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
    /// A later create's model-only `response.metadata` event, delivered right before
    /// the `response.created` that starts it ([`Upstream::later_metadata`]). The
    /// bootstrap create has none: the server delivers its metadata.
    announce: Option<String>,
}

impl Prepared {
    pub(super) fn new(native: bool) -> Self {
        Self {
            native,
            usage: UsageSink::default(),
            announce: None,
        }
    }

    /// The settings a response retains. An automatic successor that inherits them is not
    /// a client request, so it gets no metadata.
    fn retained(&self) -> Self {
        Self {
            native: self.native,
            usage: self.usage.clone(),
            announce: None,
        }
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
    /// This record belongs to the latest start, even when multiple starts reuse an ID.
    /// Settling it must never promote an older record to the latest one.
    latest_start: bool,
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

/// The request an upstream failure (`error`, `response.failed`) after the first response
/// belongs to. A failure names its response in `response.id`, or in `response_id` on an
/// `error` event.
enum Owner {
    /// No request can be identified safely.
    Ambiguous,
    /// The oldest pending create, which upstream rejected before it started: the failure
    /// names no response while none runs, or an unmatched ID before history eviction.
    Rejected,
    /// A response this socket started. `ends_latest`: the failure ends the latest
    /// response, which no longer runs. `open`: the response's unsettled record.
    Started { ends_latest: bool, open: Option<usize> },
}

/// What [`Duplex::settle`] left to the caller.
enum Settlement {
    /// The message's records are settled, if it settles any.
    Settled,
    /// A failure the client must not see ([`cpa_core::exec::Delivery::Withheld`]): an
    /// account failure, or one whose frame reads as one. Every record failed with it, and
    /// it ends the stream.
    Account(ExecError),
    /// A failure no request can be assigned; nothing settled.
    Ambiguous,
}

/// The duplex stream's state: Go's writer and reader goroutines' shared metadata.
/// An upstream message whose observation is running (Go's reader inside
/// `EmitWebSocketResponseEvent`): the rest of its handling waits for it.
struct Observing {
    observation: futures_util::future::BoxFuture<'static, ()>,
    payload: String,
    kind: String,
    /// The message's `response.id` ([`crate::codex_event::Event::response_id`]).
    response_id: Option<String>,
    /// What the pinned WebSocket reader makes of the message (`codex_ws::reading`).
    reading: Reading,
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
    /// Parents of steers written and not yet acknowledged. With `accepted`, at most
    /// [`MAX_STEERS`] entries ([`Duplex::steer_room`]).
    unacknowledged: Vec<String>,
    /// Accepted steer id to parent response.
    accepted: HashMap<String, String>,
    current: Prepared,
    response_id: String,
    response_settings: HashMap<String, Prepared>,
    response_order: VecDeque<String>,
    /// A started response has left the retained history. An unmatched failure ID may
    /// now name that response instead of rejecting a pending create.
    response_history_evicted: bool,
    /// Settings pinned by in-flight steering, independent of the retention window. Only
    /// a parent of `unacknowledged` or `accepted` keeps its entry.
    steering_settings: HashMap<String, Prepared>,
    /// The response waiting for required input (`response.steer.pending`).
    waiting_parent: String,
    /// Upstream started a successor on its own.
    automatic_active: bool,
    /// Client creates waiting until steering settles.
    queued_creates: VecDeque<String>,
    /// A steer whose parent settings are unknown waits for pending creates to start.
    deferred_steer: Option<(String, String)>,
    /// The metadata of the explicit create whose `response.created` is being handled.
    announce: Option<String>,
    /// Client frames are read only once the first response's `response.created`, or its
    /// terminal event when it has none, was delivered.
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
/// account check (`WebsocketAuthEnabled`) right before `writeCodexWebsocketMessage`. The
/// client authority admits each write once the socket is ready to take it
/// ([`super::write_phases`], N1 #111 W2): a frame admitted while the client's key was
/// accepted is not written once it no longer is, however long it waited in this queue or
/// for the socket. The stream then ends with the refusal, as it does for a disabled
/// account.
async fn write_loop(
    conn: Arc<Upstream>,
    input: Arc<SteeringInput>,
    authority: Option<Arc<Authority>>,
    credential: String,
    wire: crate::codex_capture::Wire,
    mut queue: mpsc::Receiver<Outgoing>,
) -> Result<(), ExecError> {
    while let Some(Outgoing { frame }) = queue.recv().await {
        if !(input.enabled)(&credential) {
            return Err(local("websocket credential is no longer enabled"));
        }
        // The capture records the frame the writer attempts, as before the write.
        wire.ws_frame(&conn.target.url, frame.as_bytes());
        conn.send_authorized(frame, authority.as_deref())
            .await
            .map_err(|failure| failure.error)?;
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
            turn.session.authority(),
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
            response_history_evicted: false,
            steering_settings: HashMap::new(),
            waiting_parent: String::new(),
            automatic_active: false,
            queued_creates: VecDeque::new(),
            deferred_steer: None,
            announce: None,
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
            // Nobody reads the stream any more, but the upstream outcome that message
            // carries is known: it settles the records it settles while the client is
            // there (a terminal, a failure, a rejection), and nothing is delivered.
            self.settle(
                &observing.payload,
                &observing.kind,
                observing.response_id.as_deref(),
                &observing.reading,
            );
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
                        response_id,
                        reading,
                        ..
                    } = self.observing.take().expect("an observation finished");
                    self.observed(&payload, &kind, response_id.as_deref(), reading);
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
    fn account(&self, payload: &str, kind: &str, response_id: Option<&str>) {
        if !self.detached {
            if self.turn.usage.enabled() {
                self.turn.usage.response_line(Format::Codex, payload.as_bytes());
            }
            return;
        }
        if let Some(i) = self.open_index(kind, response_id) {
            self.open[i].accounting.observe(payload);
        }
    }

    /// The open response an upstream event belongs to: the latest one with the
    /// `response.id` a response event names; any other event, or one without an ID,
    /// belongs to the latest. A response event naming no open response (a duplicate
    /// terminal event, say) belongs to none. `response_id` is the ID as the pinned
    /// `Value` reads it ([`crate::codex_event::Event::response_id`]): the last of a
    /// repeated `id`, so a decoded event settles the response its reader completes.
    fn open_index(&self, kind: &str, response_id: Option<&str>) -> Option<usize> {
        let latest = self.open.len().checked_sub(1)?;
        if !RESPONSE_EVENTS.contains(&kind) {
            return Some(latest);
        }
        match response_id.filter(|id| !id.is_empty()) {
            Some(id) => self.open.iter().rposition(|open| open.id == id),
            None => Some(latest),
        }
    }

    /// What an upstream message settles, apart from its delivery: a terminal event its
    /// response's record, a failure the record of the request it belongs to, and a
    /// withheld failure every record. `reading` is the message's classification. The
    /// queue and running state follow (a rejected create leaves the queue). The same
    /// whether or not the consumer still reads the stream.
    fn settle(&mut self, payload: &str, kind: &str, response_id: Option<&str>, reading: &Reading) -> Settlement {
        if let Reading::Failure { error, .. } = reading {
            // `super::reading` withheld an account failure, and a failure whose frame
            // reads as one under another reading (an explicit `status: 400` beside a
            // rate-limit error, say): neither may reach the client as an event, so the
            // stream ends on it, as on any account failure. Its classified status stays.
            if error.withheld() {
                self.fail_records(error);
                return Settlement::Account(error.clone());
            }
            if !self.first_response {
                match self.failure_owner(payload) {
                    Owner::Ambiguous => return Settlement::Ambiguous,
                    Owner::Rejected => {
                        if let Some(rejected) = self.pending.pop_front()
                            && let Some(accounting) = rejected.accounting
                        {
                            accounting.fail(error);
                        }
                    }
                    Owner::Started { ends_latest, open } => {
                        if ends_latest {
                            self.response_active = false;
                            self.automatic_active = false;
                        }
                        if let Some(open) = open.and_then(|i| self.open.remove(i)) {
                            open.accounting.fail(error);
                        }
                    }
                }
            }
        }
        // Only an event the WebSocket reader ends the response on
        // (`codex_event::Outcome::ends_turn`): a `response.completed` without a
        // `response`, or a `response.done`, ends nothing.
        if matches!(reading, Reading::Event { terminal: true }) {
            self.response_active = false;
            self.automatic_active = false;
            if self.first_response {
                self.first_terminal();
            } else {
                self.settle_terminal(kind, response_id);
            }
        }
        Settlement::Settled
    }

    /// The first response ended without a `response.created`: upstream answered the
    /// bootstrap create with its terminal event alone. That create was the only one
    /// upstream had, because no client frame is read before the first response ends or
    /// starts, so the terminal is its outcome. Its record settles now with what the
    /// terminal reported, detached as `response.created` would have detached it, so the
    /// attempt publishes nothing more and later creates get records of their own. The
    /// connection then goes on as after a first response that started: client frames are
    /// read, and the next create runs on this socket with the bootstrap's settings.
    /// Nothing is invented: no `response.created`, and the terminal's response ID binds
    /// no settings, so a later event naming it is read as any unmatched ID.
    fn first_terminal(&mut self) {
        let bootstrap = self.pending.pop_front().and_then(|p| p.accounting);
        if let Some(accounting) = bootstrap.or_else(|| self.detach()) {
            accounting.complete();
        }
        self.first_response = false;
    }

    /// [`Owner`] of a failure after the first response. A response the socket saw start
    /// never owns a pending create, so a failure naming one, still open or settled, the
    /// latest or an older one, leaves the queue alone; only a failure of the latest
    /// response stops it running.
    // ponytail: retain settings for RETAINED_RESPONSES starts. Once history is evicted,
    // unmatched failure IDs are ambiguous and end the socket instead of consuming a
    // pending create. Stronger attribution needs provider-supported request correlation.
    // A failure reusing a retained settled ID cannot be told from that old response;
    // only a new response.created binds a fresh record to that ID.
    fn failure_owner(&self, payload: &str) -> Owner {
        // As the pinned reader decodes the failure: a struct written as a JSON array by
        // its members, the last of a repeated key, private values resolved.
        let view = crate::codex_event::classification_view(payload);
        let mut failed = rawjson::get(&view, "response.id").str().to_owned();
        if failed.is_empty() {
            failed = rawjson::get(&view, "response_id").str().to_owned();
        }
        if failed.is_empty() {
            if (!self.pending.is_empty() && self.response_active) || !self.unacknowledged.is_empty() {
                return Owner::Ambiguous;
            }
            if !self.pending.is_empty() {
                return Owner::Rejected;
            }
            return Owner::Started {
                ends_latest: true,
                open: self.open.iter().rposition(|open| open.latest_start),
            };
        }
        let open = self.open.iter().rposition(|open| open.id == failed);
        let latest = failed == self.response_id;
        if open.is_some() || latest || self.response_settings.contains_key(&failed) {
            return Owner::Started {
                ends_latest: latest,
                open,
            };
        }
        if self.response_history_evicted {
            return Owner::Ambiguous;
        }
        if self.pending.is_empty() {
            // Go's reading of a failure no create can own: the running response ended.
            Owner::Started {
                ends_latest: true,
                open: self.open.iter().rposition(|open| open.latest_start),
            }
        } else {
            Owner::Rejected
        }
    }

    /// A terminal event settles its response's record with what it reported.
    fn settle_terminal(&mut self, kind: &str, response_id: Option<&str>) {
        if let Some(open) = self.open_index(kind, response_id).and_then(|i| self.open.remove(i)) {
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
        self.open.push_back(Open {
            id,
            latest_start: true,
            accounting,
        });
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
        if let Some(refusal) = self.turn.session.refusal() {
            // The client's key no longer has authority (N1 #111 W2): this frame is not
            // sent, the client gets the refusal, and the connection stays open, as Go
            // rejects a duplex turn (`AuthorizeWebsocketTurn`).
            self.outbox.push_back(Ok(refusal.event));
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
        if !self.steer_room() {
            // Upstream read the earlier steers but released none of them. Like the
            // create bound: a local, request-scoped end, and this frame is not sent.
            self.fail(local(&format!(
                "too many response.steer requests in flight: {MAX_STEERS} await upstream acknowledgement or their successor"
            )));
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
        // Metadata eligibility and client facts are read before model routing.
        let kept = body
            .get("model")
            .and_then(|m| m.text())
            .is_some_and(|named| named.eq_ignore_ascii_case(&upstream));
        let announce = super::handshake_metadata_event(&self.turn.conn.later_metadata(&upstream, kept));
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
            announce,
        };
        self.pending.push_back(Pending { prepared, accounting });
        self.write(frame);
    }

    /// Whether one more steer fits under [`MAX_STEERS`]. Every insertion into
    /// `unacknowledged` or `accepted` that can grow them checks it first.
    fn steer_room(&self) -> bool {
        self.unacknowledged.len() + self.accepted.len() < MAX_STEERS
    }

    /// `releaseSteeringSettings`, for every parent at once: settings stay pinned only
    /// while a steer in flight names their parent. Go releases only the parent the event
    /// names, so an acknowledgement naming another parent than its steer left Go's
    /// entry pinned for the rest of the connection.
    // At most MAX_STEERS entries, each checked against at most MAX_STEERS parents.
    fn release_steering_settings(&mut self) {
        let (unacknowledged, accepted) = (&self.unacknowledged, &self.accepted);
        self.steering_settings
            .retain(|parent, _| unacknowledged.contains(parent) || accepted.values().any(|p| p == parent));
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
        let event = crate::codex_event::decode(payload);
        let reading = super::reading(payload, event.as_ref(), self.turn.cooling);
        // A decoded event or a wrapped failure: what the pinned reader reads.
        let read = event.is_some() || matches!(reading, Reading::Failure { .. });
        let accounted = event.as_ref().is_none_or(crate::codex_event::Event::accounted);
        // The response IDs as the pinned `Value` reads them.
        let response_id = event.as_ref().and_then(crate::codex_event::Event::response_id);
        let parent = event
            .as_ref()
            .and_then(|event| event.response_str("previous_response_id"));
        let kind = match (event, &reading) {
            (Some(event), _) => event.kind.into_owned(),
            // A wrapped status error.
            (None, Reading::Failure { .. }) => "error".to_owned(),
            (None, _) => String::new(),
        };
        if read {
            if kind == "response.created" && !self.response_created(response_id.as_deref(), parent) {
                return;
            }
            self.turn.signals(payload);
            if accounted {
                self.account(payload, &kind, response_id.as_deref());
            }
        }
        match self.turn.wire.ws_response_observation(payload) {
            Some(observation) => {
                self.observing = Some(Observing {
                    observation,
                    payload: payload.to_owned(),
                    kind,
                    response_id,
                    reading,
                });
            }
            None => self.observed(payload, &kind, response_id.as_deref(), reading),
        }
    }

    /// The rest of an upstream message's handling, once observers saw it.
    fn observed(&mut self, payload: &str, kind: &str, response_id: Option<&str>, reading: Reading) {
        // The outbound credential guard (`crate::codex_secret`): a message that carries
        // the socket's credential is never delivered, whatever it does. What it does is
        // decided as for any other message: it settles, cools and accounts as read. Only
        // the stream's guard leaves it out ([`super::CodexExecutor::stream_ws`]), which
        // records that it was served (`ExecResponse::omitted`), so a later failure cannot
        // fail over a response that already ran (#25). Every message processed here goes
        // to the stream; this connection only has to end where the client would
        // otherwise wait for a response that ended.
        let confidential = self.turn.conn.secrets.exposes(payload.as_bytes());
        // Steering acknowledgements, pending notices and failures are opaque: IDs, input,
        // sequence numbers and event types reach the client byte for byte.
        if kind.starts_with("response.steer.") {
            let recorded = self.steer_event(kind, payload);
            self.outbox.push_back(Ok(Bytes::from(payload.to_owned())));
            if !recorded {
                // As for an ambiguous failure: keep the event, end the socket.
                self.fail(local(&format!(
                    "upstream accepted more response.steer requests than the {MAX_STEERS} a connection keeps in flight"
                )));
            }
            return;
        }
        if let Reading::Passed { decoded } = reading {
            // The pinned reader skips or ignores this frame. It settles nothing and
            // reaches the client as sent, unless it would read as an account failure (#6).
            if !super::account_shaped(payload, decoded) {
                self.outbox.push_back(Ok(Bytes::from(payload.to_owned())));
            }
            return;
        }
        // Records settle before the event is delivered: a record does not depend on
        // whether the client is still there to receive it.
        match self.settle(payload, kind, response_id, &reading) {
            Settlement::Settled => {}
            // An account failure on a native connection never reaches the client as
            // sent, whichever request it belongs to: its frame can name the pooled
            // account. The error alone ends the stream, and the server delivers the
            // allowlisted subscription-limit event (#6) or closes silently
            // (`Delivery::Withheld`).
            Settlement::Account(error) => {
                self.turn.wire.ws_exec_error("upstream_error", &error);
                self.outbox.push_back(Err(error));
                self.done = true;
                return;
            }
            Settlement::Ambiguous => {
                // Guessing could corrupt either request: keep the event, end the socket.
                self.outbox.push_back(Ok(Bytes::from(payload.to_owned())));
                self.fail(local(
                    "cannot associate websocket failure with a response or pending create",
                ));
                return;
            }
        }
        // Every event reaches the client as upstream sent it: nothing is repaired or
        // restored.
        // Only the first rejection fails the stream (and can enter bootstrap retry);
        // later ones are events the client may correct on this socket.
        let reading_ended = matches!(reading, Reading::Event { terminal: true });
        let terminal = match reading {
            Reading::Failure { error, .. } => Some(error),
            Reading::Event { .. } | Reading::Passed { .. } => None,
        };
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
                .then(|| crate::codex_event::context_window_event(payload))
                .flatten()
            {
                Some(event) => self.outbox.push_back(Ok(Bytes::from(event.into_owned()))),
                None => super::deliver_as_sent(&mut error, payload),
            }
            self.outbox.push_back(Err(error));
            self.done = true;
            return;
        }
        // An explicit later create's model-only metadata, ahead of its first event.
        if kind == "response.created"
            && let Some(announce) = self.announce.take()
        {
            self.outbox.push_back(Ok(Bytes::from(announce)));
        }
        if confidential {
            // The stream's guard leaves it out, and records it as served.
            self.outbox.push_back(Ok(Bytes::from(payload.to_owned())));
            // A later failure, or an event the client's reader ends a response on, that
            // the client cannot get: the connection ends here, as after an account
            // failure, so the client does not wait for a response that already ended.
            // Its own record settled above; every other one fails with the connection.
            // Any other event is only left out.
            let ends = match &terminal {
                Some(error) => Some(crate::codex_secret::confidential(
                    error.clone(),
                    &self.turn.conn.secrets,
                )),
                None if reading_ended => Some(crate::codex_secret::confidential(
                    local(crate::codex_secret::WITHHELD_FRAME),
                    &self.turn.conn.secrets,
                )),
                None => None,
            };
            if !self.first_response {
                self.input_ready = true;
            }
            if let Some(error) = ends {
                self.fail(error);
            }
            return;
        }
        self.outbox.push_back(Ok(Bytes::from(payload.to_owned())));
        if !self.first_response {
            // The first response started (`response_created`) or ended without starting
            // ([`Duplex::first_terminal`]). Delivered before any locally generated
            // error, so the handler also sees the bootstrap succeed first.
            self.input_ready = true;
        }
    }

    /// `response.created`: which request it serves and what it retains. False ends the
    /// stream (an automatic successor without retained parent settings). `response_id`
    /// and `parent` (its `previous_response_id`) are read as the pinned `Value` reads
    /// them ([`crate::codex_event::Event::response_str`]).
    fn response_created(&mut self, response_id: Option<&str>, parent: Option<String>) -> bool {
        let mut parent = parent.unwrap_or_default();
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
        // Only an explicit create taken from `pending` announces; nothing carries over.
        self.announce = None;
        let accounting = match self.pending.pop_front() {
            Some(Pending {
                mut prepared,
                accounting,
            }) => {
                self.announce = prepared.announce.take();
                self.current = prepared;
                accounting.or_else(|| self.detach())
            }
            // An automatic successor keeps its parent's facts, without announcing.
            None => self.detached.then(|| {
                self.current.usage = self.current.usage.successor(None);
                Accounting::new(self.current.usage.clone())
            }),
        };
        for open in &mut self.open {
            open.latest_start = false;
        }
        self.response_id = response_id.unwrap_or_default().to_owned();
        if let Some(accounting) = accounting {
            self.track(self.response_id.clone(), accounting);
        }
        self.response_settings
            .insert(self.response_id.clone(), self.current.retained());
        self.response_order.push_back(self.response_id.clone());
        // The queue counts starts; an ID's newer occurrence still owns its settings.
        if self.response_order.len() > RETAINED_RESPONSES
            && let Some(oldest) = self.response_order.pop_front()
            && !self.response_order.contains(&oldest)
        {
            self.response_settings.remove(&oldest);
            self.response_history_evicted = true;
        }
        self.release_steering_settings();
        self.first_response = false;
        self.response_active = true;
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

    /// `response.steer.*`: acknowledgement bookkeeping. False when an acceptance would
    /// take the steers in flight past [`MAX_STEERS`]; nothing is recorded then.
    fn steer_event(&mut self, kind: &str, payload: &str) -> bool {
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
                // Acknowledging a steer in flight moves it, and a repeated ID replaces
                // its entry; only an acceptance of no steer in flight adds one.
                if !self.accepted.contains_key(&id) && !self.steer_room() {
                    return false;
                }
                self.accepted.insert(id, parent);
            }
            "response.steer.failed" => {
                if self.accepted.remove(&id).is_none() {
                    consume(&mut self.unacknowledged);
                }
            }
            // Tool results may already wait in the queue; no automatic successor can start
            // until an explicit continuation supplies them.
            "response.steer.pending" => self.waiting_parent = parent,
            _ => {}
        }
        self.release_steering_settings();
        true
    }
}
