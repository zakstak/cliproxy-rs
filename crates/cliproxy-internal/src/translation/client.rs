//! Applies client-specific request rewrites, then converts the request format.

use crate::config::Config;
use crate::format::Format;
use crate::protocol::codex_client::{self as cc, Settings};
use crate::text::GoStr;
use crate::translation::RequestCtx;
use http::HeaderMap;

/// The client side of one translation: its headers, the config the executor reads, the
/// executor's identity and whether the selected API-key model is `is-compat`.
pub(crate) struct Client<'a> {
    pub headers: &'a HeaderMap,
    pub settings: Settings,
    /// Go's `targetExecutor`: Codex executors keep integer tool types; "" for executors
    /// Go calls without one.
    pub target_executor: &'a str,
    pub is_compat: bool,
}

impl<'a> Client<'a> {
    /// The executor's own config, as Go's executors pass `e.cfg`.
    pub fn new(headers: &'a HeaderMap, cfg: &Config, target_executor: &'a str, is_compat: bool) -> Self {
        Self {
            headers,
            settings: Settings::from_config(cfg),
            target_executor,
            is_compat,
        }
    }
}

/// `isCodexTargetExecutor`.
fn codex_target(executor: &str) -> bool {
    matches!(
        executor.trim().go_lower().as_str(),
        "codex" | "codex-websockets" | "codex_websockets"
    )
}

/// The `...WithCompat` translator Go picks for an is-compat model, if the pair has one.
fn compat_translator(from: Format, to: Format) -> Option<crate::translation::RequestFn> {
    Some(match (from, to) {
        (Format::Claude, Format::Codex) => crate::translation::claude_to_codex_with_compat,
        (Format::Claude, Format::OpenAI) => crate::translation::claude_to_openai_with_compat,
        (Format::OpenAI, Format::Claude) => crate::translation::openai_to_claude_with_compat,
        (Format::OpenAIResponse, Format::Claude) => crate::translation::responses_to_claude_with_compat,
        _ => return None,
    })
}

/// `TranslateRequestWithAPIKeyModelCompatibilityForExecutor` (and, without compat,
/// `TranslateRequestWithCodexMultiAgentV2ForExecutor`):
/// 1. Codex clients' integer tool types are normalized for non-Codex executors.
/// 2. OpenAI Responses requests get the orphan-delegation rewrite and, for non-Codex
///    targets, the multi-agent v2 input rewrite (portable messages; compat also strips
///    `author`, `recipient` and passthrough metadata).
/// 3. An is-compat model uses its pair's `...WithCompat` translator between summary
///    extraction and application; anything else goes through
///    `crate::translation::translate_request`.
pub(crate) fn translate_request(
    from: Format,
    to: Format,
    ctx: &RequestCtx<'_>,
    body: &[u8],
    client: &Client<'_>,
) -> Result<Vec<u8>, crate::translation::Error> {
    let mut payload = if codex_target(client.target_executor) {
        body.to_vec()
    } else {
        crate::protocol::payload::normalize_codex_tool_integer_types(body, client.headers)
    };
    if from == Format::OpenAIResponse {
        payload = cc::rewrite_orphan_delegation_input(client.headers, &payload, client.settings.orphan_delegation);
        if !matches!(to, Format::Codex | Format::OpenAIResponse) {
            payload = cc::rewrite_multi_agent_v2_input(client.headers, &payload, &client.settings, client.is_compat);
        }
    }
    let compat = client.is_compat.then(|| compat_translator(from, to)).flatten();
    let Some(translate) = compat else {
        return crate::translation::translate_request(from, to, ctx, &payload);
    };
    use crate::protocol::thinking::{apply_summary_config_for_model, extract_translated_summary_config};
    let summary = extract_translated_summary_config(&payload, from.as_str(), to.as_str());
    let translated = translate(ctx, &payload)?;
    Ok(apply_summary_config_for_model(
        &translated,
        to.as_str(),
        ctx.model,
        summary,
    ))
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
