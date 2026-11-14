//! Anthropic-Beta assembly for the Claude Code profile (claude_executor_request.go).
//! The list is per request: model capability, body features and caller requests
//! decide which managed betas appear, in Claude Code 2.1.325's wire order
//! (`testdata/claude_code_2_1_325.json`, a loopback capture of that release).

use cpa_core::gostr::GoStr;
use std::collections::HashSet;

use super::cloak::{canonical_model, is_fable51, is_opus55, legacy_system_reminder};
use super::signals;
use crate::rawjson;

pub(crate) const TOKEN_COUNTING: &str = "token-counting-2024-11-01";
pub(crate) const FAST_MODE: &str = "fast-mode-2026-02-01";
pub(crate) const OAUTH: &str = "oauth-2025-04-20";
pub(crate) const CLAUDE_CODE: &str = "claude-code-20250219";
const CONTEXT_1M: &str = "context-1m-2025-08-07";
const MID_CONV_SYSTEM: &str = "mid-conversation-system-2026-04-07";
const PER_TURN_CONTROL: &str = "per-turn-control-2026-07-01";
const PER_TURN_TIMING: &str = "timing-2026-09-09";
const MID_CONV_TOOL_CHANGES: &str = "mid-conversation-tool-changes-2026-07-01";
const INLINE_TOOLS: &str = "inline-tools-2026-09-15";
const MID_CONV_CLEAR_AT: &str = "mid-conversation-system-clear-at-2026-08-21";
const DANGEROUS_TOOL_USE: &str = "dangerous-tool-use-2026-09-03";
pub(crate) const ADVISOR_TOOL: &str = "advisor-tool-2026-03-01";
const ADVANCED_TOOL_USE: &str = "advanced-tool-use-2025-11-20";
pub(crate) const EFFORT: &str = "effort-2025-11-24";
pub(crate) const SERVER_SIDE_FALLBACK: &str = "server-side-fallback-2026-06-01";
const FALLBACK_CREDIT: &str = "fallback-credit-2026-06-01";
const STRUCTURED_OUTPUTS: &str = "structured-outputs-2025-12-15";
pub(crate) const THINKING_DISPLAY_UPDATES: &str = "thinking-display-updates-2026-08-18";
const THINKING_BINDING: &str = "thinking-binding-controls-2026-08-01";
const THINKING_RESUMPTION: &str = "thinking-resumption-2026-07-17";
pub(crate) const EXTENDED_CACHE_TTL: &str = "extended-cache-ttl-2025-04-11";
const PROMPT_CACHING_EVICT: &str = "prompt-caching-evict-2026-05-12";
const CACHE_DIAGNOSIS: &str = "cache-diagnosis-2026-04-07";
const REDACT_THINKING: &str = "redact-thinking-2026-02-12";
const AFK_MODE: &str = "afk-mode-2026-01-31";
const THINKING_TOKEN_COUNT: &str = "thinking-token-count-2026-05-13";
const PROMPT_CACHING_SCOPE: &str = "prompt-caching-scope-2026-01-05";

/// Sent on every "cli" entrypoint Messages request, after claude-code-20250219 (before
/// it on Haiku models). The 2.1.325 capture never shows prompt-caching-scope: it is
/// gated by account data the capture could not reach, and earlier real-account
/// measurements put it here.
const CONSTANT: [&str; 4] = [
    "interleaved-thinking-2025-05-14",
    REDACT_THINKING,
    "context-management-2025-06-27",
    PROMPT_CACHING_SCOPE,
];
const TRAILING: [&str; 3] = [SERVER_SIDE_FALLBACK, FALLBACK_CREDIT, STRUCTURED_OUTPUTS];

/// Every beta the proxy assembles or gates. Other caller betas are newer-client
/// features and pass through (#5738).
pub(crate) fn managed(beta: &str) -> bool {
    const MANAGED: [&str; 27] = [
        TOKEN_COUNTING,
        FAST_MODE,
        OAUTH,
        CLAUDE_CODE,
        CONTEXT_1M,
        MID_CONV_SYSTEM,
        PER_TURN_CONTROL,
        PER_TURN_TIMING,
        MID_CONV_TOOL_CHANGES,
        INLINE_TOOLS,
        MID_CONV_CLEAR_AT,
        DANGEROUS_TOOL_USE,
        ADVISOR_TOOL,
        ADVANCED_TOOL_USE,
        EFFORT,
        SERVER_SIDE_FALLBACK,
        FALLBACK_CREDIT,
        STRUCTURED_OUTPUTS,
        THINKING_DISPLAY_UPDATES,
        THINKING_BINDING,
        THINKING_RESUMPTION,
        EXTENDED_CACHE_TTL,
        PROMPT_CACHING_EVICT,
        CACHE_DIAGNOSIS,
        REDACT_THINKING,
        AFK_MODE,
        THINKING_TOKEN_COUNT,
    ];
    let beta = beta.trim();
    MANAGED.contains(&beta) || CONSTANT.contains(&beta) || TRAILING.contains(&beta)
}

pub(crate) type Requested = HashSet<String>;

/// `claudeRequestedBetas`: header and body-lifted betas.
pub(crate) fn requested(incoming: &str, extra: &[String]) -> Requested {
    incoming
        .split(',')
        .chain(extra.iter().map(String::as_str))
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(str::to_owned)
        .collect()
}

fn model(body: &str) -> String {
    rawjson::string(body, "model")
}

fn sonnet55(m: &str) -> bool {
    let m = canonical_model(m);
    m == "claude-sonnet-5-5" || m.starts_with("claude-sonnet-5-5-") || m.starts_with("claude-sonnet-5-5[")
}
fn sonnet5(m: &str) -> bool {
    let m = canonical_model(m);
    !sonnet55(&m) && (m == "claude-sonnet-5" || m.starts_with("claude-sonnet-5-") || m.starts_with("claude-sonnet-5["))
}
fn fable52(m: &str) -> bool {
    let m = canonical_model(m);
    m == "claude-fable-5-2" || m.starts_with("claude-fable-5-2-") || m.starts_with("claude-fable-5-2[")
}
fn haiku55(m: &str) -> bool {
    let m = canonical_model(m);
    m == "claude-haiku-5-5" || m.starts_with("claude-haiku-5-5-") || m.starts_with("claude-haiku-5-5[")
}
/// Models Claude Code's cli entrypoint sends `thinking.display: updates` for when
/// thinking is active (the 2.1.325 capture adds Fable 5.2 and Haiku 5.5).
pub(crate) fn progress_display(m: &str) -> bool {
    is_opus55(m) || is_fable51(m) || fable52(m) || sonnet5(m) || sonnet55(m) || haiku55(m)
}
/// Models Claude Code 2.1.325 sends per-turn-control-2026-07-01 for on every request.
fn per_turn_control_model(m: &str) -> bool {
    per_turn_timing_base(m) || sonnet55(m) || haiku55(m)
}
/// Opus 5.5, Fable 5.1 and Fable 5.2: per-turn control and per-turn timing.
fn per_turn_timing_base(m: &str) -> bool {
    is_opus55(m) || canonical_model(m).starts_with("claude-fable-5-1") || fable52(m)
}
fn per_turn_timing_model(m: &str) -> bool {
    per_turn_timing_base(m) || canonical_model(m).starts_with("claude-mythos-5-1")
}

fn include_per_turn_control(body: &str, r: &Requested) -> bool {
    r.contains(PER_TURN_CONTROL) || per_turn_control_model(&model(body))
}
fn include_per_turn_timing(body: &str, r: &Requested) -> bool {
    if r.contains(PER_TURN_TIMING) {
        return true;
    }
    per_turn_timing_model(&model(body))
        && (rawjson::get(body, "output_config.timing").exists()
            || rawjson::get(body, "messages")
                .values()
                .iter()
                .any(|m| m.get("output_config.timing").exists()))
}
fn include_inline_tools(body: &str, r: &Requested) -> bool {
    r.contains(INLINE_TOOLS)
        || rawjson::get(body, "messages").values().iter().any(|m| {
            m.get("content").values().iter().any(|b| {
                b.get("type").str().trim().eq_ignore_ascii_case("tool_addition") && b.get("tool.definition").exists()
            })
        })
}
fn include_clear_at(body: &str, r: &Requested) -> bool {
    r.contains(MID_CONV_CLEAR_AT)
        || progress_display(&model(body))
        || rawjson::get(body, "messages")
            .values()
            .iter()
            .any(|m| m.get("clear_at").exists())
}

/// `claudeRequestSupportsEffort`.
pub(crate) fn supports_effort(body: &str) -> bool {
    if body.is_empty() {
        return true;
    }
    if signals::probe_or_helper(body) {
        return false;
    }
    // Haiku 5.5 sends effort in the 2.1.325 capture; Haiku 4.5 does not.
    let m = model(body);
    if m.trim().go_lower().contains("haiku") && !haiku55(&m) {
        return false;
    }
    rawjson::string(body, "thinking.type").trim().go_lower() != "disabled"
}

fn display_updates(body: &str) -> bool {
    let d = rawjson::get(body, "thinking.display");
    d.kind() == rawjson::Kind::String && d.str().trim().go_eq_fold("updates")
}
fn display_set(body: &str) -> bool {
    let d = rawjson::get(body, "thinking.display");
    d.kind() == rawjson::Kind::String && !d.str().trim().is_empty()
}
fn tool_types(body: &str) -> Vec<String> {
    rawjson::get(body, "tools")
        .items()
        .iter()
        .map(|t| t.get("type").str().trim().go_lower())
        .collect()
}
fn advanced_tool_use(body: &str) -> bool {
    let tools = rawjson::get(body, "tools");
    tools.kind() == rawjson::Kind::Array
        && tools.array().iter().any(|t| {
            t.get("type").str().trim().go_lower().starts_with("tool_search_tool_")
                || t.get("defer_loading").bool()
                || t.get("input_examples").exists()
                || t.get("allowed_callers").exists()
        })
}
pub(crate) fn has_advisor_tool(body: &str) -> bool {
    tool_types(body).iter().any(|t| t.starts_with("advisor_"))
}
pub(crate) fn uses_fast_mode(body: &str, r: &Requested) -> bool {
    let speed = rawjson::get(body, "speed");
    r.contains(FAST_MODE) || (speed.kind() == rawjson::Kind::String && speed.str().trim().go_eq_fold("fast"))
}

/// `claudeCodeCLIBetas`, in the order of the 2.1.325 capture: claude-code-20250219
/// follows the constant group on Haiku models, the quota probe stops after that group,
/// mid-conversation-system is sent only when requested (2.1.325 omits it even with a
/// role=system turn), thinking-token-count follows the tool betas, and effort precedes
/// dangerous-tool-use. The capture has no advisor tool; advisor-tool before
/// thinking-token-count follows Go's 2.1.321 capture (`b4e7dbd2`).
pub(crate) fn cli(body: &str, r: &Requested, oauth: bool) -> String {
    let quota_probe = signals::probe(body);
    let haiku = !signals::probe_or_helper(body) && model(body).go_lower().contains("haiku");
    let mut b: Vec<&str> = Vec::new();
    if !haiku {
        b.push(CLAUDE_CODE);
    }
    if oauth {
        b.push(OAUTH);
    }
    if r.contains(CONTEXT_1M) {
        b.push(CONTEXT_1M);
    }
    let redact = !display_set(body);
    b.extend(CONSTANT.iter().filter(|x| **x != REDACT_THINKING || redact));
    if haiku {
        b.push(CLAUDE_CODE);
    }
    if quota_probe {
        return b.join(",");
    }
    let legacy = legacy_system_reminder(body);
    if !legacy {
        if r.contains(MID_CONV_SYSTEM) {
            b.push(MID_CONV_SYSTEM);
        }
        if include_per_turn_control(body, r) {
            b.push(PER_TURN_CONTROL);
        }
        if include_per_turn_timing(body, r) {
            b.push(PER_TURN_TIMING);
        }
        if !sonnet5(&model(body)) {
            b.push(MID_CONV_TOOL_CHANGES);
        }
        if include_inline_tools(body, r) {
            b.push(INLINE_TOOLS);
        }
    } else {
        if include_per_turn_control(body, r) {
            b.push(PER_TURN_CONTROL);
        }
        if include_per_turn_timing(body, r) {
            b.push(PER_TURN_TIMING);
        }
    }
    if r.contains(ADVISOR_TOOL) || has_advisor_tool(body) {
        b.push(ADVISOR_TOOL);
    }
    b.push(THINKING_TOKEN_COUNT);
    if r.contains(ADVANCED_TOOL_USE) || advanced_tool_use(body) {
        b.push(ADVANCED_TOOL_USE);
    }
    if !legacy && include_clear_at(body, r) {
        b.push(MID_CONV_CLEAR_AT);
    }
    if supports_effort(body) {
        b.push(EFFORT);
    }
    if r.contains(DANGEROUS_TOOL_USE) || rawjson::get(body, "safeguards").exists() {
        b.push(DANGEROUS_TOOL_USE);
    }
    let probe = signals::probe_or_helper(body);
    let fallbacks = rawjson::get(body, "fallbacks").exists();
    if !probe && (r.contains(SERVER_SIDE_FALLBACK) || fallbacks) {
        b.push(SERVER_SIDE_FALLBACK);
    }
    if r.contains(FALLBACK_CREDIT) || rawjson::get(body, "fallback_credit_token").exists() || (oauth && fallbacks) {
        b.push(FALLBACK_CREDIT);
    }
    if r.contains(STRUCTURED_OUTPUTS) {
        b.push(STRUCTURED_OUTPUTS);
    }
    let thinking = rawjson::string(body, "thinking.type");
    if r.contains(THINKING_BINDING)
        || rawjson::get(body, "thinking.block_binding").exists()
        || (progress_display(&model(body)) && thinking == "adaptive")
    {
        b.push(THINKING_BINDING);
    }
    if !probe && thinking != "disabled" && (r.contains(THINKING_DISPLAY_UPDATES) || display_updates(body)) {
        b.push(THINKING_DISPLAY_UPDATES);
    }
    if r.contains(THINKING_RESUMPTION) {
        b.push(THINKING_RESUMPTION);
    }
    if uses_fast_mode(body, r) {
        b.push(FAST_MODE);
    }
    if r.contains(AFK_MODE) {
        b.push(AFK_MODE);
    }
    if !probe
        && ((oauth && !signals::subagent(&Default::default(), body))
            || r.contains(EXTENDED_CACHE_TTL)
            || signals::has_1h_ttl(body))
    {
        b.push(EXTENDED_CACHE_TTL);
    }
    if r.contains(PROMPT_CACHING_EVICT) || body.contains("\"evict_on_complete\"") {
        b.push(PROMPT_CACHING_EVICT);
    }
    if rawjson::get(body, "diagnostics").kind() == rawjson::Kind::Object {
        b.push(CACHE_DIAGNOSIS);
    }
    b.join(",")
}

/// count_tokens profile: claude-code, OAuth when applicable, then the fixed trio.
pub(crate) fn count_tokens(oauth: bool) -> String {
    let mut b = vec![CLAUDE_CODE];
    if oauth {
        b.push(OAUTH);
    }
    b.extend([
        "interleaved-thinking-2025-05-14",
        "context-management-2025-06-27",
        TOKEN_COUNTING,
    ]);
    b.join(",")
}

fn dedup(betas: &str, skip: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    betas
        .split(',')
        .map(str::trim)
        .filter(|b| !b.is_empty() && *b != skip && seen.insert(b.to_string()))
        .map(str::to_owned)
        .collect()
}

fn insert_oauth(parts: &mut Vec<String>) {
    if !parts.iter().any(|p| p == OAUTH) {
        let at = usize::from(parts.first().is_some_and(|p| p == CLAUDE_CODE));
        parts.insert(at, OAUTH.into());
    }
}

/// `withClaudeCountTokensOAuthBeta`.
pub(crate) fn with_count_oauth(betas: &str) -> String {
    let mut parts = dedup(betas, "");
    insert_oauth(&mut parts);
    parts.join(",")
}

/// `withClaudeOAuthCredentialBetas`: the selected OAuth account must be described
/// even when a native caller did not know CPA would pick one.
pub(crate) fn with_oauth_credential(betas: &str, extended_ttl: bool) -> String {
    let mut parts = dedup(betas, "");
    let had_ttl = parts.iter().any(|p| p == EXTENDED_CACHE_TTL);
    insert_oauth(&mut parts);
    if extended_ttl && !had_ttl {
        parts.push(EXTENDED_CACHE_TTL.into());
    }
    parts.join(",")
}

/// `withoutClaudeBeta`.
pub(crate) fn without(betas: &str, remove: &str) -> String {
    betas
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty() && *p != remove)
        .collect::<Vec<_>>()
        .join(",")
}

/// `withClaudeExtendedCacheTTLBeta`.
pub(crate) fn with_extended_ttl(betas: &str) -> String {
    let mut parts = dedup(betas, "");
    if !parts.iter().any(|p| p == EXTENDED_CACHE_TTL) {
        parts.push(EXTENDED_CACHE_TTL.into());
    }
    parts.join(",")
}

/// `withClaudeAdvisorToolBeta`: advisor goes before the first beta that follows it
/// on the native wire.
pub(crate) fn with_advisor(betas: &str) -> String {
    if betas.trim().is_empty() {
        return ADVISOR_TOOL.into();
    }
    let mut parts = dedup(betas, ADVISOR_TOOL);
    let boundary = [
        ADVANCED_TOOL_USE,
        EFFORT,
        SERVER_SIDE_FALLBACK,
        FALLBACK_CREDIT,
        STRUCTURED_OUTPUTS,
        FAST_MODE,
        AFK_MODE,
        EXTENDED_CACHE_TTL,
        CACHE_DIAGNOSIS,
    ];
    let at = parts
        .iter()
        .position(|p| boundary.contains(&p.as_str()))
        .unwrap_or(parts.len());
    parts.insert(at, ADVISOR_TOOL.into());
    parts.join(",")
}

/// A list Go assembled for the Claude Code 2.1.280 profile, rewritten into the 2.1.325
/// order `cli` follows: Go goldens keep their recording, and replays accept either
/// form. Not a capture: the 2.1.325 order itself is pinned by the capture test below.
#[cfg(test)]
pub(crate) fn from_2_1_280(go: &str, body: &str) -> String {
    let had_token_count = go.split(',').any(|b| b == THINKING_TOKEN_COUNT);
    let mut b: Vec<&str> = go
        .split(',')
        .filter(|b| *b != MID_CONV_SYSTEM && *b != THINKING_TOKEN_COUNT)
        .collect();
    if signals::probe(body) {
        b.retain(|x| [CLAUDE_CODE, OAUTH, CONTEXT_1M].contains(x) || CONSTANT.contains(x));
        return b.join(",");
    }
    if !signals::probe_or_helper(body)
        && model(body).go_lower().contains("haiku")
        && let Some(i) = b.iter().position(|x| *x == CLAUDE_CODE)
    {
        b.remove(i);
        let at = b.iter().rposition(|x| CONSTANT.contains(x)).map_or(0, |i| i + 1);
        b.insert(at, CLAUDE_CODE);
    }
    if had_token_count {
        let follows = [
            ADVANCED_TOOL_USE,
            MID_CONV_CLEAR_AT,
            EFFORT,
            DANGEROUS_TOOL_USE,
            SERVER_SIDE_FALLBACK,
            FALLBACK_CREDIT,
            STRUCTURED_OUTPUTS,
            THINKING_BINDING,
            THINKING_DISPLAY_UPDATES,
            THINKING_RESUMPTION,
            FAST_MODE,
            AFK_MODE,
            EXTENDED_CACHE_TTL,
            PROMPT_CACHING_EVICT,
            CACHE_DIAGNOSIS,
        ];
        let at = b.iter().position(|x| follows.contains(x)).unwrap_or(b.len());
        b.insert(at, THINKING_TOKEN_COUNT);
    }
    if let (Some(e), Some(d)) = (
        b.iter().position(|x| *x == EFFORT),
        b.iter().position(|x| *x == DANGEROUS_TOOL_USE),
    ) && d < e
    {
        b.swap(d, e);
    }
    b.join(",")
}

/// Whether `ours` is Go's recorded `Anthropic-Beta` value, as recorded or in the 2.1.325
/// order ([`from_2_1_280`]).
#[cfg(test)]
pub(crate) fn matches_go(ours: &str, go: &str, body: &str) -> bool {
    ours == go || ours == from_2_1_280(go, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Claude Code 2.1.325's own lists, from the loopback capture in
    /// `testdata/claude_code_2_1_325.json` (see its `source` and `limits`): cli and sdk-cli,
    /// API-key and OAuth-token modes, Fable 5.2, Fable 5.1, Opus 5.5, Sonnet 5.5, Haiku 5.5,
    /// Haiku 4.5 and the quota probe.
    #[test]
    fn cli_betas_match_claude_code_2_1_325_captures() {
        let doc = include_str!("testdata/claude_code_2_1_325.json");
        // Gated by feature flags or account data the network-denied capture could not
        // reach, so the capture cannot show them; assembly keeps them where earlier
        // measurements put them.
        let unobservable = [PROMPT_CACHING_SCOPE, MID_CONV_CLEAR_AT, THINKING_BINDING];
        let strip = |s: String| {
            s.split(',')
                .filter(|b| !unobservable.contains(b))
                .collect::<Vec<_>>()
                .join(",")
        };
        let captures = rawjson::get(doc, "captures");
        assert_eq!(captures.array().len(), 10);
        for capture in captures.array() {
            let name = capture.get("name").str().to_owned();
            let body = capture.get("body");
            let body = body.json();
            let header = |n: &str| {
                capture
                    .get("headers")
                    .array()
                    .iter()
                    .find(|h| h.get("0").str().eq_ignore_ascii_case(n))
                    .map(|h| h.get("1").str().to_owned())
            };
            let native = header("anthropic-beta").unwrap();
            let oauth = header("authorization").is_some();
            assert_eq!(
                strip(cli(body, &requested(&native, &[]), oauth)),
                native,
                "{name}: caller sent the native list"
            );
            // A caller sending no betas gets the same list without afk-mode, which Claude
            // Code sends only from auto-mode sessions.
            assert_eq!(
                strip(cli(body, &Requested::new(), oauth)),
                without(&native, AFK_MODE),
                "{name}: caller sent no betas"
            );
        }
    }

    /// Fable 5.2 gates. Per-turn control and progress display follow the 2.1.325 capture;
    /// per-turn timing follows a caller's `output_config.timing` body on the models whose
    /// timing beta was already assembled, now with Fable 5.2 (Go `6db7d4b8`; the capture
    /// sends no timing body).
    #[test]
    fn fable_5_2_gates() {
        let timing = |model: &str| {
            let body = format!(r#"{{"model":"{model}","output_config":{{"timing":{{"type":"enabled"}}}}}}"#);
            include_per_turn_timing(&body, &Requested::new())
        };
        for model in ["claude-fable-5-2", "claude-fable-5-2[1m]", "anthropic/claude-fable-5-2"] {
            assert!(per_turn_control_model(model), "{model}");
            assert!(progress_display(model), "{model}");
            assert!(timing(model), "{model}");
        }
        // Sonnet 5.5 and Haiku 5.5 send per-turn control (capture); timing stays unassembled.
        for model in ["claude-sonnet-5-5", "claude-haiku-5-5"] {
            assert!(per_turn_control_model(model), "{model}");
            assert!(!timing(model), "{model}");
        }
        assert!(progress_display("claude-haiku-5-5"));
        for model in ["claude-fable-5", "claude-fable-5-20", "claude-haiku-4-5-20251001"] {
            assert!(!per_turn_control_model(model), "{model}");
            assert!(!progress_display(model), "{model}");
        }
    }

    #[test]
    fn harness_profiles_match_go_captures() {
        let body = r#"{"model":"claude-sonnet-4-6","diagnostics":{"previous_message_id":null}}"#;
        // Recorded from Go's 2.1.280 profile; thinking-token-count now follows the tool
        // betas (2.1.325).
        let go = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,effort-2025-11-24,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07";
        assert_eq!(cli(body, &Requested::new(), true), from_2_1_280(go, body));
        assert_eq!(
            from_2_1_280(go, body),
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,context-management-2025-06-27,prompt-caching-scope-2026-01-05,thinking-token-count-2026-05-13,effort-2025-11-24,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07"
        );
        assert_eq!(
            count_tokens(true),
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,context-management-2025-06-27,token-counting-2024-11-01"
        );
        assert_eq!(
            with_oauth_credential("claude-code-20250219,interleaved-thinking-2025-05-14", true),
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,extended-cache-ttl-2025-04-11"
        );
        assert_eq!(
            with_advisor("a,effort-2025-11-24,b"),
            "a,advisor-tool-2026-03-01,effort-2025-11-24,b"
        );
    }
}
