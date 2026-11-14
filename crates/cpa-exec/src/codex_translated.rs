//! The Codex translated route (#27): a Claude Messages or Chat Completions request
//! served by a Codex credential on a `chatgpt`, `api-key` or `compatible` profile (`docs/codex/CONTRACT.md` "Translated route").
//!
//! The request is converted into a Responses body by the built-in translator of its
//! pair, and nothing else edits it. No payload rule, prompt-cache key, reasoning replay, input-ID rewrite, tool
//! injection or schema normalization, no namespace optimization and no default
//! `instructions`. The translator refuses, before the send, any content part the
//! destination cannot express. Two transport edits follow the conversion: the routed
//! `model`, and `stream: true`, since the response is translated from upstream events
//! either way.
//!
//! The response is converted back by the route's pair ([`cpa_translate::codex_response`]),
//! without namespace restoration or a local token estimate in place of provider usage. A buffered client receives the aggregate of the upstream's events.
//! What reaches the client passes the outbound credential guard ([`crate::codex_secret`]),
//! bound to the values the upstream request carried.

use std::collections::VecDeque;

use super::*;
use crate::codex_response::{Step, Steps};
use crate::codex_secret::Guard;

/// The source formats the route converts. Each has a registered built-in pair to Codex.
const SOURCES: [Format; 2] = [Format::Claude, Format::OpenAI];

/// Whether the executor serves `req` on the translated route: a generation from one of
/// [`SOURCES`] on a native profile. A request whose server hooks read unframed stream
/// payloads takes the route too: the hooks act on the client's own format, and the
/// conversion stays the built-in one ([`client_stream`] hands them the payloads).
pub(crate) fn route(view: &View<'_>, req: &ExecRequest) -> bool {
    req.operation == Operation::Generate
        && req.alt.is_none()
        && SOURCES.contains(&req.source_format)
        && native::Profile::of(view).is_some()
}

/// The Responses body for `req`: the built-in conversion, the client's own reasoning
/// choice (`thinking`, `reasoning_effort`, a thinking budget or a model suffix) as
/// `reasoning`, then the routed `model` and `stream: true`.
pub(crate) fn upstream_body(req: &ExecRequest, model: &str) -> Result<String, ExecError> {
    let ctx = cpa_translate::RequestCtx {
        model,
        stream: true,
        tool_search: false,
        system_turns: false,
    };
    let translated = cpa_translate::codex_request(req.source_format, &ctx, &req.body)
        .ok_or_else(|| {
            ExecError::local(
                501,
                FailureScope::Request,
                format!(
                    "{} -> codex request translation is not registered",
                    req.source_format.as_str()
                ),
            )
        })?
        .map_err(|e| ExecError::local(400, FailureScope::Request, e.to_string()))?;
    let caps = req
        .resolved_model
        .as_ref()
        .map(|r| cpa_common::thinking::ModelCaps::from(&r.info));
    let thought = cpa_common::thinking::apply_request_thinking(&cpa_common::thinking::RequestThinking {
        body: &translated,
        payload: &req.body,
        original: &req.original_body,
        model: &req.model,
        from: req.source_format.as_str(),
        to: Format::Codex.as_str(),
        provider: "codex",
        resolved: caps.as_ref().map(Some),
        has_request_transformer: true,
        updates_changed: false,
    })
    .map_err(|e| ExecError::local(e.status(), FailureScope::Request, e.message))?;
    let body = String::from_utf8(thought)
        .map_err(|_| ExecError::local(400, FailureScope::Request, "request body is not valid UTF-8 JSON"))?;
    let body = crate::codex_json::set_str_if_different(body, "model", model);
    Ok(crate::codex_json::set_bool_if_different(body, "stream", true))
}

impl CodexExecutor {
    /// One translated request (see the module documentation). The attempt is recorded as
    /// canceled when the client leaves before the response is ready: during the upstream
    /// send, the stream bootstrap or a buffered client's body read ([`crate::cancel`]).
    /// A stream settles its own outcome ([`client_stream`]). The answer, translated, is
    /// guarded against the authentication values the upstream request carried.
    pub(crate) async fn translated(
        &self,
        view: &View<'_>,
        settings: &Settings,
        req: ExecRequest,
    ) -> Result<ExecResponse, ExecError> {
        let usage = req.usage.clone();
        let mut secrets = cpa_core::exec::SentCredentials::default();
        let exchange = self.translated_exchange(view, settings, req, &mut secrets);
        let answer = crate::cancel::fail_if_canceled(&usage, exchange).await;
        secrets.guard_response(answer)
    }

    /// `secrets` receives the authentication values the upstream request carries.
    async fn translated_exchange(
        &self,
        view: &View<'_>,
        settings: &Settings,
        req: ExecRequest,
        secrets: &mut cpa_core::exec::SentCredentials,
    ) -> Result<ExecResponse, ExecError> {
        let model = request::base_model(&req.model).to_owned();
        // One buffer for the upstream send and the response translator: the send shares
        // it instead of copying the converted body (#29).
        let sent = Bytes::from(upstream_body(&req, &model)?);
        // The conversion produced a `String`, so the bytes are UTF-8.
        let body = std::str::from_utf8(&sent)
            .map_err(|_| ExecError::local(400, FailureScope::Request, "request body is not valid UTF-8 JSON"))?;
        settings.refuse_image_tool(&req.request_path, body.as_bytes())?;
        report_request(&req, Format::Codex, body);
        // The handshake identity headers follow the native route's rule; #26 owns them.
        let cache = request::prompt_cache_id(&req, body, None, false);
        // A translated request carries the proxy's own identity (#26).
        let headers = request::http_headers(
            view,
            &req.headers,
            body,
            &model,
            cache.as_deref(),
            true,
            crate::codex_identity::Origin::of(&req),
        );
        let url = format!("{}/responses", view.base_url);
        // The request log and the usage record keep this exchange scrubbed of them too.
        *secrets = crate::codex_secret::from_request(&headers, &url);
        let mut wire = Wire::new(req.capture(), view.credential);
        wire.bind(secrets);
        req.usage.sent_credentials(secrets);
        let started = Instant::now();
        let upstream = self
            .open_for(
                view,
                settings,
                url,
                headers,
                sent.clone(),
                &wire,
                ErrorBody::Strict,
                &model,
                false,
                false,
                None,
            )
            .await?;
        let (status, headers) = (upstream.status, upstream.headers);
        let events = crate::upstream::framed(upstream.body);
        let processor = Processor::new(false, settings.model_level_cooling).reporting(req.usage.clone());
        if req.stream {
            let processor = processor.capturing(wire);
            let (held, rest) = if settings.bootstrap_buffering {
                match response::bootstrap(events, processor, settings.bootstrap_timeout, started).await {
                    Bootstrap::Reject(error) => return Err(error),
                    Bootstrap::Stream { held, rest } => (held, rest),
                }
            } else {
                (VecDeque::new(), Some(response::processed(events, processor, 0)))
            };
            return Ok(ExecResponse {
                status,
                headers,
                body: ResponseBody::Stream(client_stream(&req, body, held, rest)),
                omitted: Default::default(),
            });
        }
        let completed = aggregate(events, processor, &wire).await?;
        Ok(ExecResponse {
            status,
            headers,
            body: ResponseBody::Buffered(client_body(&req, body, &completed)?),
            omitted: Default::default(),
        })
    }
}

/// The upstream's events for a buffered client, read whole and aggregated into the
/// terminal event (`Processor::buffered`), a read error's partial tail included.
async fn aggregate(mut events: ExecStream, mut processor: Processor, wire: &Wire) -> Result<String, ExecError> {
    let (mut data, mut failed) = (Vec::new(), None);
    while let Some(chunk) = events.next().await {
        match chunk {
            Ok(chunk) => data.extend_from_slice(&chunk),
            Err(error) => {
                failed = Some(error);
                break;
            }
        }
    }
    // Every byte that arrived is recorded, an unfinished tail included, then any read
    // error, as the buffered call sites do.
    wire.chunk(&data);
    if let Some(completed) = processor.buffered(&data)? {
        return Ok(completed);
    }
    if let Some(error) = &failed {
        wire.exec_error(error);
    }
    Err(response::request_scoped(408, response::INCOMPLETE_MESSAGE))
}

fn response_ctx<'a>(req: &'a ExecRequest, translated: &'a str) -> cpa_translate::ResponseCtx<'a> {
    cpa_translate::ResponseCtx {
        model: &req.model,
        original_request: &req.original_body,
        translated_request: translated.as_bytes(),
        tool_search: false,
    }
}

/// The client's buffered body: the built-in non-stream translation of the aggregate.
fn client_body(req: &ExecRequest, translated: &str, completed: &str) -> Result<Bytes, ExecError> {
    let ctx = response_ctx(req, translated);
    let pair = cpa_translate::codex_response(req.response_format).ok_or_else(unregistered_response)?;
    let out = (pair.non_stream)(&ctx, completed.as_bytes()).map_err(translate_error)?;
    if out.is_empty() {
        return Err(ExecError::local(
            502,
            FailureScope::Request,
            cpa_translate::APPLY_PATCH_UPSTREAM_ERROR,
        ));
    }
    Ok(Bytes::from(out))
}

/// The client's stream: the built-in stream translation of the `held` steps, then of
/// `rest` ([`Bootstrap`]), its events as the translator renders them. A Claude client's
/// `message_start` carries what the translator knows; no local token estimate stands in
/// for provider usage.
///
/// The stream owns the attempt's outcome if the consumer drops it ([`crate::cancel`]):
/// canceled until the upstream ends. The upstream ends when the stream observes its
/// terminal event (its usage is observed) or its clean end; or when it observes a
/// failure, which is the record until the consumer takes it. A step counts as observed
/// once it reaches this stream, so a held step, and client events still queued for the
/// consumer, keep the outcome of the step they came from.
fn client_stream(req: &ExecRequest, translated: &str, held: VecDeque<Step>, rest: Option<Steps>) -> ExecStream {
    let ctx = response_ctx(req, translated);
    let Some(pair) = cpa_translate::codex_response(req.response_format) else {
        return futures_util::stream::once(async { Err(unregistered_response()) }).boxed();
    };
    let translator = (pair.stream)(&ctx);
    let mut outcome = crate::cancel::StreamOutcome::new(req.usage.clone());
    match held.back() {
        Some(Step::Fail { error, .. }) => outcome.failed(error),
        // A held terminal event, or an upstream that ended with no event.
        _ if rest.is_none() => outcome.settled(),
        _ => {}
    }
    struct State {
        held: VecDeque<Step>,
        rest: Option<Steps>,
        translator: Box<dyn cpa_translate::StreamTranslator>,
        ready: VecDeque<Bytes>,
        failed: Option<ExecError>,
        done: bool,
        outcome: crate::cancel::StreamOutcome,
    }
    impl State {
        /// Ends the stream with `error`: the pending client frame, then the error.
        fn fail(&mut self, error: ExecError) {
            self.ready.extend(self.translator.flush_frames());
            self.outcome.failed(&error);
            self.failed = Some(error);
            self.done = true;
        }
    }
    futures_util::stream::unfold(
        State {
            held,
            rest,
            translator,
            ready: VecDeque::new(),
            failed: None,
            done: false,
            outcome,
        },
        |mut st| async move {
            loop {
                if let Some(event) = st.ready.pop_front() {
                    return Some((Ok(event), st));
                }
                if let Some(error) = st.failed.take() {
                    // The consumer settles the failure it takes (always the last item).
                    st.outcome.settled();
                    return Some((Err(error), st));
                }
                if st.done {
                    return None;
                }
                let step = match st.held.pop_front() {
                    Some(step) => Some(step),
                    None => match st.rest.as_mut() {
                        Some(rest) => rest.next().await,
                        None => None,
                    },
                };
                let (events, upstream_failure) = match step {
                    Some(Step::Event(event)) => (st.translator.event(&event), None),
                    Some(Step::Terminal(event)) => {
                        st.outcome.settled();
                        (st.translator.event(&event), None)
                    }
                    Some(Step::Fail { error, before, .. }) => {
                        // The lines that preceded the failure in its event go first; the
                        // failure then ends the stream (`State::fail`).
                        let events = before.map_or_else(|| Ok(Vec::new()), |before| st.translator.event(&before));
                        (events, Some(error))
                    }
                    None => {
                        st.done = true;
                        st.outcome.settled();
                        (st.translator.finish(), None)
                    }
                };
                match events {
                    Ok(events) => st.ready.extend(events),
                    Err(error) => {
                        st.fail(translate_error(error));
                        continue;
                    }
                }
                if let Some(error) = upstream_failure {
                    st.fail(error);
                }
            }
        },
    )
    .boxed()
}

fn unregistered_response() -> ExecError {
    ExecError::local(
        501,
        FailureScope::Request,
        "codex response translation is not registered",
    )
}

#[cfg(test)]
#[path = "codex_translated_tests.rs"]
mod tests;
