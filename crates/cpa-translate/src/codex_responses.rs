//! OpenAI Responses <-> Codex (internal/translator/codex/openai/responses).
//!
//! Requests keep the client's bytes except for Codex's required fields and removals.
//! Responses pass through, adding the requested model to `response.created` and
//! `response.in_progress` when upstream omits it. An executor may own an apply_patch
//! [`Bridge`] for the request ([`stream_with_bridge`], [`non_stream_with_bridge`]); native
//! Codex never installs one.

use crate::apply_patch_responses::Bridge;
use crate::{Error, Registered, RequestCtx, ResponseCtx, StreamTranslator, common, stream};
use cpa_common::json::{self as gj, Kind};
use cpa_core::gostr::GoStr;

pub static PAIR: Registered = registered!(
    OpenAIResponse -> Codex,
    request: request,
    non_stream: non_stream,
    go_stream: go_stream,
    token_count: None,
);

fn request(ctx: &RequestCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    let _ = ctx;
    Ok(convert(body, ResponsesClient::Translated))
}

/// The client that built a Responses request. It decides whether the conversion forces
/// `parallel_tool_calls` and rebuilds `include`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponsesClient {
    /// Any Responses client other than official Codex. Go's rules apply:
    /// `parallel_tool_calls` becomes `true` and `include` is rebuilt.
    Translated,
    /// An official Codex client. Codex sets `parallel_tool_calls` and `include` for the
    /// OpenAI upstream it expects, so `parallel_tool_calls` keeps the client's value or
    /// stays absent. `output_replay` also keeps `output.encrypted_content` in `include`.
    /// Go v8.0.35 applies the `Translated` rules to every client
    /// (docs/DIFFERENCES-FROM-GO.md).
    NativeCodex { output_replay: bool },
}

/// ConvertOpenAIResponsesRequestToCodex, with the changes for a native Codex client
/// ([`ResponsesClient`]).
pub(crate) fn convert(body: &[u8], client: ResponsesClient) -> Vec<u8> {
    let mut raw = body.to_vec();
    let input = gj::get(&raw, "input");
    if input.kind == Kind::String {
        let mut wrapped = br#"[{"type":"message","role":"user","content":[{"type":"input_text","text":""}]}]"#.to_vec();
        gj::set_str(&mut wrapped, "0.content.0.text", input.bytes());
        gj::set_raw(&mut raw, "input", wrapped);
    }
    set_required_bool(&mut raw, "stream", true);
    set_required_bool(&mut raw, "store", false);
    let output_replay = match client {
        ResponsesClient::Translated => {
            set_required_bool(&mut raw, "parallel_tool_calls", true);
            false
        }
        ResponsesClient::NativeCodex { output_replay } => output_replay,
    };
    set_required_include(&mut raw, output_replay);
    delete_fields(
        &mut raw,
        &["max_output_tokens", "max_completion_tokens", "temperature", "top_p"],
    );
    let tier = gj::get(&raw, "service_tier");
    if tier.exists() {
        if tier.kind == Kind::String {
            let value = tier.bytes().into_owned();
            match tier.str().trim().go_lower().as_str() {
                "priority" | "fast" => {
                    if value != b"priority" {
                        gj::set_str(&mut raw, "service_tier", "priority");
                    }
                }
                "ultrafast" => {
                    if value != b"ultrafast" {
                        gj::set_str(&mut raw, "service_tier", "ultrafast");
                    }
                }
                _ => delete_fields(&mut raw, &["service_tier"]),
            }
        } else {
            delete_fields(&mut raw, &["service_tier"]);
        }
    }
    // ponytail: native Codex loses its `prompt_cache_options` TTL here too. OpenAI's API
    // documents the field, but no evidence shows that the ChatGPT Codex backend accepts
    // it, and Go's SIWC rules reject it. Keep it per route once backend support is shown.
    delete_fields(
        &mut raw,
        &["truncation", "prompt_cache_options", "prompt_cache_retention"],
    );
    raw = strip_cache_breakpoints(raw);
    if gj::get(&raw, "context_management").exists() {
        gj::delete(&mut raw, "context_management");
    }
    delete_fields(&mut raw, &["user"]);
    raw = system_role_to_developer(raw);
    raw = normalize_builtin_tools(raw);
    normalize_empty_function_call_arguments(raw)
}

fn set_required_bool(raw: &mut Vec<u8>, path: &str, value: bool) {
    let current = gj::get(raw, path).kind;
    if (value && current == Kind::True) || (!value && current == Kind::False) {
        return;
    }
    gj::set_bool(raw, path, value);
}

/// Go `setCodexRequiredInclude`: `reasoning.encrypted_content`, plus
/// `web_search_call.action.sources` when the client asked for it. With `output_replay`,
/// a requested `output.encrypted_content` follows them. An `include` that already has
/// exactly those values in that order keeps its bytes. Every other value is dropped.
fn set_required_include(raw: &mut Vec<u8>, output_replay: bool) {
    const REASONING: &str = "reasoning.encrypted_content";
    const SOURCES: &str = "web_search_call.action.sources";
    const OUTPUT: &str = "output.encrypted_content";
    let current = gj::get(raw, "include");
    let values = if current.is_array() { current.array() } else { vec![] };
    let requested = |want: &str| {
        values
            .iter()
            .any(|v| v.kind == Kind::String && &*v.bytes() == want.as_bytes())
    };
    let mut wanted = vec![REASONING];
    if requested(SOURCES) {
        wanted.push(SOURCES);
    }
    if output_replay && requested(OUTPUT) {
        wanted.push(OUTPUT);
    }
    if current.is_array()
        && values.len() == wanted.len()
        && values
            .iter()
            .zip(&wanted)
            .all(|(v, want)| v.kind == Kind::String && &*v.bytes() == want.as_bytes())
    {
        return;
    }
    let encoded = format!(
        "[{}]",
        wanted.iter().map(|v| format!("\"{v}\"")).collect::<Vec<_>>().join(",")
    );
    gj::set_raw(raw, "include", encoded);
}

fn delete_fields(raw: &mut Vec<u8>, paths: &[&str]) {
    for path in paths {
        if gj::get(raw, path).exists() {
            gj::delete(raw, path);
        }
    }
}

/// Rebuilds `input` with `edit` applied to each item; unchanged when nothing changed.
fn rebuild_input(raw: Vec<u8>, mut edit: impl FnMut(&gj::Res<'_>, &mut Vec<u8>) -> bool) -> Vec<u8> {
    let input = gj::get(&raw, "input");
    if !input.is_array() {
        return raw;
    }
    let items = input.array();
    if items.is_empty() {
        return raw;
    }
    let mut changed = false;
    let mut rebuilt = Vec::with_capacity(items.len());
    for item in &items {
        let mut item_raw = item.raw.to_vec();
        changed |= edit(item, &mut item_raw);
        rebuilt.push(item_raw);
    }
    if !changed {
        return raw;
    }
    let mut out = raw.clone();
    gj::set_raw(&mut out, "input", gj::join(&rebuilt));
    out
}

fn normalize_empty_function_call_arguments(raw: Vec<u8>) -> Vec<u8> {
    rebuild_input(raw, |item, item_raw| {
        let args = item.get("arguments");
        if item.is_object()
            && item.get("type").str() == "function_call"
            && args.kind == Kind::String
            && common::trim_space(&args.s).is_empty()
        {
            return gj::set_str(item_raw, "arguments", "{}");
        }
        false
    })
}

fn strip_cache_breakpoints(raw: Vec<u8>) -> Vec<u8> {
    if !raw.windows(25).any(|w| w == b"\"prompt_cache_breakpoint\"") {
        return raw;
    }
    rebuild_input(raw, |item, item_raw| {
        let mut changed = false;
        for path in ["content", "output"] {
            let array = item.get(path);
            if !array.is_array() {
                continue;
            }
            let parts = array.array();
            if !parts.iter().any(|p| p.get("prompt_cache_breakpoint").exists()) {
                continue;
            }
            let mut part_changed = false;
            let rebuilt: Vec<Vec<u8>> = parts
                .iter()
                .map(|p| {
                    let mut part = p.raw.to_vec();
                    if p.get("prompt_cache_breakpoint").exists() {
                        part_changed |= gj::delete(&mut part, "prompt_cache_breakpoint");
                    }
                    part
                })
                .collect();
            if part_changed && gj::set_raw(item_raw, path, gj::join(&rebuilt)) {
                changed = true;
            }
        }
        if item.get("prompt_cache_breakpoint").exists() && gj::delete(item_raw, "prompt_cache_breakpoint") {
            changed = true;
        }
        changed
    })
}

/// convertSystemRoleToDeveloper. Go re-marshals the input through `[]json.RawMessage`,
/// which compacts every item and HTML-escapes it, and gives up if any item is invalid.
fn system_role_to_developer(raw: Vec<u8>) -> Vec<u8> {
    let input = gj::get(&raw, "input");
    if !input.is_array() {
        return raw;
    }
    let items = input.array();
    let system = |item: &gj::Res<'_>| item.is_object() && &*item.get("role").bytes() == b"system";
    if !items.iter().any(system) {
        return raw;
    }
    let mut rebuilt = Vec::with_capacity(items.len());
    for item in &items {
        let mut item_raw = item.raw.to_vec();
        if system(item) && !gj::set_raw(&mut item_raw, "role", r#""developer""#) {
            return raw;
        }
        if !gj::valid(&item_raw) {
            return raw;
        }
        rebuilt.push(item_raw);
    }
    let mut out = raw.clone();
    gj::set_raw(&mut out, "input", gj::compact(&gj::join(&rebuilt), true));
    out
}

fn builtin_tool_type(kind: &[u8]) -> Option<&'static str> {
    matches!(kind, b"web_search_preview" | b"web_search_preview_2025_03_11").then_some("web_search")
}

fn normalize_builtin_tools(raw: Vec<u8>) -> Vec<u8> {
    let mut raw = normalize_tool_array(raw, "tools");
    if let Some(kind) = builtin_tool_type(&gj::get(&raw, "tool_choice.type").bytes()) {
        gj::set_str(&mut raw, "tool_choice.type", kind);
    }
    normalize_tool_array(raw, "tool_choice.tools")
}

fn normalize_tool_array(raw: Vec<u8>, path: &str) -> Vec<u8> {
    let tools = gj::get(&raw, path);
    if !tools.is_array() {
        return raw;
    }
    let mut changed = false;
    let mut items = vec![];
    tools.each(|_, tool| {
        let mut item = tool.raw.to_vec();
        if let Some(kind) = builtin_tool_type(&tool.get("type").bytes())
            && gj::set_str(&mut item, "type", kind)
        {
            changed = true;
        }
        items.push(item);
        true
    });
    if !changed {
        return raw;
    }
    let mut out = raw.clone();
    gj::set_raw(&mut out, path, gj::join(&items));
    out
}

fn go_stream(ctx: &ResponseCtx<'_>) -> Box<dyn stream::GoStream> {
    Box::new(events(ctx, None))
}

/// The Go-shaped line translator behind [`stream_with_bridge`], for golden tests.
#[doc(hidden)]
pub fn go_stream_with_bridge(ctx: &ResponseCtx<'_>, bridge: Option<Bridge>) -> Box<dyn stream::GoStream> {
    Box::new(events(ctx, bridge))
}

fn events(ctx: &ResponseCtx<'_>, bridge: Option<Bridge>) -> Events {
    Events {
        model: ctx.model.as_bytes().to_vec(),
        original: ctx.original_request.to_vec(),
        translated: ctx.translated_request.to_vec(),
        bridge,
    }
}

struct Events {
    model: Vec<u8>,
    original: Vec<u8>,
    translated: Vec<u8>,
    /// Go's executor-owned `param` bridge (responsesBridge).
    bridge: Option<Bridge>,
}

impl stream::GoStream for Events {
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        let (sse, payload) = match line.strip_prefix(b"data:") {
            Some(rest) => (true, common::trim_space(rest)),
            None => (false, line),
        };
        let updated = with_model(payload, &self.model, &self.original, &self.translated);
        let Some(bridge) = &mut self.bridge else {
            if updated == payload {
                return Ok(vec![line.to_vec()]);
            }
            let mut out = vec![];
            if sse {
                out.extend_from_slice(b"data: ");
            }
            out.extend_from_slice(&updated);
            return Ok(vec![out]);
        };
        // The failure stays on the bridge (tool_input_failed), as Go's param does.
        let (outputs, _) = bridge.transform(&updated);
        Ok(outputs
            .into_iter()
            .map(|o| if sse { [&b"data: "[..], &o].concat() } else { o })
            .collect())
    }

    fn tool_input_failed(&self) -> bool {
        self.bridge.as_ref().is_some_and(|b| b.tool_input_error().is_some())
    }
}

/// setResponsesModel: creation events without `response.model` get the request's model.
fn with_model(raw: &[u8], model: &[u8], original: &[u8], translated: &[u8]) -> Vec<u8> {
    let kind = gj::get(raw, "type").bytes();
    if (&*kind != b"response.created" && &*kind != b"response.in_progress") || gj::get(raw, "response.model").exists() {
        return raw.to_vec();
    }
    let mut name = common::request_model_name(original, translated);
    if name.is_empty() {
        name = model.to_vec();
    }
    let mut out = raw.to_vec();
    if !name.is_empty() {
        gj::set_str(&mut out, "response.model", name);
    }
    out
}

/// The openai-response:codex stream with an executor-owned apply_patch bridge for this
/// request (Go passes the bridge as the translator `param`). After each event, check
/// [`StreamTranslator::tool_input_failed`]: the event's frames end in `response.failed`
/// and the executor stops with HTTP 502 and [`crate::APPLY_PATCH_UPSTREAM_ERROR`].
pub fn stream_with_bridge(ctx: &ResponseCtx<'_>, bridge: Bridge) -> Box<dyn StreamTranslator> {
    let levels = crate::levels(&[ctx.original_request, ctx.translated_request]);
    stream::framed(
        cpa_core::format::Format::OpenAIResponse,
        cpa_core::format::Format::Codex,
        Box::new(events(ctx, Some(bridge))),
        levels,
    )
}

/// The openai-response:codex non-stream with an executor-owned apply_patch bridge. A
/// rejected body is `Err(APPLY_PATCH_UPSTREAM_ERROR)` (Go returns nil and the executor
/// answers 502); the bridge keeps the underlying error.
pub fn non_stream_with_bridge(ctx: &ResponseCtx<'_>, body: &[u8], bridge: &mut Bridge) -> Result<Vec<u8>, Error> {
    let levels = crate::levels(&[body, ctx.original_request, ctx.translated_request]);
    crate::deep_stack(levels, || match bridge.transform_non_stream(body) {
        Ok(converted) => non_stream(ctx, &converted),
        Err(_) => Err(Error(crate::APPLY_PATCH_UPSTREAM_ERROR.into())),
    })
}

fn non_stream(_: &ResponseCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    let root = gj::parse(body);
    let kind = root.get("type").bytes();
    if kind.is_empty() && root.get("output").is_array() {
        return Ok(body.to_vec());
    }
    if &*kind != b"response.completed" && &*kind != b"response.incomplete" {
        return Ok(vec![]);
    }
    Ok(root.get("response").raw.to_vec())
}

#[cfg(test)]
mod tests {
    /// Go `TestConvertOpenAIResponsesRequestToCodex_PreservesWebSearchSourcesInclude`.
    #[test]
    fn web_search_sources_stay_in_include() {
        let reasoning = r#"["reasoning.encrypted_content"]"#;
        let both = r#"["reasoning.encrypted_content","web_search_call.action.sources"]"#;
        for (include, want) in [
            ("", reasoning),
            ("null", reasoning),
            (r#""web_search_call.action.sources""#, reasoning),
            (r#"{"web_search_call.action.sources":true}"#, reasoning),
            ("[42,true]", reasoning),
            ("[]", reasoning),
            (r#"["web_search_call.action.sources"]"#, both),
            (both, both),
            (
                r#"["web_search_call.action.sources","reasoning.encrypted_content"]"#,
                both,
            ),
            (
                r#"["web_search_call.action.sources","web_search_call.action.sources"]"#,
                both,
            ),
            (
                r#"["file_search_call.results","web_search_call.action.sources","code_interpreter_call.outputs"]"#,
                both,
            ),
            (r#"[42,"web_search_call.action.sources",null]"#, both),
        ] {
            let body = if include.is_empty() {
                r#"{"model":"gpt-5.6","input":[{"type":"message","role":"user","content":"hi"}]}"#.to_owned()
            } else {
                format!(
                    r#"{{"model":"gpt-5.6","input":[{{"type":"message","role":"user","content":"hi"}}],"include":{include}}}"#
                )
            };
            let out = super::convert(body.as_bytes(), super::ResponsesClient::Translated);
            assert_eq!(
                crate::gj::get(&out, "include").raw.as_ref(),
                want.as_bytes(),
                "{include}"
            );
        }
    }

    const NATIVE: super::ResponsesClient = super::ResponsesClient::NativeCodex { output_replay: true };

    /// The shape of a request from current Codex (core/src/client.rs `build_request`):
    /// serde field order, `include` with output replay, and an explicit
    /// `parallel_tool_calls`. Spacing marks bytes that a re-serialization would change.
    fn codex_body(parallel: &str, include: &str) -> String {
        format!(
            r#"{{"model":"gpt-6.1-sol","instructions":"x","input":[{{"type":"message","role":"user","content":[{{"type":"input_text","text":"hi"}}]}}],"tools":[{{"type":"function","name":"f","parameters":{{"type":"object"}}}}],"tool_choice":"auto"{parallel},"reasoning":{{"effort":"high"}},"store":false,"stream":true{include},"prompt_cache_key":"k",  "text" : {{"verbosity":"low"}}}}"#
        )
    }

    #[test]
    fn native_codex_keeps_parallel_tool_calls_and_output_replay_bytes() {
        let include = r#","include":["reasoning.encrypted_content","output.encrypted_content"]"#;
        for parallel in [r#","parallel_tool_calls":false"#, r#","parallel_tool_calls":true"#, ""] {
            let body = codex_body(parallel, include);
            assert_eq!(
                String::from_utf8(super::convert(body.as_bytes(), NATIVE)).unwrap(),
                body,
                "{parallel:?}"
            );
        }
    }

    #[test]
    fn translated_clients_keep_gos_forced_fields() {
        let include = r#","include":["reasoning.encrypted_content","output.encrypted_content"]"#;
        for parallel in [r#","parallel_tool_calls":false"#, ""] {
            let out = super::convert(
                codex_body(parallel, include).as_bytes(),
                super::ResponsesClient::Translated,
            );
            assert_eq!(crate::gj::get(&out, "parallel_tool_calls").raw.as_ref(), b"true");
            assert_eq!(
                crate::gj::get(&out, "include").raw.as_ref(),
                br#"["reasoning.encrypted_content"]"#
            );
        }
    }

    #[test]
    fn native_include_keeps_only_supported_values() {
        let reasoning = r#"["reasoning.encrypted_content"]"#;
        let replay = r#"["reasoning.encrypted_content","output.encrypted_content"]"#;
        for (client, include, want) in [
            // Without output replay (SIWC and compat upstreams), Go's include.
            (
                super::ResponsesClient::NativeCodex { output_replay: false },
                replay,
                reasoning,
            ),
            (NATIVE, "", reasoning),
            (NATIVE, r#"["output.encrypted_content"]"#, replay),
            (
                NATIVE,
                r#"["output.encrypted_content","reasoning.encrypted_content"]"#,
                replay,
            ),
            (
                NATIVE,
                r#"["reasoning.encrypted_content","file_search_call.results","output.encrypted_content"]"#,
                replay,
            ),
            (
                NATIVE,
                r#"["output.encrypted_content","web_search_call.action.sources"]"#,
                r#"["reasoning.encrypted_content","web_search_call.action.sources","output.encrypted_content"]"#,
            ),
        ] {
            let body = if include.is_empty() {
                r#"{"model":"m","input":[]}"#.to_owned()
            } else {
                format!(r#"{{"model":"m","input":[],"include":{include}}}"#)
            };
            let out = super::convert(body.as_bytes(), client);
            assert_eq!(
                crate::gj::get(&out, "include").raw.as_ref(),
                want.as_bytes(),
                "{client:?} {include}"
            );
        }
    }

    /// The native path keeps every other Go rule, including the cache-option removal:
    /// nothing shows that the ChatGPT Codex backend accepts `prompt_cache_options`.
    #[test]
    fn native_codex_still_drops_unsupported_fields() {
        let body = br#"{"model":"m","input":"hi","parallel_tool_calls":false,"prompt_cache_options":{"ttl":"30m"},"prompt_cache_retention":"24h","max_output_tokens":5,"user":"u"}"#;
        let out = super::convert(body, NATIVE);
        for gone in [
            "prompt_cache_options",
            "prompt_cache_retention",
            "max_output_tokens",
            "user",
        ] {
            assert!(!crate::gj::get(&out, gone).exists(), "{gone}");
        }
        assert_eq!(crate::gj::get(&out, "parallel_tool_calls").raw.as_ref(), b"false");
        assert_eq!(crate::gj::get(&out, "input.0.content.0.text").str(), "hi");
    }
}
