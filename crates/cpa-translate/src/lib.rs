//! Translation between wire formats (internal/translator, sdk/translator).
//!
//! Executors call [`translate_request`] (sdk/translator's TranslateRequest: the registered
//! pair wrapped in the summary pipeline, or the model-rewrite fallback) and [`pair`] for
//! the response transforms of `(client format, upstream format)`. Same-format pairs may
//! be registered for normalization (Go registers OpenAI -> OpenAI).
//!
//! Contract for every pair:
//! - `request` turns a client body into an upstream body (the bare Go converter; use
//!   [`translate_request`] for the registry wrapper).
//! - `non_stream` turns one buffered upstream body into one client body. The input may
//!   be buffered SSE when the executor streamed upstream for a non-streaming client
//!   (claude_executor_execute.go asks Claude for SSE whenever formats differ).
//! - `stream` builds request-local state that turns framed upstream events (see
//!   [`sse::Framer`]) into zero or more client events, then flushes on `finish`. Output
//!   bytes are framed the way the client's Go route handler writes them ([`stream::frame`]).
//! - [`token_count`] renders an upstream token count in the client's shape (Go's
//!   TokenCount); `count_tokens` is the older body-based form and stays unset.
//! - Go's plugin translator hooks are not part of this registry: native plugins are
//!   removed (docs/DIFFERENCES-FROM-GO.md "Native plugins").
//! - Response transforms run after the executor has reversed provider rewrites (tool
//!   aliases, cloak) and see the client's original request and the translated request
//!   from before those rewrites.
//!
//! JSON is read and edited with `cpa_common::json`, a byte-exact port of the gjson/sjson
//! behaviour Go's translators depend on, so malformed bytes, coercions and escaping
//! match Go.

/// One Go registration (`translator.Register`). Every transform runs on the caller's
/// thread: walkers of nested JSON keep their open containers on the heap and stop at
/// [`cpa_common::json::WALK_DEPTH_LIMIT`] (issue #146), so no input depth needs a larger
/// native stack.
macro_rules! registered {
    ($client:ident -> $upstream:ident, request: $request:expr, non_stream: $non_stream:expr, go_stream: $go_stream:expr, token_count: $token_count:expr $(,)?) => {
        $crate::Registered {
            pair: $crate::Pair {
                request: $request,
                non_stream: $non_stream,
                stream: |ctx| {
                    $crate::stream::framed(
                        cpa_core::format::Format::$client,
                        cpa_core::format::Format::$upstream,
                        ($go_stream)(ctx),
                    )
                },
                count_tokens: None,
            },
            token_count: $token_count,
            go_stream: $go_stream,
        }
    };
}

mod apply_patch;
pub mod apply_patch_responses;
mod claude_chat_request;
mod claude_chat_response;
mod claude_fallback;
mod claude_responses;
mod claude_responses_response;
mod codex_chat_request;
mod codex_chat_response;
mod codex_claude;
mod codex_claude_response;
mod codex_responses;
mod codex_strict;
mod common;
mod openai;
mod openai_claude;
mod openai_claude_response;
mod openai_responses;
mod openai_responses_response;
mod responses_controls;
mod responses_tools;
pub mod sse;
pub mod stream;
mod thinking;

#[doc(hidden)]
pub use codex_responses::go_stream_with_bridge as codex_responses_go_stream_with_bridge;
pub use codex_responses::{
    ResponsesClient as CodexResponsesClient, non_stream_with_bridge as codex_responses_non_stream_with_bridge,
    stream_with_bridge as codex_responses_stream_with_bridge,
};

/// util.SanitizedFunctionNameMap, for executors that key tool schemas like Go.
pub fn sanitized_function_name_map(raw: &[u8]) -> std::collections::HashMap<Vec<u8>, Vec<u8>> {
    responses_tools::sanitized_function_name_map(raw)
}

/// util.MapSanitizedFunctionName.
pub fn map_sanitized_function_name(map: &std::collections::HashMap<Vec<u8>, Vec<u8>>, name: &[u8]) -> Vec<u8> {
    responses_tools::map_sanitized_function_name(map, name)
}

// Go's `...WithCompat` request converters, exported beside the registered pairs for
// compatibility endpoints.

/// ConvertOpenAIResponsesRequestToOpenAIChatCompletions as Go's Chat Completions handler
/// calls it: the converted body, even when the conversion would refuse an emptied turn.
pub fn responses_to_chat_completions_body(model: &str, body: &[u8], stream: bool) -> Vec<u8> {
    openai_responses::convert_with_refusal(model, body, stream).0
}

/// ConvertOpenAIRequestToClaudeWithCompat.
pub fn openai_to_claude_with_compat(ctx: &RequestCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    claude_chat_request::request_with_compat(ctx, body)
}

/// ConvertClaudeRequestToCodexWithCompat.
pub fn claude_to_codex_with_compat(ctx: &RequestCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    codex_claude::request_with_compat(ctx, body)
}

/// ConvertClaudeRequestToOpenAIWithCompat.
pub fn claude_to_openai_with_compat(ctx: &RequestCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    openai_claude::request_with_compat(ctx, body)
}

/// ConvertOpenAIResponsesRequestToClaudeWithCompat: like the registered Responses ->
/// Claude request, but unsigned reasoning history is kept for compatibility endpoints.
pub fn responses_to_claude_with_compat(ctx: &RequestCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    claude_responses::convert(ctx.model, body, ctx.stream, true, ctx.tool_search)
}

use bytes::Bytes;
use cpa_common::json as gj;
use cpa_core::format::Format;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// Request-scoped inputs available to request transforms.
pub struct RequestCtx<'a> {
    pub model: &'a str,
    pub stream: bool,
    /// `upstream.claude.tool-search` as pinned for this request (Go
    /// `WithClientToolSearch`): Codex client tool_search on the Responses -> Claude pair.
    pub tool_search: bool,
}

/// Request-scoped inputs available to response transforms.
pub struct ResponseCtx<'a> {
    pub model: &'a str,
    pub original_request: &'a [u8],
    pub translated_request: &'a [u8],
    /// The request's pinned `upstream.claude.tool-search` ([`RequestCtx::tool_search`]).
    pub tool_search: bool,
}

pub trait StreamTranslator: Send {
    /// One complete upstream event in, zero or more complete client events out.
    fn event(&mut self, event: &[u8]) -> Result<Vec<Bytes>, Error>;
    /// Upstream ended cleanly; emit any closing events.
    fn finish(&mut self) -> Result<Vec<Bytes>, Error>;
    /// Before a terminal error is written to a Responses client: the client-side frame
    /// still being joined, if it is complete enough to send (Go's responsesSSEFramer.Flush).
    /// Other clients have nothing pending.
    fn flush_frames(&mut self) -> Vec<Bytes> {
        vec![]
    }
    /// Go's apply_patch tool-input contract (`ToolInputError`): the upstream sent an
    /// invalid or conflicting `apply_patch` call. Check after every `event`: write that
    /// event's frames (they end in `response.failed`), then end the stream with HTTP 502
    /// and [`APPLY_PATCH_UPSTREAM_ERROR`], as helps.StopApplyPatchStream does.
    fn tool_input_failed(&self) -> bool {
        false
    }
    /// Go's `FinalizeToolInput` (helps.EndApplyPatchStream): call when the upstream
    /// transport ends, before any synthetic terminator (`[DONE]`) or `finish`. A stream that
    /// declared `apply_patch` and ended without its terminator yields `response.failed`;
    /// then check [`Self::tool_input_failed`].
    fn finalize_tool_input(&mut self) -> Vec<Bytes> {
        vec![]
    }
    /// Go's `CanFinalizeResponseStream` (0ea4e1dc): the OpenAI Chat Completions to
    /// Responses stream saw a finish reason that left every item done, so a clean EOF
    /// without `[DONE]` may translate a synthetic `data: [DONE]` instead of failing.
    fn can_finalize_response_stream(&self) -> bool {
        false
    }
}

/// helps.ApplyPatchUpstreamErrorMessage: the 502 message executors return when a
/// translator rejects upstream apply_patch input (a `non_stream` error carries it too).
pub const APPLY_PATCH_UPSTREAM_ERROR: &str = apply_patch::UPSTREAM_ERROR_MESSAGE;

pub type RequestFn = fn(ctx: &RequestCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error>;
pub type NonStreamFn = fn(ctx: &ResponseCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error>;
pub type StreamFn = fn(ctx: &ResponseCtx<'_>) -> Box<dyn StreamTranslator>;
pub type CountTokensFn = fn(ctx: &ResponseCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error>;
/// Go's `TokenCount(ctx, count)`: an input-token count in the client's response shape.
pub type TokenCountFn = fn(count: i64) -> Vec<u8>;
#[doc(hidden)]
pub type GoStreamFn = fn(ctx: &ResponseCtx<'_>) -> Box<dyn stream::GoStream>;

pub struct Pair {
    pub request: RequestFn,
    pub non_stream: NonStreamFn,
    pub stream: StreamFn,
    pub count_tokens: Option<CountTokensFn>,
}

/// One Go registration: the public transforms plus what [`token_count`] and
/// [`go_stream`] expose.
pub(crate) struct Registered {
    pub pair: Pair,
    pub token_count: Option<TokenCountFn>,
    pub go_stream: GoStreamFn,
}

fn registered(client: Format, upstream: Format) -> Option<&'static Registered> {
    // ponytail: static match over the built-in pairs.
    match (client, upstream) {
        (Format::OpenAI, Format::OpenAI) => Some(&openai::PAIR),
        (Format::OpenAI, Format::Claude) => Some(&claude_chat_request::PAIR),
        (Format::OpenAIResponse, Format::Codex) => Some(&codex_responses::PAIR),
        (Format::OpenAIResponse, Format::Claude) => Some(&claude_responses_response::PAIR),
        (Format::Claude, Format::OpenAI) => Some(&openai_claude::PAIR),
        (Format::OpenAI, Format::Codex) => Some(&codex_chat_request::PAIR),
        (Format::Claude, Format::Codex) => Some(&codex_claude::PAIR),
        (Format::OpenAIResponse, Format::OpenAI) => Some(&openai_responses_response::PAIR),
        _ => None,
    }
}

/// Transforms for a client format talking to an upstream format, or `None` when the
/// pair is not registered.
pub fn pair(client: Format, upstream: Format) -> Option<&'static Pair> {
    registered(client, upstream).map(|r| &r.pair)
}

/// Go's TokenCount for the pair: renders an upstream input-token count in the client's
/// shape. `None` when Go registers none (the upstream body is then returned as is).
pub fn token_count(client: Format, upstream: Format) -> Option<TokenCountFn> {
    registered(client, upstream).and_then(|r| r.token_count)
}

/// A pair's `stream` driven the way a Go executor with [`stream::StreamOptions`] drives
/// it; `None` when Go registers no pair.
pub fn stream_with(
    client: Format,
    upstream: Format,
    ctx: &ResponseCtx<'_>,
    options: stream::StreamOptions,
) -> Option<Box<dyn StreamTranslator>> {
    registered(client, upstream).map(|r| stream::framed_with(client, upstream, (r.go_stream)(ctx), options))
}

/// The Go-shaped line translator behind a pair's `stream`, for golden tests.
#[doc(hidden)]
pub fn go_stream(client: Format, upstream: Format) -> Option<GoStreamFn> {
    registered(client, upstream).map(|r| r.go_stream)
}

/// sdk/translator TranslateRequest. A registered pair runs between summary extraction
/// (from the client body) and summary application (to the upstream body). Without a
/// pair, a differing top-level `model` is rewritten. Go's plugin request normalizers
/// and translators are removed with native plugins.
pub fn translate_request(
    client: Format,
    upstream: Format,
    ctx: &RequestCtx<'_>,
    body: &[u8],
) -> Result<Vec<u8>, Error> {
    match pair(client, upstream) {
        Some(pair) => translate_registered(client, upstream, ctx, body, |body| (pair.request)(ctx, body)),
        None => Ok(translate_fallback(ctx, body)),
    }
}

/// The Codex translated route's request conversion (`docs/codex/CONTRACT.md`
/// "Translated route"): the built-in converter of `client` into Codex in its strict
/// mode. The thinking summary pipeline runs, as it converts the client's own
/// reasoning-summary choice. `None` for a source the route does not convert.
pub fn codex_request(client: Format, ctx: &RequestCtx<'_>, body: &[u8]) -> Option<Result<Vec<u8>, Error>> {
    let convert: RequestFn = match client {
        Format::Claude => codex_claude::codex_strict,
        Format::OpenAI => codex_chat_request::STRICT.pair.request,
        _ => return None,
    };
    let upstream = Format::Codex;
    let summary = thinking::extract_translated_summary(body, client.as_str(), upstream.as_str());
    let converted = convert(ctx, body).and_then(|mut out| {
        codex_strict::controls(client, body, &mut out)?;
        Ok(out)
    });
    Some(converted.map(|out| thinking::apply_summary_for_model(out, upstream.as_str(), ctx.model, summary)))
}

/// The Codex translated route's response translators for a `client` answered from Codex:
/// the pair whose request conversion [`codex_request`] uses. For a Chat Completions client
/// they read nested custom tools as that conversion does; the registered pair, which the
/// xAI and Meta executors share, keeps its reading.
pub fn codex_response(client: Format) -> Option<&'static Pair> {
    match client {
        Format::OpenAI => Some(&codex_chat_request::STRICT.pair),
        _ => pair(client, Format::Codex),
    }
}

/// The registered branch of Go `TranslateRequestEnvelope`.
fn translate_registered(
    client: Format,
    upstream: Format,
    ctx: &RequestCtx<'_>,
    body: &[u8],
    native: impl FnOnce(&[u8]) -> Result<Vec<u8>, Error>,
) -> Result<Vec<u8>, Error> {
    let summary = thinking::extract_translated_summary(body, client.as_str(), upstream.as_str());
    let out = native(body)?;
    Ok(thinking::apply_summary_for_model(
        out,
        upstream.as_str(),
        ctx.model,
        summary,
    ))
}

/// The unregistered branch of Go `TranslateRequestEnvelope`.
fn translate_fallback(ctx: &RequestCtx<'_>, body: &[u8]) -> Vec<u8> {
    let mut out = body.to_vec();
    if !ctx.model.is_empty() && *gj::get(body, "model").bytes() != *ctx.model.as_bytes() {
        gj::set_str(&mut out, "model", ctx.model);
    }
    out
}

/// The pair's Go-shaped stream translator, for executors that drive Go lines
/// themselves. `None` when no pair is registered (Go's `TranslateStream` then passes
/// each chunk through).
pub fn named_go_stream(client: &str, upstream: &str, ctx: &ResponseCtx<'_>) -> Option<Box<dyn stream::GoStream>> {
    Format::parse(client)
        .zip(Format::parse(upstream))
        .and_then(|(c, u)| registered(c, u))
        .map(|r| (r.go_stream)(ctx))
}

/// sdk/translator TranslateTokenCount: the pair's token-count shape, or the upstream body
/// unchanged when the pair registers none.
pub fn translate_token_count(client: Format, upstream: Format, count: i64, body: &[u8]) -> Vec<u8> {
    match token_count(client, upstream) {
        Some(render) => render(count),
        None => body.to_vec(),
    }
}

#[cfg(test)]
mod go_helper_tests;
