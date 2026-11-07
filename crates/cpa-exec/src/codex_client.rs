//! Request translation for Codex clients (executor/helps/codex_multi_agent_v2.go): the
//! `cpa_common::codex_client` rewrites around `cpa_translate`, for every executor that
//! translates a request Codex clients may send. The pure rewrites live in cpa-common;
//! this module also needs the translators, which cpa-common cannot depend on.

use cpa_common::codex_client::{self as cc, Settings};
use cpa_core::config::Config;
use cpa_core::format::Format;
use cpa_core::gostr::GoStr;
use cpa_translate::RequestCtx;
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
fn compat_translator(from: Format, to: Format) -> Option<cpa_translate::RequestFn> {
    Some(match (from, to) {
        (Format::Claude, Format::Codex) => cpa_translate::claude_to_codex_with_compat,
        (Format::Claude, Format::Gemini) => cpa_translate::claude_to_gemini_with_compat,
        (Format::Claude, Format::Interactions) => cpa_translate::claude_to_interactions_with_compat,
        (Format::Claude, Format::OpenAI) => cpa_translate::claude_to_openai_with_compat,
        (Format::OpenAI, Format::Claude) => cpa_translate::openai_to_claude_with_compat,
        (Format::OpenAIResponse, Format::Claude) => cpa_translate::responses_to_claude_with_compat,
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
///    extraction and application, then the plugin request normalizers; anything else
///    goes through `translate_request_tracked`.
///
/// Whether a plugin normalizer changed the `configuration_update` items is reported
/// only off the compat path (`...AndUpdateIntentForExecutor`).
pub(crate) fn translate_request(
    from: Format,
    to: Format,
    ctx: &RequestCtx<'_>,
    body: &[u8],
    client: &Client<'_>,
) -> Result<cpa_translate::Translated, cpa_translate::Error> {
    let mut payload = if codex_target(client.target_executor) {
        body.to_vec()
    } else {
        cpa_common::payload::normalize_codex_tool_integer_types(body, client.headers)
    };
    if from == Format::OpenAIResponse {
        payload = cc::rewrite_orphan_delegation_input(client.headers, &payload, client.settings.orphan_delegation);
        if !matches!(to, Format::Codex | Format::OpenAIResponse) {
            payload = cc::rewrite_multi_agent_v2_input(client.headers, &payload, &client.settings, client.is_compat);
        }
    }
    let compat = client.is_compat.then(|| compat_translator(from, to)).flatten();
    let Some(translate) = compat else {
        let mut out = cpa_translate::translate_request_tracked(from, to, ctx, &payload)?;
        out.configuration_updates_changed &= !client.is_compat || (to == Format::Codex && from != Format::Claude);
        return Ok(out);
    };
    use cpa_common::thinking::{apply_summary_config_for_model, extract_translated_summary_config};
    let summary = extract_translated_summary_config(&payload, from.as_str(), to.as_str());
    let translated = translate(ctx, &payload)?;
    let body = apply_summary_config_for_model(&translated, to.as_str(), ctx.model, summary);
    Ok(cpa_translate::Translated {
        body: cpa_translate::hooks::normalize_request(from.as_str(), to.as_str(), ctx.model, body, ctx.stream),
        configuration_updates_changed: false,
    })
}

/// `TranslateRequestEnvelopeWithCodexMultiAgentV2`: the Responses orphan-delegation and
/// multi-agent v2 rewrites (no integer tool-type normalization), then
/// `cpa_translate::translate_request_envelope` with the attempt's model info.
pub(crate) fn translate_request_envelope(
    from: Format,
    to: Format,
    ctx: &RequestCtx<'_>,
    body: &[u8],
    client: &Client<'_>,
    model_info: Option<&cpa_core::registry::ModelInfo>,
) -> Result<Vec<u8>, cpa_translate::Error> {
    let mut payload = body.to_vec();
    if from == Format::OpenAIResponse {
        payload = cc::rewrite_orphan_delegation_input(client.headers, &payload, client.settings.orphan_delegation);
        if !matches!(to, Format::Codex | Format::OpenAIResponse) {
            payload = cc::rewrite_multi_agent_v2_input(client.headers, &payload, &client.settings, client.is_compat);
        }
    }
    cpa_translate::translate_request_envelope(from, to, ctx, &payload, model_info)
}

#[cfg(test)]
#[path = "codex_client_tests.rs"]
mod tests;
