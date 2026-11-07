//! The native Codex HTTP route (#23, and the HTTP half of #25): a Responses body from a
//! Responses client, sent over HTTP to a `chatgpt`, `api-key` or `compatible` profile
//! (`docs/codex/CONTRACT.md` "Native HTTP route"). The predicates and request edits of
//! the native WebSocket route (#24) live here too; its socket handling is in
//! `codex_ws.rs` and `codex_duplex.rs` ("Native WebSocket route").
//!
//! The request goes upstream as the client sent it. The only edits are the contract's
//! permitted transformations: the upstream `model` of an alias, and `stream: true` for a
//! buffered client on the `chatgpt` profile, whose buffered support is unknown. Nothing
//! else runs: no translation, thinking, payload rules, prompt-cache key, reasoning
//! replay, tool or namespace preparation, or Sign in with ChatGPT rules.
//!
//! The response reaches the client as the upstream sent it. A stream's events are
//! forwarded byte for byte; observation (usage, capture, failure classification) reads
//! a copy. Leading handshake events are held, never changed, so that a failure before
//! any content can still fail over to another credential. Once anything is delivered,
//! a failure ends the stream after its event and is never replayed elsewhere. A
//! buffered `chatgpt` answer is the `response` object of the terminal event, byte for
//! byte. Two existing exceptions stay (`docs/DIFFERENCES-FROM-GO.md`): a Codex client
//! gets the backend's status-less context-window `error` event as `response.failed`
//! (#5), and an empty `response.incomplete` without a client-side reason before any
//! content is a request-scoped 502.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::time::Duration;

use cpa_common::codex_body::{CodexBody, Kind};

use super::*;

/// The endpoint profiles the native route serves. The `siwc` profile is not one of them:
/// no first-party source covers it, so it keeps the legacy request path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Profile {
    Chatgpt,
    ApiKey,
    Compatible,
}

impl Profile {
    /// The profile follows the credential: a ChatGPT OAuth token is `chatgpt` whatever
    /// its base URL (a relay in front of the backend), an API key is `api-key` on
    /// `api.openai.com` and `compatible` anywhere else.
    pub(crate) fn of(view: &View<'_>) -> Option<Self> {
        if view.siwc {
            return None;
        }
        if !view.api_key {
            return Some(Self::Chatgpt);
        }
        let host = url::Url::parse(view.base_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_default();
        Some(if host.eq_ignore_ascii_case("api.openai.com") {
            Self::ApiKey
        } else {
            Self::Compatible
        })
    }
}

/// Whether `req` on `view` may take the native route: Responses formats on both sides, a
/// generation, and a native profile. The server decides with this
/// ([`CodexExecutor::native_http`]).
pub(crate) fn eligible(view: &View<'_>, req: &ExecRequest) -> Option<Profile> {
    (req.operation == Operation::Generate && request::responses_formats(req))
        .then(|| Profile::of(view))
        .flatten()
}

/// Whether the executor serves `req` natively: an eligible request the server handed
/// over as native. The server clears `stream_payloads` on a native attempt, because no
/// hook reads its chunks; a request that still asks for unframed payloads (a plugin
/// model callback, a hooked attempt the server did not make native) keeps the legacy
/// path and its payload framing.
pub(crate) fn route(view: &View<'_>, req: &ExecRequest) -> Option<Profile> {
    eligible(view, req).filter(|_| !req.stream_payloads)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call {
    Stream,
    Buffered,
    Compact,
}

/// Whether a WebSocket turn on `view` may take the native route (#24): an eligible
/// request on a credential with `websockets: true` whose frame is a `response.create`,
/// the one request frame the pinned client sends (`codex-api/src/common.rs:400-406`).
/// That covers a first turn, a continuation with `previous_response_id`, a prewarm with
/// `generate: false`, and a create the client sends while a response runs. The
/// Go-defined `response.append` keeps the legacy path: no first-party or documented
/// source defines it, so the proxy's own handling is all it has. The server decides
/// with this ([`CodexExecutor::native_ws`]); `frame` is the client's frame.
pub(crate) fn eligible_ws(view: &View<'_>, req: &ExecRequest, frame: &[u8]) -> Option<Profile> {
    eligible(view, req).filter(|_| view.websockets() && creates(frame))
}

/// Whether the executor serves a WebSocket turn natively, as [`route`] does for HTTP.
pub(crate) fn route_ws(view: &View<'_>, req: &ExecRequest) -> Option<Profile> {
    eligible_ws(view, req, &req.body).filter(|_| !req.stream_payloads)
}

/// Whether `frame` is a `response.create`. A frame the request boundary refuses counts
/// too, so that the boundary rejects it on the native route instead of the legacy path
/// rewriting it.
pub(crate) fn creates(frame: &[u8]) -> bool {
    match CodexBody::parse(frame) {
        Ok(body) => body.get("type").and_then(|t| t.text()).as_deref() == Some("response.create"),
        Err(_) => true,
    }
}

/// The upstream model of a native request: the routed model, trimmed. A thinking suffix
/// has no native meaning, so it is refused rather than turned into body fields.
pub(crate) fn upstream_model(req: &ExecRequest) -> Result<String, ExecError> {
    let suffix = cpa_common::thinking::parse_suffix(&req.model);
    if suffix.has_suffix {
        return Err(ExecError::local(
            400,
            FailureScope::Request,
            format!(
                "model suffix `({})` is not supported on native Codex Responses requests; set `reasoning` in the request body, or route an alias to a model without a suffix",
                suffix.raw_suffix
            ),
        ));
    }
    Ok(req.model.trim().to_owned())
}

/// `frame` with `model` routed to `model`: the contract's one permitted request edit
/// besides the buffered `stream` adaptation. The bytes are shared when the model
/// already matches.
pub(crate) fn routed(frame: &Bytes, model: &str) -> Result<Bytes, ExecError> {
    let body = CodexBody::parse(frame)?;
    if body.get("model").and_then(|m| m.text()).as_deref() == Some(model) {
        return Ok(frame.clone());
    }
    let quoted = serde_json::to_string(model).unwrap_or_default();
    Ok(Bytes::from(body.with_member("model", &quoted)))
}

/// The upstream body: the client's bytes with only the permitted edits.
fn upstream_body(req: &ExecRequest, profile: Profile, call: Call, model: &str) -> Result<Bytes, ExecError> {
    let mut bytes = routed(&req.body, model)?;
    if call == Call::Buffered && profile == Profile::Chatgpt {
        let current = CodexBody::parse(&bytes)?;
        let streamed =
            (current.get("stream").map(|m| m.kind) != Some(Kind::True)).then(|| current.with_member("stream", "true"));
        if let Some(streamed) = streamed {
            bytes = Bytes::from(streamed);
        }
    }
    Ok(bytes)
}

impl CodexExecutor {
    /// One native HTTP request. The body already passed the request boundary.
    pub(crate) async fn native(
        &self,
        view: &View<'_>,
        settings: &Settings,
        req: ExecRequest,
        profile: Profile,
    ) -> Result<ExecResponse, ExecError> {
        let call = match (req.alt.as_deref(), req.stream) {
            (Some("responses/compact"), true) => {
                return Err(ExecError::local(
                    400,
                    FailureScope::Request,
                    "streaming not supported for /responses/compact",
                ));
            }
            (Some("responses/compact"), false) => Call::Compact,
            (_, true) => Call::Stream,
            (_, false) => Call::Buffered,
        };
        let model = upstream_model(&req)?;
        let body = upstream_body(&req, profile, call, &model)?;
        // The body passed the boundary, so this never fails.
        let text = std::str::from_utf8(&body)
            .map_err(|_| ExecError::local(400, FailureScope::Request, "request body is not valid UTF-8"))?;
        let upstream_stream = call == Call::Stream || (call == Call::Buffered && profile == Profile::Chatgpt);
        let cache = request::prompt_cache_id(&req, text, None, false);
        let headers = request::http_headers(
            view,
            &req.headers,
            text,
            &model,
            cache.as_deref(),
            upstream_stream,
            // A native exchange carries the caller's own identity (#26).
            crate::codex_identity::Origin::Caller,
        );
        report_request(
            &req,
            if call == Call::Compact {
                Format::OpenAIResponse
            } else {
                Format::Codex
            },
            text,
        );
        let url = match call {
            Call::Compact => format!("{}/responses/compact", view.base_url),
            _ => format!("{}/responses", view.base_url),
        };
        let wire = Wire::new(req.capture(), view.credential);
        let error_body = if call == Call::Stream {
            ErrorBody::Strict
        } else {
            ErrorBody::Lossy
        };
        let opened = self
            .open_for(
                view,
                settings,
                url,
                headers,
                body.clone(),
                &wire,
                error_body,
                &model,
                true,
            )
            .await;
        let upstream = match (call, opened) {
            (Call::Compact, Err(error)) => return Err(compact_error(error)),
            (_, Err(error)) => return Err(error),
            (_, Ok(upstream)) => upstream,
        };
        let sse = upstream
            .headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"));
        let observer = Observer {
            usage: req.usage.clone(),
            wire,
            capture_lines: call == Call::Stream,
            model_level_cooling: settings.model_level_cooling,
            codex_client: request::codex_client(&req),
            items: 0,
            saw_delta: false,
        };
        let (status, headers) = (upstream.status, upstream.headers);
        if call == Call::Stream && sse {
            let hold = settings.bootstrap_buffering.then_some(settings.bootstrap_timeout);
            let stream = passthrough(crate::upstream::framed(upstream.body), observer, hold);
            return Ok(ExecResponse {
                status,
                headers,
                body: ResponseBody::Stream(stream),
            });
        }
        // Every byte that arrived is recorded, an unfinished tail included, then any read
        // error (the buffered call sites' policy).
        let mut body = upstream.body;
        let (mut data, mut failed) = (Vec::new(), None);
        while let Some(chunk) = body.next().await {
            match chunk {
                Ok(chunk) => data.extend_from_slice(&chunk),
                Err(error) => {
                    failed = Some(error);
                    break;
                }
            }
        }
        observer.wire.chunk(&data);
        let read_failure = |observer: &Observer, error: ExecError| {
            observer.wire.exec_error(&error);
            match call {
                Call::Compact => compact_error(error),
                _ => error,
            }
        };
        let body = if sse {
            let wire = observer.wire.clone();
            match observer.aggregate(&data) {
                Ok(body) => body,
                Err(error) => {
                    if let Some(failure) = failed {
                        wire.exec_error(&failure);
                    }
                    return Err(error);
                }
            }
        } else {
            if let Some(error) = failed {
                return Err(read_failure(&observer, error));
            }
            if req.usage.enabled() {
                req.usage.response_body(Format::OpenAIResponse, &data);
            }
            Bytes::from(data)
        };
        Ok(ExecResponse {
            status,
            headers,
            body: ResponseBody::Buffered(body),
        })
    }
}

/// What one upstream event means for delivery.
enum Seen {
    /// A leading event with no content yet (`response.created` and the like).
    Handshake,
    Content,
    /// `response.completed`, `response.incomplete` or `response.done`.
    Terminal,
    /// An empty `response.incomplete` that counts as a silent upstream abort.
    EmptyIncomplete,
    /// A failure event, classified on a copy. `deliver` replaces the event only for the
    /// #5 context-window conversion.
    Failure {
        error: ExecError,
        body: Bytes,
        deliver: Option<Bytes>,
    },
}

/// Passive observation of the upstream events: usage, capture and classification. It
/// never changes what the client receives, except through [`Seen::Failure::deliver`].
struct Observer {
    usage: cpa_core::exec::UsageSink,
    wire: Wire,
    /// Record each scanned line in the request log, as the streaming call site does; a
    /// buffered body is recorded whole instead.
    capture_lines: bool,
    model_level_cooling: bool,
    codex_client: bool,
    items: usize,
    saw_delta: bool,
}

/// The lines of one framed event, split as the shared SSE framer splits them: `\r\n`,
/// a lone `\r` and a lone `\n` each end a line.
fn sse_lines(frame: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = frame;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let Some(end) = rest.iter().position(|&b| b == b'\r' || b == b'\n') else {
            let line = rest;
            rest = &[];
            return Some(line);
        };
        let line = &rest[..end];
        let crlf = rest[end] == b'\r' && rest.get(end + 1) == Some(&b'\n');
        rest = &rest[end + if crlf { 2 } else { 1 }..];
        Some(line)
    })
}

/// The `event:` name and `data` payload of one framed SSE event. Several `data:` lines
/// join with newlines, as SSE defines, in one growing buffer.
fn parse_frame(frame: &[u8]) -> (Option<&str>, Option<Cow<'_, str>>) {
    let mut name = None;
    let mut data: Option<Cow<'_, str>> = None;
    for line in sse_lines(frame) {
        // An event that is not UTF-8 carries no payload the observer can read.
        let Ok(line) = std::str::from_utf8(line) else {
            return (None, None);
        };
        if let Some(value) = line.strip_prefix("event:") {
            name = Some(value.trim());
        } else if let Some(value) = line.strip_prefix("data:") {
            let value = value.strip_prefix(' ').unwrap_or(value);
            match &mut data {
                None => data = Some(Cow::Borrowed(value)),
                Some(Cow::Owned(joined)) => {
                    joined.push('\n');
                    joined.push_str(value);
                }
                Some(Cow::Borrowed(first)) => {
                    let mut joined = String::with_capacity(first.len() + value.len() + 1);
                    joined.push_str(first);
                    joined.push('\n');
                    joined.push_str(value);
                    data = Some(Cow::Owned(joined));
                }
            }
        }
    }
    (name, data)
}

/// What the client receives for a failure it does not see as an event (a buffered
/// answer, or a failure before anything was delivered): the provider's error object in
/// the API's error envelope, byte for byte, with the event's `sequence_number` as
/// spelled; the whole event when it carries no error object. Nothing is filled in.
fn delivered_failure(payload: &str) -> Bytes {
    let whole = || Bytes::copy_from_slice(payload.as_bytes());
    let Ok(event) = CodexBody::parse(payload.as_bytes()) else {
        return whole();
    };
    fn object<'a>(m: Option<&cpa_common::codex_body::Member<'a>>) -> Option<&'a [u8]> {
        m.filter(|m| m.kind == Kind::Object).map(|m| m.raw)
    }
    let mut error = None;
    if let Some(response) = object(event.get("response"))
        && let Ok(response) = CodexBody::parse(response)
    {
        error = object(response.get("error"));
    }
    let Some(error) = error.or_else(|| object(event.get("error"))) else {
        return whole();
    };
    let mut out = Vec::with_capacity(error.len() + 48);
    out.extend_from_slice(b"{\"error\":");
    out.extend_from_slice(error);
    if let Some(sequence) = event.get("sequence_number") {
        out.extend_from_slice(b",\"sequence_number\":");
        out.extend_from_slice(sequence.raw);
    }
    out.push(b'}');
    Bytes::from(out)
}

impl Observer {
    fn see(&mut self, frame: &Bytes) -> Seen {
        let seen = self.classify(frame);
        if self.capture_lines {
            // Each physical line as scanned, and none after the event that ends the
            // response.
            let last = matches!(seen, Seen::Terminal | Seen::EmptyIncomplete | Seen::Failure { .. });
            for line in sse_lines(frame) {
                self.wire.chunk(line);
                if last && line.starts_with(b"data:") {
                    break;
                }
            }
        }
        seen
    }

    fn classify(&mut self, frame: &Bytes) -> Seen {
        let (name, payload) = parse_frame(frame);
        let Some(payload) = payload else {
            return Seen::Handshake;
        };
        let payload = payload.trim();
        if payload.is_empty() {
            return Seen::Handshake;
        }
        if self.usage.enabled() {
            self.usage.response_line(Format::Codex, payload.as_bytes());
        }
        // Classification reads a copy; the client receives `delivered_failure`.
        if let Some((error, _classified)) = response::terminal_failure(payload, self.model_level_cooling) {
            // #5: the status-less context-window `error` event, renamed for what it is.
            let deliver = self
                .codex_client
                .then(|| cpa_common::codex_client::context_window_failure_event(payload))
                .flatten()
                .and_then(|event| match event {
                    Cow::Owned(event) => Some(Bytes::from(match name {
                        Some(_) => format!("event: response.failed\ndata: {event}\n\n"),
                        None => format!("data: {event}\n\n"),
                    })),
                    Cow::Borrowed(_) => None,
                });
            let body = delivered_failure(payload);
            return Seen::Failure { error, body, deliver };
        }
        if response::meaningful_delta(payload) {
            self.saw_delta = true;
        }
        let kind = CodexBody::parse(payload.as_bytes())
            .ok()
            .and_then(|event| event.get("type").and_then(|t| t.text()).map(Cow::into_owned))
            .unwrap_or_default();
        match kind.as_str() {
            "response.output_item.done" => {
                self.items += 1;
                Seen::Content
            }
            "response.incomplete" if response::empty_incomplete(payload, self.items, self.saw_delta) => {
                Seen::EmptyIncomplete
            }
            "response.completed" | "response.incomplete" | "response.done" => Seen::Terminal,
            _ if response::bufferable(payload) => Seen::Handshake,
            _ => Seen::Content,
        }
    }

    /// A buffered answer from an SSE body: the terminal event's `response` object, byte
    /// for byte.
    fn aggregate(mut self, data: &[u8]) -> Result<Bytes, ExecError> {
        let mut framer = cpa_translate::sse::Framer::with_limit(data.len() + 1);
        let mut frames = framer.push(data).unwrap_or_default();
        frames.extend(framer.finish());
        for frame in frames {
            match self.see(&frame) {
                Seen::Terminal => {
                    let (_, payload) = parse_frame(&frame);
                    let payload = payload.unwrap_or_default();
                    let event = CodexBody::parse(payload.trim().as_bytes()).map_err(|_| response_unreadable())?;
                    let object = event.get("response").filter(|r| r.kind == Kind::Object);
                    return object
                        .map(|r| Bytes::copy_from_slice(r.raw))
                        .ok_or_else(response_unreadable);
                }
                Seen::EmptyIncomplete => {
                    return Err(response::request_scoped(502, response::EMPTY_INCOMPLETE_MESSAGE));
                }
                Seen::Failure { error, body, .. } => return Err(direct(error, body)),
                Seen::Handshake | Seen::Content => {}
            }
        }
        Err(response::request_scoped(408, response::INCOMPLETE_MESSAGE))
    }
}

fn response_unreadable() -> ExecError {
    ExecError::local(
        502,
        FailureScope::Request,
        "upstream terminal event has no response object",
    )
}

/// A failure event as a direct error: the scheduler reads its classification, and the
/// client receives `body` ([`delivered_failure`]). An in-stream failure arrived on an
/// HTTP 200, so the classified status is the only one there is to send.
fn direct(mut error: ExecError, body: Bytes) -> ExecError {
    error.body = body;
    error.direct = true;
    error.headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    error
}

struct Passthrough {
    upstream: ExecStream,
    observer: Observer,
    /// Leading handshake events not yet delivered, and the deadline for holding them.
    held: Vec<Bytes>,
    held_bytes: usize,
    /// Hold leading handshake events (`stream-bootstrap-buffering`).
    holding: bool,
    hold: Option<tokio::time::Instant>,
    /// Something reached the client: from here on nothing fails over.
    committed: bool,
    out: VecDeque<Result<Bytes, ExecError>>,
    done: bool,
}

impl Passthrough {
    fn commit(&mut self) {
        self.committed = true;
        self.out.extend(self.held.drain(..).map(Ok));
        self.held_bytes = 0;
    }

    fn take(&mut self, frame: Bytes) {
        match self.observer.see(&frame) {
            Seen::Handshake if self.holding && !self.committed => {
                self.held_bytes += frame.len();
                self.held.push(frame);
                if self.held.len() >= response::BOOTSTRAP_MAX_FRAMES || self.held_bytes >= response::BOOTSTRAP_MAX_BYTES
                {
                    self.commit();
                }
            }
            Seen::Handshake | Seen::Content => {
                self.commit();
                self.out.push_back(Ok(frame));
            }
            Seen::Terminal => {
                self.commit();
                self.out.push_back(Ok(terminated(frame)));
                self.done = true;
            }
            Seen::EmptyIncomplete if !self.committed => {
                self.held.clear();
                self.out
                    .push_back(Err(response::request_scoped(502, response::EMPTY_INCOMPLETE_MESSAGE)));
                self.done = true;
            }
            Seen::EmptyIncomplete => {
                self.out.push_back(Ok(terminated(frame)));
                self.done = true;
            }
            Seen::Failure { error, body, deliver } => {
                self.observer.wire.exec_error(&error);
                if !self.committed && error.scope != FailureScope::Request {
                    // Nothing was delivered: another credential may serve the request.
                    self.held.clear();
                    self.out.push_back(Err(direct(error, body)));
                } else {
                    // The client gets the provider's event; the error after it only ends
                    // the attempt and reports its outcome.
                    self.commit();
                    self.out.push_back(Ok(terminated(deliver.unwrap_or(frame))));
                    self.out.push_back(Err(error));
                }
                self.done = true;
            }
        }
    }

    fn end(&mut self, error: ExecError) {
        self.observer.wire.exec_error(&error);
        self.held.clear();
        self.out.push_back(Err(error));
        self.done = true;
    }
}

/// `frame` with the line endings that close an SSE event. Only a final event the upstream
/// left open can lack them; without them an SSE client drops that event at the end of the
/// stream. Transport framing, not payload: the event's own bytes are unchanged.
fn terminated(frame: Bytes) -> Bytes {
    let strip = |b: &[u8]| -> Option<usize> {
        if b.ends_with(b"\r\n") {
            Some(2)
        } else if b.ends_with(b"\n") || b.ends_with(b"\r") {
            Some(1)
        } else {
            None
        }
    };
    let missing: &[u8] = match strip(&frame) {
        // The last line already ended: closed when the line before it is blank.
        Some(n) if strip(&frame[..frame.len() - n]).is_some() => return frame,
        // One more of the same line ending (after a lone `\r`, `\n` would only make it
        // a CRLF).
        Some(_) if frame.ends_with(b"\n") => b"\n",
        Some(_) => b"\r",
        None => b"\n\n",
    };
    let mut out = Vec::with_capacity(frame.len() + missing.len());
    out.extend_from_slice(&frame);
    out.extend_from_slice(missing);
    Bytes::from(out)
}

/// Forwards upstream events unchanged (see the module docs). With `hold`, leading
/// handshake events wait for content, up to its deadline when it has one
/// (`stream-bootstrap-buffering` and `stream-bootstrap-timeout`); without, every event
/// is delivered at once. Either way a failure that arrives before anything was
/// delivered may fail over.
fn passthrough(upstream: ExecStream, observer: Observer, hold: Option<Option<Duration>>) -> ExecStream {
    let state = Passthrough {
        upstream,
        observer,
        held: Vec::new(),
        held_bytes: 0,
        holding: hold.is_some(),
        hold: hold.flatten().map(|d| tokio::time::Instant::now() + d),
        committed: false,
        out: VecDeque::new(),
        done: false,
    };
    futures_util::stream::unfold(state, |mut st| async move {
        loop {
            if let Some(item) = st.out.pop_front() {
                return Some((item, st));
            }
            if st.done {
                return None;
            }
            let next = match (st.committed || !st.holding, st.hold) {
                (false, Some(deadline)) => match tokio::time::timeout_at(deadline, st.upstream.next()).await {
                    Ok(next) => next,
                    Err(_) => {
                        st.commit();
                        continue;
                    }
                },
                _ => st.upstream.next().await,
            };
            match next {
                Some(Ok(frame)) => st.take(frame),
                Some(Err(error)) => st.end(error),
                // A clean end without a terminal event: a truncated response.
                None => st.end(response::request_scoped(408, response::INCOMPLETE_MESSAGE)),
            }
        }
    })
    .boxed()
}

#[cfg(test)]
#[path = "codex_native_tests.rs"]
mod tests;
