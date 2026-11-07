//! The Codex translated route (#27): a Claude Messages, Chat Completions, Gemini or
//! Interactions request served by a Codex credential on a `chatgpt`, `api-key` or
//! `compatible` profile (`docs/codex/CONTRACT.md` "Translated route").
//!
//! The request is converted into a Responses body by the built-in translator of its
//! pair, and nothing else edits it. No plugin request normalizer or plugin translator
//! runs, no payload rule, prompt-cache key, reasoning replay, input-ID rewrite, tool
//! injection or schema normalization, no namespace optimization and no default
//! `instructions`. The translator refuses, before the send, any content part the
//! destination cannot express. Two transport edits follow the conversion: the routed
//! `model`, and `stream: true`, since the response is translated from upstream events
//! either way.
//!
//! The response is converted back by the same pair's built-in translator, without
//! plugin response hooks, namespace restoration or a local token estimate in place of
//! provider usage. A buffered client receives the aggregate of the upstream's events.

use super::*;

/// The source formats the route converts. Each has a registered built-in pair to Codex.
const SOURCES: [Format; 4] = [Format::Claude, Format::OpenAI, Format::Gemini, Format::Interactions];

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
    /// One translated request (see the module documentation).
    pub(crate) async fn translated(
        &self,
        view: &View<'_>,
        settings: &Settings,
        req: ExecRequest,
    ) -> Result<ExecResponse, ExecError> {
        let model = request::base_model(&req.model).to_owned();
        let body = upstream_body(&req, &model)?;
        report_request(&req, Format::Codex, &body);
        // The handshake identity headers follow the native route's rule; #26 owns them.
        let cache = request::prompt_cache_id(&req, &body, None, false);
        let headers = request::http_headers(view, settings, &req.headers, &body, &model, cache.as_deref(), true);
        let url = format!("{}/responses", view.base_url);
        let wire = Wire::new(req.capture(), view.credential);
        let started = Instant::now();
        let upstream = self
            .open_for(
                view,
                settings,
                url,
                headers,
                Bytes::from(body.clone()),
                &wire,
                ErrorBody::Strict,
                &model,
                false,
            )
            .await?;
        let (status, headers) = (upstream.status, upstream.headers);
        let events = crate::upstream::framed(upstream.body);
        let processor = Processor::new(false, settings.model_level_cooling).reporting(req.usage.clone());
        if req.stream {
            let processor = processor.capturing(wire);
            let stream = if settings.bootstrap_buffering {
                match response::bootstrap(events, processor, settings.bootstrap_timeout, started).await {
                    Bootstrap::Reject(error) => return Err(error),
                    Bootstrap::Stream(stream) => stream,
                }
            } else {
                response::processed(events, processor, 0)
            };
            return Ok(ExecResponse {
                status,
                headers,
                body: ResponseBody::Stream(client_stream(&req, &body, stream)),
            });
        }
        let completed = aggregate(events, processor, &wire).await?;
        Ok(ExecResponse {
            status,
            headers,
            body: ResponseBody::Buffered(client_body(&req, &body, &completed)?),
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
    let out = cpa_translate::builtin_non_stream(req.response_format, Format::Codex, &ctx, completed.as_bytes())
        .ok_or_else(unregistered_response)?
        .map_err(translate_error)?;
    if out.is_empty() {
        return Err(ExecError::local(
            502,
            FailureScope::Request,
            cpa_translate::APPLY_PATCH_UPSTREAM_ERROR,
        ));
    }
    Ok(Bytes::from(out))
}

/// The client's stream: the built-in stream translation, its events as the translator
/// renders them. A Claude client's `message_start` carries what the translator knows; no
/// local token estimate stands in for provider usage.
fn client_stream(req: &ExecRequest, translated: &str, upstream: ExecStream) -> ExecStream {
    let ctx = response_ctx(req, translated);
    let Some(mut translator) = cpa_translate::builtin_stream(req.response_format, Format::Codex, &ctx) else {
        return futures_util::stream::once(async { Err(unregistered_response()) }).boxed();
    };
    // The server's stream interceptors frame the client's payloads themselves.
    if req.stream_payloads {
        translator.executor_chunks();
    }
    struct State {
        upstream: ExecStream,
        translator: Box<dyn cpa_translate::StreamTranslator>,
        ready: std::collections::VecDeque<Bytes>,
        failed: Option<ExecError>,
        done: bool,
    }
    futures_util::stream::unfold(
        State {
            upstream,
            translator,
            ready: Default::default(),
            failed: None,
            done: false,
        },
        |mut st| async move {
            loop {
                if let Some(event) = st.ready.pop_front() {
                    return Some((Ok(event), st));
                }
                if let Some(error) = st.failed.take() {
                    return Some((Err(error), st));
                }
                if st.done {
                    return None;
                }
                let result = match st.upstream.next().await {
                    Some(Ok(event)) => st.translator.event(&event).map_err(translate_error),
                    Some(Err(error)) => Err(error),
                    None => {
                        st.done = true;
                        st.translator.finish().map_err(translate_error)
                    }
                };
                match result {
                    Ok(events) => st.ready.extend(events),
                    Err(error) => {
                        // The pending client frame goes before the terminal error.
                        st.ready.extend(st.translator.flush_frames());
                        st.failed = Some(error);
                        st.done = true;
                    }
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
