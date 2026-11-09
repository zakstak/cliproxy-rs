//! The native Codex HTTP route (#23, and the HTTP half of #25): a Responses body from a
//! Responses client, sent over HTTP to any Codex endpoint profile
//! (`docs/codex/CONTRACT.md` "Native HTTP route"). The predicates and request edits of
//! the native WebSocket route (#24) live here too; its socket handling is in
//! `codex_ws.rs` and `codex_duplex.rs` ("Native WebSocket route").
//!
//! The request goes upstream as the client sent it. The only edits are the contract's
//! permitted transformations: the upstream `model` of an alias, and `stream: true` for a
//! buffered client on the `chatgpt` profile, whose buffered support is unknown. Nothing
//! else runs: no translation, thinking, payload rules, prompt-cache key, reasoning
//! replay, tool or namespace preparation, or Sign in with ChatGPT rules (#28).
//!
//! A WebSocket turn on a credential without `websockets` also comes here
//! ([`CodexExecutor::native_ws_http`]): the first-party `response.create` frame is the
//! HTTP request body plus its `type` tag (`codex-api/src/common.rs:315-339`), so the
//! frame goes upstream without `type`, with its `model` routed and `stream: true` (a
//! WebSocket turn always delivers events).
//!
//! The response reaches the client as the upstream sent it. A stream's events are
//! forwarded byte for byte; observation (usage, capture, failure classification) reads
//! a copy. Only an event the pinned client decodes ([`codex_event`]) can end, fail or
//! convert the response or feed usage. Leading handshake events are held, never changed, so that a failure before
//! any content can still fail over to another credential. Once anything is delivered,
//! a failure ends the stream after its event and is never replayed elsewhere. A
//! buffered `chatgpt` answer is the `response` object of the terminal event, byte for
//! byte. One exception stays (`docs/DIFFERENCES-FROM-GO.md`): a Codex client gets the
//! backend's status-less context-window `error` event as `response.failed` (#5). Every
//! `response.incomplete` reaches the client as sent, whatever its reason, output or
//! usage, before or after delivery (#25). The pinned client classifies its reason as an
//! interrupted end, a content-filter error or a terminal `ResponseIncomplete`
//! (`codex-api/src/sse/responses.rs:451-506`). The response ends where the client's
//! reader ends it ([`codex_event::Outcome`]): an SSE client reads past a processing error
//! (a content filter, or a `response` that does not read as `ResponseCompleted`) to a
//! later terminal event, and ends on it only when the stream ends; a WebSocket turn served
//! over HTTP ends on it, as the WebSocket reader does.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::time::Duration;

use cpa_common::codex_body::{CodexBody, Kind};

use super::*;
use crate::codex_event;

/// The endpoint profiles. Every profile takes the native and translated routes (#28).
/// Only `chatgpt` gets the buffered `stream: true` adaptation; the others pass the
/// client's `stream` choice through, as the OpenAI API documents it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Profile {
    Chatgpt,
    ApiKey,
    Compatible,
    /// Sign in with ChatGPT. No first-party source covers its Responses behaviour, so
    /// the proxy adds nothing: Go's request rules for it (`siwc_body`) were deleted.
    Siwc,
}

impl Profile {
    /// The profile follows the credential: a ChatGPT OAuth token is `chatgpt` whatever
    /// its base URL (a relay in front of the backend), an API key is `api-key` on
    /// `api.openai.com` and `compatible` anywhere else.
    pub(crate) fn of(view: &View<'_>) -> Option<Self> {
        if view.siwc {
            return Some(Self::Siwc);
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

/// Whether the executor serves `req` natively. Every eligible request does.
pub(crate) fn route(view: &View<'_>, req: &ExecRequest) -> Option<Profile> {
    eligible(view, req)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call {
    Stream,
    Buffered,
    Compact,
}

/// Whether a WebSocket turn on `view` may take the native route (#24): an eligible
/// request whose frame is a `response.create`, the one request frame the pinned client
/// sends (`codex-api/src/common.rs:400-406`). That covers a first turn, a continuation
/// with `previous_response_id`, a prewarm with `generate: false`, and a create the
/// client sends while a response runs. A credential with `websockets` carries it on a
/// socket; one without sends a first turn over HTTP and refuses the rest
/// ([`CodexExecutor::native_ws_http`]). Any other frame, such as the Go-defined
/// `response.append`, is refused ([`refused_frame`]). The server decides with this
/// ([`CodexExecutor::native_ws`]); `frame` is the client's frame.
pub(crate) fn eligible_ws(view: &View<'_>, req: &ExecRequest, frame: &[u8]) -> Option<Profile> {
    eligible(view, req).filter(|_| creates(frame))
}

/// Whether the executor serves a WebSocket turn natively, as [`route`] does for HTTP.
pub(crate) fn route_ws(view: &View<'_>, req: &ExecRequest) -> Option<Profile> {
    eligible_ws(view, req, &req.body)
}

/// The refusal of a WebSocket turn no Codex route carries: a frame other than
/// `response.create` (such as Go's `response.append`, which no first-party or documented
/// source defines), or a turn whose formats are not Responses on both sides.
pub(crate) fn refused_frame(frame: &[u8]) -> ExecError {
    let kind = cpa_common::json::get(frame, "type");
    let message = match &*kind.str() {
        "response.append" => APPEND_REFUSED.to_owned(),
        "" | "response.create" => {
            "Codex WebSocket turns must be Responses `response.create` frames from a Responses client".to_owned()
        }
        other => format!(
            "`{other}` is not supported on Codex credentials: no first-party or documented source defines it; send a `response.create` (with `previous_response_id` to continue a response)"
        ),
    };
    ExecError::local(400, FailureScope::Request, message)
}

/// The refusal of Go's `response.append` frame, on a turn and inside a steering
/// connection alike.
pub const APPEND_REFUSED: &str = "`response.append` is not supported on Codex credentials: no first-party or documented source defines it; send a `response.create` (with `previous_response_id` to continue a response)";

/// The HTTP body of a WebSocket `response.create` frame: the frame without its `type`
/// tag, with `model` routed and `stream: true`. A WebSocket turn always delivers its
/// response as events, and the first-party frame carries the HTTP request's own `stream`
/// (`codex-api/src/common.rs:315-339`), so a frame that omits it or sets `false` asks for
/// the event stream over HTTP: the transport adaptation of docs/codex/CONTRACT.md. A
/// continuation (`previous_response_id`) needs the state of an upstream socket this
/// credential does not keep, so it is refused with the replay close; a prewarm
/// (`generate: false`) has no HTTP meaning and is refused.
pub(crate) fn ws_http_body(frame: &Bytes, model: &str) -> Result<Bytes, ExecError> {
    let body = CodexBody::parse(frame)?;
    if body.get("previous_response_id").is_some_and(|m| m.kind != Kind::Null) {
        return Err(ExecError::replay_required());
    }
    if body.get("generate").is_some_and(|m| m.kind == Kind::False) {
        return Err(ExecError::local(
            400,
            FailureScope::Request,
            "a prewarm (`generate: false`) needs an upstream WebSocket; enable `websockets` on the Codex credential or send the turn without `generate`",
        ));
    }
    // One pass and at most one copy: drop `type`, route `model`, ask for the stream.
    match body.routed_without(model, true, "type") {
        Cow::Borrowed(_) => Ok(frame.clone()),
        Cow::Owned(bytes) => Ok(Bytes::from(bytes)),
    }
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
    match body.routed(model, false) {
        Cow::Borrowed(_) => Ok(frame.clone()),
        Cow::Owned(bytes) => Ok(Bytes::from(bytes)),
    }
}

/// The upstream body: the client's bytes with only the permitted edits.
fn upstream_body(req: &ExecRequest, profile: Profile, call: Call, model: &str) -> Result<Bytes, ExecError> {
    let body = CodexBody::parse(&req.body)?;
    match body.routed(model, call == Call::Buffered && profile == Profile::Chatgpt) {
        Cow::Borrowed(_) => Ok(req.body.clone()),
        Cow::Owned(bytes) => Ok(Bytes::from(bytes)),
    }
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
        let usage = req.usage.clone();
        let send = self.native_send(view, settings, req, profile, call, model, body, None);
        crate::cancel::fail_if_canceled(&usage, send).await
    }

    /// A WebSocket turn on a credential without `websockets`: the client's
    /// `response.create` frame as an HTTP stream ([`ws_http_body`]), with the downstream
    /// connection as prompt-cache identity.
    pub(crate) async fn native_ws_http(
        &self,
        view: &View<'_>,
        settings: &Settings,
        req: ExecRequest,
        profile: Profile,
        session: &str,
    ) -> Result<ExecResponse, ExecError> {
        let model = upstream_model(&req)?;
        let body = ws_http_body(&req.body, &model)?;
        let usage = req.usage.clone();
        let send = self.native_send(view, settings, req, profile, Call::Stream, model, body, Some(session));
        crate::cancel::fail_if_canceled(&usage, send).await
    }

    /// The exchange up to the response: the upstream send, then either the stream, or the
    /// whole body for a buffered client. The callers record the attempt as canceled when
    /// the client leaves before this returns ([`crate::cancel`]); a stream settles its own
    /// outcome ([`passthrough`]).
    #[allow(clippy::too_many_arguments)]
    async fn native_send(
        &self,
        view: &View<'_>,
        settings: &Settings,
        req: ExecRequest,
        profile: Profile,
        call: Call,
        model: String,
        body: Bytes,
        ws_session: Option<&str>,
    ) -> Result<ExecResponse, ExecError> {
        settings.refuse_image_tool(&req.request_path, &body)?;
        // The body passed the boundary, so this never fails.
        let text = std::str::from_utf8(&body)
            .map_err(|_| ExecError::local(400, FailureScope::Request, "request body is not valid UTF-8"))?;
        let upstream_stream = call == Call::Stream || (call == Call::Buffered && profile == Profile::Chatgpt);
        let cache = request::prompt_cache_id(&req, text, ws_session, false);
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
        let (sse, upstream) = classify_response(upstream, upstream_stream, &wire).await?;
        let observer = Observer {
            usage: req.usage.clone(),
            wire,
            capture_lines: call == Call::Stream,
            model_level_cooling: settings.model_level_cooling,
            codex_client: request::codex_client(&req),
        };
        let (status, headers) = (upstream.status, upstream.headers);
        if ws_session.is_some() && !sse {
            // A WebSocket turn asked for the event stream; a success without one has no
            // events to deliver, and the proxy invents none. Nothing reached the client,
            // so another credential may serve the turn.
            let error = ExecError::local(
                502,
                FailureScope::Credential,
                "codex upstream answered a WebSocket turn without an event stream (`stream: true` was requested)",
            );
            observer.wire.exec_error(&error);
            return Err(error);
        }
        if call == Call::Stream && sse {
            let hold = settings.bootstrap_buffering.then_some(settings.bootstrap_timeout);
            let mut stream = passthrough(
                crate::upstream::framed(upstream.body),
                observer,
                hold,
                ws_session.is_some(),
            );
            if ws_session.is_some() {
                // A WebSocket client takes each event's payload as one text frame.
                stream = stream.filter_map(|item| async move { unframed(item) }).boxed();
            }
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
            match observer.aggregate(&data, failed.is_none()) {
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

/// Missing MIME is not evidence of a JSON aggregate: the ChatGPT backend can omit
/// it on a valid event stream. Inspect only enough framing to choose the reader,
/// then replay the original chunks. Explicit media types remain authoritative.
async fn classify_response(
    mut upstream: crate::proxy::Upstream,
    expected_stream: bool,
    wire: &Wire,
) -> Result<(bool, crate::proxy::Upstream), ExecError> {
    if let Some(mime) = upstream.headers.get(http::header::CONTENT_TYPE) {
        let sse = mime.to_str().ok().is_some_and(|v| {
            v.split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .eq_ignore_ascii_case("text/event-stream")
        });
        return Ok((sse, upstream));
    }
    if !expected_stream {
        return Ok((false, upstream));
    }
    // ponytail: at most 64 KiB of leading empty lines before a standard SSE field.
    // An upstream needing more should declare text/event-stream instead.
    const PREFIX_LIMIT: usize = 64 * 1024;
    let mut prefix = Vec::new();
    let mut chunks = Vec::new();
    let mut start = 0;
    let sse = loop {
        if let Some(sse) = sse_prefix(&prefix, &mut start) {
            break sse;
        }
        if prefix.len() == PREFIX_LIMIT {
            break false;
        }
        match upstream.body.next().await {
            Some(Ok(chunk)) => {
                if !chunk.is_empty() {
                    prefix.extend_from_slice(&chunk[..chunk.len().min(PREFIX_LIMIT - prefix.len())]);
                    chunks.push(chunk);
                }
            }
            Some(Err(error)) => {
                for chunk in &chunks {
                    wire.chunk(chunk);
                }
                wire.exec_error(&error);
                return Err(error);
            }
            None => break false,
        }
    };
    upstream.body = futures_util::stream::iter(chunks.into_iter().map(Ok))
        .chain(upstream.body)
        .boxed();
    Ok((sse, upstream))
}

/// Only framing is inspected; event payloads are never parsed or rewritten here.
/// The cursor makes incremental inspection linear even with one-byte chunks.
fn sse_prefix(prefix: &[u8], start: &mut usize) -> Option<bool> {
    if *start == 0 && prefix.first() == Some(&0xef) {
        if prefix.len() < 3 {
            return None;
        }
        if !prefix.starts_with(b"\xef\xbb\xbf") {
            return Some(false);
        }
        *start = 3;
    }
    while prefix.get(*start).is_some_and(|b| matches!(b, b'\r' | b'\n')) {
        *start += 1;
    }
    let head = &prefix[*start..];
    if head.is_empty() {
        return None;
    }
    let fields: &[&[u8]] = &[b"data:", b"event:", b"id:", b"retry:", b":"];
    if fields.iter().any(|field| head.starts_with(field)) {
        Some(true)
    } else if fields.iter().any(|field| field.starts_with(head)) {
        None
    } else {
        Some(false)
    }
}

/// What one upstream event means for delivery.
enum Seen {
    /// A leading event with no content yet (`response.created` and the like).
    Handshake,
    Content,
    /// The provider's terminal event, delivered as sent: one both pinned readers end the
    /// response on ([`codex_event::Outcome::Ends`]), a readable completion or a
    /// `response.incomplete` with any reason but interrupted and content filter. A
    /// `response.completed` without a `response` and a `response.done` end nothing: the
    /// pinned reader reads on after them.
    Terminal,
    /// A `response.incomplete` or `response.completed` the pinned readers take as a
    /// processing error ([`codex_event::Outcome::Recorded`]): a content filter, or a
    /// `response` that does not read as `ResponseCompleted`. Delivered as sent, as the
    /// provider's event. Its usage is not observed yet: whether the response ends on it
    /// depends on the client's reader ([`Passthrough::take`], [`Observer::aggregate`]).
    Recorded,
    /// An event the pinned client does not decode ([`codex_event::decode`]): invalid
    /// JSON, a repeated known member, or a known member of the wrong type. The pinned
    /// client skips it, so it never ends, fails or converts the response and feeds no
    /// usage. It passes as sent, except where [`Passthrough::undecoded`] withholds it.
    Undecoded,
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
    // A leading UTF-8 BOM belongs to SSE framing, not the first field name.
    // Strip it only from this parsing view; the original frame still passes through.
    let frame = frame.strip_prefix(b"\xef\xbb\xbf").unwrap_or(frame);
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

/// An event as the payload a WebSocket client reads as one text frame: its `data`,
/// unchanged. An event without data (a comment) carries nothing for it.
fn unframed(item: Result<Bytes, ExecError>) -> Option<Result<Bytes, ExecError>> {
    match item {
        Ok(frame) => match parse_frame(&frame).1 {
            Some(Cow::Borrowed(data)) => Some(Ok(frame.slice_ref(data.as_bytes()))),
            Some(Cow::Owned(data)) => Some(Ok(Bytes::from(data))),
            None => None,
        },
        Err(error) => Some(Err(error)),
    }
}

/// What the client receives for a failure it does not see as an event (a buffered
/// answer, or a failure before anything was delivered): the provider's error object in
/// the API's error envelope, byte for byte, with the event's `sequence_number` as
/// spelled; the whole event when it carries no single error object. Nothing is filled
/// in. `event` is the decoded event ([`codex_event::decode`]), so its `response` and
/// `error` members are unique; a `response` with more than one `error` member is
/// delivered whole rather than one of them picked.
fn delivered_failure(event: &codex_event::Event<'_>, payload: &str) -> Bytes {
    let whole = || Bytes::copy_from_slice(payload.as_bytes());
    let mut nested = None;
    if let Some(response) = event.response.filter(|r| r.starts_with('{')) {
        let mut repeated = false;
        rawjson::parse(response).each(|key, value| {
            if key.str() == "error" {
                repeated |= nested.is_some();
                nested = Some(value);
            }
            !repeated
        });
        if repeated {
            return whole();
        }
    }
    let nested = nested.filter(|v| v.kind() == rawjson::Kind::Object);
    let top = event.error.filter(|e| e.starts_with('{'));
    let Some(error) = nested.as_ref().map(rawjson::Value::json).or(top) else {
        return whole();
    };
    let mut out = Vec::with_capacity(error.len() + 48);
    out.extend_from_slice(b"{\"error\":");
    out.extend_from_slice(error.as_bytes());
    let sequence = rawjson::get(payload, "sequence_number");
    if sequence.exists() {
        out.extend_from_slice(b",\"sequence_number\":");
        out.extend_from_slice(sequence.json().as_bytes());
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
            let last = matches!(seen, Seen::Terminal | Seen::Failure { .. });
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
        let (name, data) = parse_frame(frame);
        let Some(data) = data else {
            return Seen::Handshake;
        };
        if data.trim().is_empty() {
            return Seen::Handshake;
        }
        // The upstream event boundary comes first: an event the pinned client skips
        // ends, fails, converts and accounts for nothing here either.
        let Some(event) = codex_event::decode(&data) else {
            return Seen::Undecoded;
        };
        let payload = data.trim();
        let outcome = event.outcome();
        if self.usage.enabled() && event.accounted() && outcome != codex_event::Outcome::Recorded {
            self.usage.response_line(Format::Codex, payload.as_bytes());
        }
        // Classification reads a copy, as the pinned reader decodes the event: a struct
        // written as a JSON array by its members, the last of a repeated key and the
        // private values resolved (`codex_event::classification_view`). The client
        // receives `delivered_failure`.
        let view = match event.kind.as_ref() {
            "error" | "response.failed" => codex_event::classification_view(payload),
            _ => Cow::Borrowed(payload),
        };
        if let Some((error, _classified)) = response::terminal_failure(&view, self.model_level_cooling) {
            // #5: the status-less context-window `error` event, renamed for what it is.
            let deliver = self
                .codex_client
                .then(|| codex_event::context_window_event(payload))
                .flatten()
                .and_then(|event| match event {
                    Cow::Owned(event) => Some(Bytes::from(match name {
                        Some(_) => format!("event: response.failed\ndata: {event}\n\n"),
                        None => format!("data: {event}\n\n"),
                    })),
                    Cow::Borrowed(_) => None,
                });
            let body = delivered_failure(&event, payload);
            return Seen::Failure { error, body, deliver };
        }
        match (event.kind.as_ref(), outcome) {
            ("response.output_item.done", _) => Seen::Content,
            (_, codex_event::Outcome::Ends) => Seen::Terminal,
            (_, codex_event::Outcome::Recorded) => Seen::Recorded,
            _ if response::bufferable(payload) => Seen::Handshake,
            _ => Seen::Content,
        }
    }

    /// Usage of a [`Seen::Recorded`] event the response ends on after all.
    fn account(&self, frame: &Bytes) {
        if self.usage.enabled()
            && let Some(data) = parse_frame(frame).1
        {
            self.usage.response_line(Format::Codex, data.trim().as_bytes());
        }
    }

    /// A buffered answer from an SSE body: the terminal event's `response` object, byte
    /// for byte. A buffered client reads the SSE upstream as the pinned SSE reader does:
    /// past a processing error ([`Seen::Recorded`]) to a later terminal event, and on
    /// the last such error when the stream ends `clean`ly without one
    /// (`sse/responses.rs:616-621`). A read failure before any terminal event fails the
    /// answer as a stream without one does, whatever was recorded: that reader returns a
    /// read error at once (lines 602-614). A terminal event or failure before the read
    /// failure still decides the answer.
    fn aggregate(mut self, data: &[u8], clean: bool) -> Result<Bytes, ExecError> {
        let mut framer = cpa_translate::sse::Framer::with_limit(data.len() + 1);
        let mut frames = framer.push(data).unwrap_or_default();
        frames.extend(framer.finish());
        let mut recorded = None;
        for frame in frames {
            match self.see(&frame) {
                Seen::Terminal => return response_object(&frame),
                Seen::Recorded => recorded = Some(frame),
                Seen::Failure { error, body, .. } => return Err(direct(error, body)),
                Seen::Handshake | Seen::Content | Seen::Undecoded => {}
            }
        }
        match recorded.filter(|_| clean) {
            Some(frame) => {
                self.account(&frame);
                response_object(&frame)
            }
            None => Err(response::request_scoped(408, response::INCOMPLETE_MESSAGE)),
        }
    }
}

/// The `response` object of a terminal event [`Observer::see`] decoded (its `response`
/// member is unique), byte for byte.
fn response_object(frame: &Bytes) -> Result<Bytes, ExecError> {
    let (_, data) = parse_frame(frame);
    let data = data.unwrap_or_default();
    let response = codex_event::decode(&data).and_then(|event| event.response);
    match response.filter(|r| r.starts_with('{')) {
        Some(object) => Ok(Bytes::copy_from_slice(object.as_bytes())),
        None => Err(response_unreadable()),
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
    /// A WebSocket turn served over HTTP: a failure follows the native WebSocket rule
    /// instead of reaching the client as an event (see [`Passthrough::fail_websocket`]).
    websocket: bool,
    out: VecDeque<Result<Bytes, ExecError>>,
    done: bool,
    /// The last [`Seen::Recorded`] event delivered to an SSE client, whose reader reads
    /// past it: the response ends on it, with its usage, only if the upstream ends without
    /// a later terminal event or failure.
    recorded: Option<Bytes>,
    /// The attempt's record if the consumer drops the stream: canceled until the
    /// upstream completes or the consumer takes the failure that ends it.
    outcome: crate::cancel::StreamOutcome,
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
            Seen::Undecoded => self.undecoded(frame),
            Seen::Terminal => {
                self.commit();
                self.out.push_back(Ok(terminated(frame)));
                self.done = true;
            }
            // A WebSocket client's reader returns the processing error: its turn ends on
            // the event (docs/codex/CONTRACT.md "Responses and failures").
            Seen::Recorded if self.websocket => {
                self.observer.account(&frame);
                self.commit();
                self.out.push_back(Ok(terminated(frame)));
                self.done = true;
            }
            // An SSE client's reader records it and reads on, to a later terminal event or
            // the end of the stream (`sse/responses.rs:616-621` and 677-690). An event the
            // upstream left open can only be its last.
            Seen::Recorded => {
                self.commit();
                self.out.push_back(Ok(terminated(frame.clone())));
                self.recorded = Some(frame);
            }
            Seen::Failure { error, deliver, .. } if self.websocket => {
                self.observer.wire.exec_error(&error);
                self.fail_websocket(error, frame, deliver);
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

    /// A failure on a WebSocket turn served over HTTP, as the native WebSocket route
    /// delivers one (`codex_ws.rs`, docs/codex/CONTRACT.md "Responses and failures"): the
    /// #5 context-window event a Codex client receives goes ahead of the error; any other
    /// failure travels on the error as the upstream sent it, for delivery once failover is
    /// decided; an account failure (401, 402, 403, 429) never reaches the client as sent,
    /// because its frame can name the pooled account.
    fn fail_websocket(&mut self, mut error: ExecError, frame: Bytes, deliver: Option<Bytes>) {
        // Nothing was delivered and another credential may serve the turn: the held
        // handshake events go with the attempt, as on the HTTP route.
        let retry = !self.committed && error.scope != FailureScope::Request;
        if retry {
            self.held.clear();
        } else {
            self.commit();
        }
        // #5 as `codex_ws.rs` `preserved_event`: a Codex client gets the context-window
        // event ahead of the error, renamed when it was the status-less `error`.
        let preserved = deliver.or_else(|| {
            let context = self.observer.codex_client
                && parse_frame(&frame)
                    .1
                    .is_some_and(|data| codex_event::context_window_event(data.trim()).is_some());
            context.then(|| frame.clone())
        });
        match preserved {
            Some(event) if !retry => self.out.push_back(Ok(event)),
            _ if crate::codex_ws::account_failure(error.status) => {}
            _ => {
                if let Some(data) = parse_frame(&frame).1 {
                    error.delivered_event = Some(Box::new(Bytes::copy_from_slice(data.as_bytes())));
                }
            }
        }
        self.out.push_back(Err(error));
        self.done = true;
    }

    /// An event the pinned client does not decode. It is delivered as sent, and it
    /// counts as delivered content: the client may show it, so nothing fails over after
    /// it. A WebSocket client never gets one that would be an account failure if it
    /// decoded (`codex_ws::account_shaped`), and the shared capture still records it.
    fn undecoded(&mut self, frame: Bytes) {
        if self.websocket
            && parse_frame(&frame)
                .1
                .is_some_and(|data| crate::codex_ws::account_shaped(data.trim(), false))
        {
            return;
        }
        self.commit();
        self.out.push_back(Ok(frame));
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
fn passthrough(
    upstream: ExecStream,
    observer: Observer,
    hold: Option<Option<Duration>>,
    websocket: bool,
) -> ExecStream {
    let outcome = crate::cancel::StreamOutcome::new(observer.usage.clone());
    let state = Passthrough {
        upstream,
        observer,
        held: Vec::new(),
        held_bytes: 0,
        holding: hold.is_some(),
        hold: hold.flatten().map(|d| tokio::time::Instant::now() + d),
        committed: false,
        websocket,
        out: VecDeque::new(),
        done: false,
        recorded: None,
        outcome,
    };
    futures_util::stream::unfold(state, |mut st| async move {
        loop {
            if let Some(item) = st.out.pop_front() {
                if item.is_err() {
                    // The consumer settles the failure it takes (always the last item).
                    st.outcome.settled();
                }
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
                Some(Err(error)) => {
                    // The client has the provider's last processing error, and the read
                    // failure still ends the attempt: its record is failed, and a failure
                    // record carries no tokens (the server's `PublishFailure`), whatever
                    // the event reported.
                    if let Some(frame) = st.recorded.take() {
                        st.observer.account(&frame);
                    }
                    st.end(error)
                }
                // A clean end after a processing error the client recorded: the response
                // ended on it, as the pinned SSE reader reports at the end of the stream.
                // Nothing is added: the client reads that end from the event it has.
                None => match st.recorded.take() {
                    Some(frame) => {
                        st.observer.account(&frame);
                        st.done = true;
                    }
                    // A clean end without a terminal event: a truncated response.
                    None => st.end(response::request_scoped(408, response::INCOMPLETE_MESSAGE)),
                },
            }
            if st.done {
                // The upstream response ended: a terminal event (its usage is observed),
                // or a failure queued behind any event delivered with it.
                match st.out.back() {
                    Some(Err(error)) => st.outcome.failed(error),
                    _ => st.outcome.settled(),
                }
            }
        }
    })
    .boxed()
}

#[cfg(test)]
#[path = "codex_native_tests.rs"]
mod tests;
