//! Claude Code cloaking (claude_executor_cloaking.go): billing and identity system
//! blocks, caller prompt relocation, current-date reminder, context management,
//! cache-control placement and TTL policy, and the fake/derived user identity.

use cpa_core::gostr::GoStr;
use sha2::{Digest, Sha256};

use super::signals;
use crate::rawjson::{self, js_string};

pub(crate) const CLI_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
pub(crate) const BILLING_PREFIX: &str = "x-anthropic-billing-header:";
const FINGERPRINT_SALT: &str = "59cf53e54c78";
pub(crate) const CONTEXT_MANAGEMENT: &str = r#"{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}"#;
pub(crate) const FABLE_REPORTING: &str = "# Reporting outcomes\n\nReport what actually happened, not what you intended. When you say something is done, sent, saved, fixed, or verified, that claim must rest on a result you observed in this session — tool output, the file as it now reads, the page as it now loads — not on what the step should have produced. If you did not check, say you did not check. If any step failed, was skipped, or came back different from what you expected, say so in the first sentence of your report, before anything else, even when the rest of the work succeeded. Never quietly work around a failure in a way that makes it look resolved; a problem the user can see is recoverable, one your summary hides is not. When you stop before the task is complete, your first line says so plainly and names what is left. Do not describe partial work as done, and do not let a summary read as more certain than the evidence behind it.";

/// Official model IDs that reject a mid-conversation `role: system` turn.
const LEGACY_SYSTEM_REMINDER_MODELS: &[&str] = &[
    "claude-3-5-haiku-20241022",
    "claude-3-5-haiku-latest",
    "claude-3-7-sonnet-20250219",
    "claude-3-7-sonnet-latest",
    "claude-haiku-4-5",
    "claude-haiku-4-5-20251001",
    "claude-opus-4",
    "claude-opus-4-20250514",
    "claude-opus-4-1",
    "claude-opus-4-1-20250805",
    "claude-opus-4-5",
    "claude-opus-4-5-20251101",
    "claude-opus-4-6",
    "claude-opus-4-7",
    "claude-sonnet-4",
    "claude-sonnet-4-20250514",
    "claude-sonnet-4-5",
    "claude-sonnet-4-5-20250929",
    "claude-sonnet-4-6",
];

/// Lowercased model without any `provider/` prefix.
pub(crate) fn canonical_model(model: &str) -> String {
    let m = model.trim().go_lower();
    m.rsplit('/').next().unwrap_or_default().to_owned()
}

pub(crate) fn legacy_system_reminder(body: &str) -> bool {
    legacy_model(rawjson::get(body, "model").str())
}

/// Whether `model` rejects a mid-conversation `role: system` turn.
pub(crate) fn legacy_model(model: &str) -> bool {
    LEGACY_SYSTEM_REMINDER_MODELS.contains(&canonical_model(model).as_str())
}

pub(crate) fn is_fable51(model: &str) -> bool {
    let m = model.trim().go_lower();
    ["fable-5-1", "fable-5.1", "mythos-5-1", "mythos-5.1"].iter().any(|t| {
        m.match_indices(t).take(1).any(|(i, _)| {
            let next = i + t.len();
            next >= m.len() || !m.as_bytes()[next].is_ascii_digit()
        })
    })
}
pub(crate) fn is_opus55(model: &str) -> bool {
    let m = canonical_model(model);
    m == "claude-opus-5-5" || m.starts_with("claude-opus-5-5[")
}

/// `{"type":"text","text":...}` with JSON.stringify escaping and an optional marker.
pub(crate) fn text_block(text: &str, cache: Option<&str>) -> String {
    match cache {
        Some(ttl) if !ttl.is_empty() => {
            format!(
                r#"{{"type":"text","text":{},"cache_control":{{"type":"ephemeral","ttl":{}}}}}"#,
                js_string(text),
                js_string(ttl)
            )
        }
        Some(_) => format!(
            r#"{{"type":"text","text":{},"cache_control":{{"type":"ephemeral"}}}}"#,
            js_string(text)
        ),
        None => format!(r#"{{"type":"text","text":{}}}"#, js_string(text)),
    }
}
const EPHEMERAL: Option<&str> = Some("");

/// The 3-character build hash Claude Code embeds in `cc_version`, sampling UTF-16
/// code units 4, 7 and 20 of the first user text.
fn fingerprint(message: &str, version: &str) -> String {
    let units: Vec<u16> = message.encode_utf16().collect();
    let sampled: Vec<u16> = [4, 7, 20]
        .iter()
        .map(|&i| units.get(i).copied().unwrap_or(u16::from(b'0')))
        .collect();
    let sampled = String::from_utf16_lossy(&sampled);
    let digest = Sha256::digest(format!("{FINGERPRINT_SALT}{sampled}{version}").as_bytes());
    super::session::hex(&digest)[..3].to_owned()
}

pub(crate) struct Billing<'a> {
    pub signed: bool,
    pub version: &'a str,
    pub message: &'a str,
    pub entrypoint: &'a str,
    pub workload: &'a str,
    pub subagent: bool,
    pub prev_req: &'a str,
    pub prompt_id: &'a str,
    pub human_turn: bool,
}

/// `generateBillingHeader`.
pub(crate) fn billing_header(b: &Billing<'_>) -> String {
    let entrypoint = if b.entrypoint.is_empty() { "cli" } else { b.entrypoint };
    let mut s = format!(
        "{BILLING_PREFIX} cc_version={}.{}; cc_entrypoint={entrypoint};",
        b.version,
        fingerprint(b.message, b.version)
    );
    if b.signed {
        s.push_str(" cch=00000;");
    }
    if !b.workload.is_empty() {
        s.push_str(&format!(" cc_workload={};", b.workload));
    }
    if b.subagent {
        s.push_str(" cc_is_subagent=true;");
    }
    if b.signed {
        if !b.prev_req.is_empty() {
            s.push_str(&format!(" cc_prev_req={};", b.prev_req));
        }
        if !b.prompt_id.is_empty() {
            s.push_str(&format!(" cc_prompt_id={};", b.prompt_id));
        }
        if b.human_turn {
            s.push_str(" cc_turn_origin=human;");
        }
    }
    s
}

pub(crate) fn first_user_index(body: &str) -> Option<usize> {
    rawjson::get(body, "messages")
        .items()
        .iter()
        .position(|m| m.get("role").str() == "user")
}

fn date_reminder_prefix(text: &str) -> bool {
    text.starts_with("<system-reminder>\nAs you answer the user's questions, you can use the following context:\n# currentDate\nToday's date is ")
}
fn context_reminder(text: &str) -> bool {
    text.starts_with("<system-reminder>") && text.contains("</system-reminder>")
}

/// `claudeBillingFingerprintMessageText`: the first user text that is not a reminder.
pub(crate) fn fingerprint_message(body: &str) -> String {
    let Some(idx) = first_user_index(body) else {
        return String::new();
    };
    let content = rawjson::get(body, &format!("messages.{idx}.content")).json().to_owned();
    let content = rawjson::parse(&content);
    match content.kind() {
        rawjson::Kind::String => content.str().to_owned(),
        rawjson::Kind::Array => content
            .array()
            .iter()
            .filter(|p| p.get("type").str() == "text")
            .map(|p| p.get("text").str().to_owned())
            .find(|t| !date_reminder_prefix(t) && !context_reminder(t))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// `isValidClaudeCacheControl` (v8.0.27, be8c35f): only an object whose `type` is the
/// string `ephemeral` is a breakpoint; `null` and other shapes are ignored.
pub(crate) fn valid_cache_control(cc: &rawjson::Value) -> bool {
    cc.kind() == rawjson::Kind::Object
        && cc.get("type").kind() == rawjson::Kind::String
        && cc.get("type").str() == "ephemeral"
}

/// A caller system block that cloaking moves: its text and, when it carried a valid
/// breakpoint, that breakpoint's raw JSON (`forwardedClaudeSystemPromptBlock`).
#[derive(Clone)]
pub(crate) struct Forwarded {
    pub text: String,
    pub cache: Option<String>,
}

impl Forwarded {
    pub(crate) fn plain(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            cache: None,
        }
    }
}

/// `collectForwardedClaudeSystemPromptBlocksWithCache`.
pub(crate) fn forwarded_system_blocks(body: &str) -> Vec<Forwarded> {
    let system = rawjson::get(body, "system");
    let keep = |t: &str| !t.trim().is_empty() && !t.trim_start().starts_with(BILLING_PREFIX) && t != CLI_IDENTITY;
    match system.kind() {
        rawjson::Kind::Array => system
            .array()
            .iter()
            .filter(|p| p.get("type").str() == "text" && keep(p.get("text").str()))
            .map(|p| {
                let cc = p.get("cache_control");
                Forwarded {
                    text: p.get("text").str().to_owned(),
                    cache: valid_cache_control(&cc).then(|| cc.json().to_owned()),
                }
            })
            .collect(),
        rawjson::Kind::String if keep(system.str()) => vec![Forwarded::plain(system.str())],
        _ => Vec::new(),
    }
}

/// `buildForwardedSystemBlock` (v8.0.27, eb6a768): CPA's default breakpoint, or in
/// explicit cache mode exactly the breakpoint the caller put on the block (or none).
fn forwarded_block(text: &str, block: &Forwarded, explicit: bool) -> String {
    match (&block.cache, explicit) {
        (Some(cc), true) => format!(r#"{{"type":"text","text":{},"cache_control":{cc}}}"#, js_string(text)),
        (None, true) => text_block(text, None),
        (_, false) => text_block(text, EPHEMERAL),
    }
}

/// `validateClaudeCallerSystemBlocks`: non-text system blocks have no destination.
pub(crate) fn invalid_system_block(body: &str) -> Option<String> {
    let system = rawjson::get(body, "system");
    if system.kind() != rawjson::Kind::Array {
        return None;
    }
    system.array().iter().enumerate().find_map(|(i, p)| {
        let kind = p.get("type").str().trim().to_owned();
        (kind != "text").then(|| {
            let kind = if kind.is_empty() { "unknown".to_owned() } else { kind };
            format!(
                "invalid_request_error: system.{i}.type: Input should be 'text'. System instructions support text only, but this block has type {}. Move non-text content into a user message.",
                rawjson::go_string(&kind).replace("\\u003c", "<").replace("\\u003e", ">").replace("\\u0026", "&")
            )
        })
    })
}

fn reminder(text: &str) -> String {
    let mut r = format!("<system-reminder>\n{text}");
    if !text.ends_with('\n') {
        r.push('\n');
    }
    r.push_str("</system-reminder>");
    r
}

/// Legacy models: caller system prompts become reminders in the first user turn
/// (`prependClaudeSystemReminderBlocksToFirstUserMessage`). In explicit cache mode a
/// reminder keeps the breakpoint its system block carried.
pub(crate) fn prepend_reminders(body: &str, blocks: &[Forwarded], explicit: bool) -> String {
    let Some(idx) = first_user_index(body) else {
        return body.to_owned();
    };
    if blocks.is_empty() {
        return body.to_owned();
    }
    let reminders: Vec<String> = blocks.iter().map(|b| reminder(&b.text)).collect();
    let reminder_block = |i: usize| match (&blocks[i].cache, explicit) {
        (Some(cc), true) => format!(
            r#"{{"type":"text","text":{},"cache_control":{cc}}}"#,
            js_string(&reminders[i])
        ),
        _ => text_block(&reminders[i], None),
    };
    let path = format!("messages.{idx}.content");
    let content = rawjson::get(body, &path);
    if content.kind() == rawjson::Kind::Array {
        let blocks = content.array();
        let mut existing: std::collections::HashMap<String, usize> = Default::default();
        for b in &blocks {
            if b.get("type").str() == "text" {
                *existing.entry(b.get("text").str().to_owned()).or_default() += 1;
            }
        }
        let mut new_blocks = Vec::new();
        for (i, r) in reminders.iter().enumerate() {
            match existing.get_mut(r) {
                Some(n) if *n > 0 => *n -= 1,
                _ => new_blocks.push(reminder_block(i)),
            }
        }
        if new_blocks.is_empty() {
            return body.to_owned();
        }
        let at = blocks
            .iter()
            .take_while(|b| b.get("type").str() == "tool_result")
            .count();
        let mut raw: Vec<String> = blocks.iter().map(|b| b.json().to_owned()).collect();
        raw.splice(at..at, new_blocks);
        return rawjson::set_raw(body, &path, &format!("[{}]", raw.join(",")));
    }
    if content.kind() == rawjson::Kind::String {
        let mut raw: Vec<String> = (0..reminders.len()).map(reminder_block).collect();
        raw.push(text_block(content.str(), None));
        return rawjson::set_raw(body, &path, &format!("[{}]", raw.join(",")));
    }
    body.to_owned()
}

pub(crate) fn message_text(content: &rawjson::Value) -> String {
    match content.kind() {
        rawjson::Kind::String => content.str().to_owned(),
        rawjson::Kind::Array => content
            .array()
            .iter()
            .filter(|b| b.get("type").str() == "text")
            .map(|b| b.get("text").str().to_owned())
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

/// `isClaudeSystemDirectiveMessage` (v8.0.27, 2d8ec73): a `role: system` turn with an
/// empty content array and an `output_config`, which Anthropic accepts anywhere.
fn system_directive(message: &rawjson::Value) -> bool {
    let content = message.get("content");
    message.get("role").str() == "system"
        && message.get("output_config").exists()
        && content.kind() == rawjson::Kind::Array
        && content.array().is_empty()
}

/// `claudeLeadingUserRunEnd`: the index after the first user turn and the user turns
/// that follow it, skipping effort directives between them.
pub(crate) fn leading_user_run_end(messages: &[rawjson::Value], first_user: usize) -> usize {
    let mut at = first_user + 1;
    while at < messages.len() && (messages[at].get("role").str() == "user" || system_directive(&messages[at])) {
        at += 1;
    }
    at
}

/// Current models: each caller system block becomes an operator-level system turn
/// after the first user turn(s), leaving the cached top-level prefix untouched
/// (`insertClaudeMidConversationSystemBlocks`).
pub(crate) fn insert_mid_system(body: &str, blocks: &[Forwarded], explicit: bool) -> String {
    let Some(first) = first_user_index(body) else {
        return body.to_owned();
    };
    let messages = rawjson::get(body, "messages");
    if blocks.is_empty() || messages.kind() != rawjson::Kind::Array {
        return body.to_owned();
    }
    let msgs = messages.array();
    let at = leading_user_run_end(&msgs, first);
    if msgs.len() - at >= blocks.len()
        && blocks.iter().enumerate().all(|(i, b)| {
            let m = &msgs[at + i];
            m.get("role").str() == "system" && message_text(&m.get("content")) == b.text
        })
    {
        return body.to_owned();
    }
    let inserted: Vec<String> = blocks
        .iter()
        .map(|b| {
            format!(
                r#"{{"role":"system","content":[{}]}}"#,
                forwarded_block(&b.text, b, explicit)
            )
        })
        .collect();
    // The messages joined with the new turns at `at`, written once: per-message copies,
    // their join and its bracketing were three more body-sized buffers.
    let size = messages.json().len() + inserted.iter().map(|m| m.len() + 1).sum::<usize>();
    let mut array = String::with_capacity(size);
    array.push('[');
    for (i, message) in msgs[..at]
        .iter()
        .map(|m| m.json())
        .chain(inserted.iter().map(String::as_str))
        .chain(msgs[at..].iter().map(|m| m.json()))
        .enumerate()
    {
        if i > 0 {
            array.push(',');
        }
        array.push_str(message);
    }
    array.push(']');
    rawjson::set_raw(body, "messages", &array)
}

/// Advisor results are bound to message positions; splicing would break them.
pub(crate) fn has_advisor_history(body: &str) -> bool {
    rawjson::get(body, "messages").items().iter().any(|msg| {
        let content = msg.get("content");
        match content.kind() {
            rawjson::Kind::Array => content.array().iter().any(|b| match b.get("type").str() {
                "advisor_tool_result" | "advisor_redacted_result" => true,
                "server_tool_use" => b.get("name").str() == "advisor",
                "tool_result" => {
                    let inner = b.get("content");
                    match inner.kind() {
                        rawjson::Kind::Array => inner
                            .array()
                            .iter()
                            .any(|x| x.get("type").str() == "advisor_redacted_result"),
                        rawjson::Kind::Object => inner.get("type").str() == "advisor_redacted_result",
                        _ => false,
                    }
                }
                _ => false,
            }),
            rawjson::Kind::Object => matches!(
                content.get("type").str(),
                "advisor_tool_result" | "advisor_redacted_result"
            ),
            _ => false,
        }
    })
}

pub(crate) fn date_reminder(date: &str) -> String {
    format!(
        "<system-reminder>\nAs you answer the user's questions, you can use the following context:\n# currentDate\nToday's date is {date}.\n\n      IMPORTANT: this context may or may not be relevant to your tasks. You should not respond to this context unless it is highly relevant to your task.\n</system-reminder>\n\n"
    )
}

/// `withEphemeralCacheControl`.
fn with_ephemeral(raw: &str) -> String {
    rawjson::set_raw(raw, "cache_control", r#"{"type":"ephemeral"}"#)
}

/// `injectClaudeCodeCurrentDateInternal`: date reminder first (after tool results), and
/// the first real user text carries the default breakpoint, except in explicit cache
/// mode (v8.0.27, eb6a768), where CPA adds no breakpoint.
pub(crate) fn inject_current_date(body: &str, date: &str, explicit: bool) -> String {
    let Some(idx) = first_user_index(body) else {
        return body.to_owned();
    };
    let path = format!("messages.{idx}.content");
    let content = rawjson::get(body, &path);
    let date_block = text_block(&date_reminder(date), None);
    if content.kind() == rawjson::Kind::String {
        let blank = content.str().trim().is_empty();
        let user = text_block(content.str(), if explicit || blank { None } else { EPHEMERAL });
        return rawjson::set_raw(body, &path, &format!("[{date_block},{user}]"));
    }
    if content.kind() != rawjson::Kind::Array {
        return body.to_owned();
    }
    let mut raw = Vec::new();
    let mut cached = false;
    for block in content.array() {
        if block.get("type").str() == "text" {
            let text = block.get("text").str().to_owned();
            if date_reminder_prefix(&text) {
                continue;
            }
            if !explicit && !cached && !context_reminder(&text) && !text.trim().is_empty() {
                raw.push(with_ephemeral(block.json()));
                cached = true;
                continue;
            }
        }
        raw.push(block.json().to_owned());
    }
    let at = raw
        .iter()
        .take_while(|b| rawjson::get(b, "type").str() == "tool_result")
        .count();
    raw.insert(at, date_block);
    rawjson::set_raw(body, &path, &format!("[{}]", raw.join(",")))
}

pub(crate) struct SystemPlan<'a> {
    pub strict: bool,
    pub billing: Billing<'a>,
    /// The session-pinned date, else today's.
    pub date: &'a str,
    /// `prompt_cache_options.mode: explicit`: CPA adds no breakpoints of its own.
    pub explicit: bool,
}

/// `checkSystemInstructionsWithSigningModeAt`. Takes the body so it is freed as soon as
/// the first rewrite has copied it.
///
/// Without advisor history, caller system text on a current model goes after the
/// leading user run, for API keys as for OAuth logins. Go v8.0.27 keeps it top-level
/// for an API key while the leading user run ends the conversation or holds more than
/// one turn (5d89040, eca1d11, 39632e5), so the second turn of a conversation moves it
/// and misses the cache from the identity block on (docs/DIFFERENCES-FROM-GO.md).
pub(crate) fn install_system(body: String, plan: &SystemPlan<'_>) -> String {
    let forwarded = forwarded_system_blocks(&body);
    let message = fingerprint_message(&body);
    let billing = Billing {
        message: &message,
        ..plan.billing
    };
    let mut blocks = vec![
        text_block(&billing_header(&billing), None),
        text_block(CLI_IDENTITY, if plan.explicit { None } else { EPHEMERAL }),
    ];
    if is_fable51(rawjson::get(&body, "model").str()) && !signals::probe_or_helper(&body) {
        blocks.push(text_block(FABLE_REPORTING, None));
    }
    let mut body = {
        let input = body;
        rawjson::set_raw(&input, "system", &format!("[{}]", blocks.join(",")))
    };
    if !plan.strict && !forwarded.is_empty() {
        if has_advisor_history(&body) {
            blocks.extend(forwarded.iter().map(|b| forwarded_block(&b.text, b, plan.explicit)));
            body = rawjson::set_raw(&body, "system", &format!("[{}]", blocks.join(",")));
        } else if legacy_system_reminder(&body) {
            body = prepend_reminders(&body, &forwarded, plan.explicit);
        } else {
            body = insert_mid_system(&body, &forwarded, plan.explicit);
        }
    }
    inject_current_date(&body, plan.date, plan.explicit)
}

/// `relocateClaudeSystemPromptForCountTokensWithPolicy`: count_tokens carries no CLI
/// blocks but caller system text is still counted in its Messages position, which is
/// [`install_system`]'s for API keys too.
pub(crate) fn relocate_system_for_count(body: &str, strict: bool, explicit: bool) -> String {
    if !rawjson::get(body, "system").exists() {
        return body.to_owned();
    }
    let forwarded = if strict {
        Vec::new()
    } else {
        forwarded_system_blocks(body)
    };
    if forwarded.is_empty() {
        return rawjson::delete(body, "system");
    }
    if has_advisor_history(body) {
        let blocks: Vec<_> = forwarded
            .iter()
            .map(|b| forwarded_block(&b.text, b, explicit))
            .collect();
        return rawjson::set_raw(body, "system", &format!("[{}]", blocks.join(",")));
    }
    let body = rawjson::delete(body, "system");
    if legacy_system_reminder(&body) {
        prepend_reminders(&body, &forwarded, explicit)
    } else {
        insert_mid_system(&body, &forwarded, explicit)
    }
}

/// `isExplicitPromptCacheMode` (v8.0.27, eb6a768): any of the request's forms sets
/// `prompt_cache_options.mode` to `explicit` (case-insensitive).
///
/// Per request: a substring scan of each form, and a key lookup only in a form that
/// may hold the key. The lookup is Go's gjson: with duplicate `prompt_cache_options`
/// members it reads the first one that has a `mode`.
pub(crate) fn explicit_cache_mode(bodies: &[&str]) -> bool {
    bodies.iter().any(|body| {
        signals::may_contain(body, &["prompt_cache_options"])
            && cpa_common::json::get(body.as_bytes(), "prompt_cache_options.mode")
                .str()
                .trim()
                .eq_ignore_ascii_case("explicit")
    })
}

/// `stripPromptCacheOptions`: the option is CPA's, never Anthropic's.
pub(crate) fn strip_prompt_cache_options(body: String) -> String {
    rawjson::delete_owned(body, "prompt_cache_options")
}

pub(crate) fn thinking_accepts_clear(body: &str) -> bool {
    matches!(rawjson::get(body, "thinking.type").str(), "enabled" | "adaptive")
}

/// `injectClaudeCodeContextManagement`.
pub(crate) fn inject_context_management(body: &str) -> Option<String> {
    (!rawjson::get(body, "context_management").exists() && thinking_accepts_clear(body))
        .then(|| rawjson::set_raw(body, "context_management", CONTEXT_MANAGEMENT))
}

/// `reconcileClaudeCodeContextManagement`.
pub(crate) fn reconcile_context_management(body: String, eligible: bool, caller_owned: bool, injected: bool) -> String {
    let current = rawjson::get(&body, "context_management");
    if !thinking_accepts_clear(&body) {
        if caller_owned || !injected || current.json() != CONTEXT_MANAGEMENT {
            return body;
        }
        return rawjson::delete(&body, "context_management");
    }
    if !eligible || caller_owned || current.exists() {
        return body;
    }
    rawjson::set_raw(&body, "context_management", CONTEXT_MANAGEMENT)
}

pub(crate) fn count_cache_controls(body: &str) -> usize {
    // Go counts system, tools, then messages; the total is order-independent.
    signals::blocks(body)
        .iter()
        .filter(|(_, block)| valid_cache_control(&rawjson::get(block, "cache_control")))
        .count()
}

/// `upgradeClaudeCacheControlTTL`: every marker without an explicit ttl joins `ttl`.
pub(crate) fn upgrade_ttl(body: String, ttl: &str) -> String {
    if !rawjson::valid(&body) {
        return body;
    }
    // Go visits the blocks of the payload it was given; each edit touches only its own
    // block, so the remaining blocks read the same before and after the others.
    let mut edits = Vec::new();
    for (path, block) in signals::blocks(&body) {
        let parsed = rawjson::get(&block, "cache_control");
        if parsed.kind() != rawjson::Kind::Object
            || parsed.get("ttl").exists()
            || parsed.get("type").kind() != rawjson::Kind::String
        {
            continue;
        }
        let mut upgraded = format!(
            r#"{{"type":{},"ttl":{}"#,
            js_string(parsed.get("type").str()),
            js_string(ttl)
        );
        let scope = parsed.get("scope");
        if scope.exists() {
            upgraded.push_str(&format!(r#","scope":{}"#, scope.json()));
        }
        upgraded.push('}');
        edits.push((format!("{path}.cache_control"), upgraded));
    }
    edits
        .into_iter()
        .fold(body, |body, (path, upgraded)| rawjson::set_raw(&body, &path, &upgraded))
}

/// `stripClaudeCacheControlTTL`.
pub(crate) fn strip_ttl(body: String) -> String {
    if !rawjson::valid(&body) {
        return body;
    }
    let paths: Vec<String> = signals::blocks(&body)
        .into_iter()
        .filter(|(_, block)| {
            let cc = rawjson::get(block, "cache_control");
            cc.kind() == rawjson::Kind::Object && cc.get("ttl").exists()
        })
        .map(|(path, _)| format!("{path}.cache_control.ttl"))
        .collect();
    paths.iter().fold(body, |body, path| rawjson::delete(&body, path))
}

/// `normalizeCacheControlTTL`: a 1h marker may not follow a 5m one in evaluation order.
pub(crate) fn normalize_ttl(body: String) -> String {
    if !rawjson::valid(&body) {
        return body;
    }
    let mut paths = Vec::new();
    let mut seen_5m = false;
    for (path, block) in signals::blocks(&body) {
        let parsed = rawjson::get(&block, "cache_control");
        if !valid_cache_control(&parsed) {
            continue;
        }
        let ttl = parsed.get("ttl");
        if ttl.kind() != rawjson::Kind::String || ttl.str() != "1h" {
            seen_5m = true;
            continue;
        }
        if seen_5m {
            paths.push(format!("{path}.cache_control.ttl"));
        }
    }
    paths.iter().fold(body, |body, path| rawjson::delete(&body, path))
}

/// `enforceCacheControlLimit`: drop the earliest non-final system/tool markers, then
/// message markers, then any remaining system/tool markers.
///
/// One walk finds every marker; the chosen blocks are rewritten on their own and
/// spliced into one copy of the body (removing a `cache_control` key never moves
/// another block, so their offsets and paths stay valid).
pub(crate) fn enforce_cache_limit(body: String, mut max: usize) -> String {
    if !rawjson::valid(&body) {
        return body;
    }
    let blocks = signals::blocks(&body);
    let marked: Vec<&(String, std::borrow::Cow<'_, str>)> = blocks
        .iter()
        .filter(|(_, block)| valid_cache_control(&rawjson::get(block, "cache_control")))
        .collect();
    let total = marked.len();
    // Anthropic reserves one breakpoint for thread continuation (v8.0.27, 26e5efb). The
    // lookup matters only once the markers reach the limit.
    if total >= max && super::thread::present(&body) {
        max = max.saturating_sub(1);
    }
    if total <= max {
        drop(blocks);
        return body;
    }
    let in_section = |section: &str| -> Vec<_> {
        marked
            .iter()
            .copied()
            .filter(|(path, _)| path.split('.').next() == Some(section))
            .collect()
    };
    let (system, tools, messages) = (in_section("system"), in_section("tools"), in_section("messages"));
    // Go's phases in order: each one is reached only once every earlier candidate went.
    let last = |marked: &[_]| marked.len().saturating_sub(1);
    let (s, t) = (last(&system), last(&tools));
    let order = [&system[..s], &tools[..t], &messages[..], &system[s..], &tools[t..]].concat();
    let mut edits = Vec::new();
    let mut by_path = Vec::new();
    for (path, block) in order.into_iter().take(total - max) {
        match block {
            std::borrow::Cow::Borrowed(raw) => {
                let start = raw.as_ptr() as usize - body.as_ptr() as usize;
                edits.push((start, start + raw.len(), rawjson::delete(raw, "cache_control")));
            }
            std::borrow::Cow::Owned(_) => by_path.push(format!("{path}.cache_control")),
        }
    }
    edits.sort_by_key(|(start, ..)| *start);
    let mut out = String::with_capacity(body.len());
    let mut at = 0;
    for (start, end, block) in &edits {
        out.push_str(&body[at..*start]);
        out.push_str(block);
        at = *end;
    }
    out.push_str(&body[at..]);
    drop(blocks);
    by_path.iter().fold(out, |body, path| rawjson::delete(&body, path))
}

fn cacheable_system(body: &str) -> bool {
    let system = rawjson::get(body, "system");
    match system.kind() {
        rawjson::Kind::Array => last_holder(&system.array(), true).is_some(),
        rawjson::Kind::String => !system.str().trim().is_empty(),
        _ => false,
    }
}

/// Whether a block can carry a breakpoint: "Empty text blocks cannot be cached" and
/// thinking blocks cannot be marked (platform docs, Prompt caching). A system block must
/// be text; a message block may also be a tool use, tool result, image or document.
fn holds_marker(block: &rawjson::Value, system: bool) -> bool {
    if block.kind() != rawjson::Kind::Object {
        return false;
    }
    match block.get("type").str() {
        "text" => !block.get("text").str().trim().is_empty(),
        "" if system => !block.get("text").str().trim().is_empty(),
        "" | "thinking" | "redacted_thinking" => false,
        _ => !system,
    }
}

/// The last block of `blocks` that can carry a breakpoint.
fn last_holder(blocks: &[rawjson::Value], system: bool) -> Option<usize> {
    blocks.iter().rposition(|b| holds_marker(b, system))
}

/// `ensureCacheControl`: CPA-owned breakpoints for non-native callers.
pub(crate) fn ensure_cache_control(mut body: String) -> String {
    if !cacheable_system(&body) {
        body = inject_tools_cache(&body);
    }
    body = inject_system_cache(&body);
    inject_messages_cache(&body)
}

const MARKER: &str = r#"{"type":"ephemeral"}"#;

fn inject_tools_cache(body: &str) -> String {
    let tools = rawjson::get(body, "tools");
    if tools.kind() != rawjson::Kind::Array {
        return body.to_owned();
    }
    let mut last = None;
    for (i, tool) in tools.array().iter().enumerate() {
        if valid_cache_control(&tool.get("cache_control")) {
            return body.to_owned();
        }
        if !tool.get("defer_loading").bool() {
            last = Some(i);
        }
    }
    match last {
        Some(i) => rawjson::set_raw(body, &format!("tools.{i}.cache_control"), MARKER),
        None => body.to_owned(),
    }
}

fn inject_system_cache(body: &str) -> String {
    let system = rawjson::get(body, "system");
    match system.kind() {
        rawjson::Kind::Array => {
            let items = system.array();
            if items.iter().any(|b| valid_cache_control(&b.get("cache_control"))) {
                return body.to_owned();
            }
            match last_holder(&items, true) {
                Some(i) => rawjson::set_raw(body, &format!("system.{i}.cache_control"), MARKER),
                None => body.to_owned(),
            }
        }
        rawjson::Kind::String if !system.str().trim().is_empty() => {
            rawjson::set_raw(body, "system", &format!("[{}]", text_block(system.str(), EPHEMERAL)))
        }
        _ => body.to_owned(),
    }
}

fn eligible_for_rolling_cache(message: &rawjson::Value) -> bool {
    let content = message.get("content");
    if content.kind() == rawjson::Kind::String {
        return !content.str().trim().is_empty();
    }
    let blocks = content.array();
    if content.kind() != rawjson::Kind::Array || last_holder(&blocks, false).is_none() {
        return false;
    }
    if message.get("role").str() != "assistant" {
        return true;
    }
    !matches!(
        blocks.last().map(|b| b.get("type").str().to_owned()).as_deref(),
        Some("thinking" | "redacted_thinking")
    )
}

/// Where a conversation breakpoint goes: `Ok` a block path, `Err` the path of a string
/// turn to wrap in one text block. The final system turn when it is a non-blank string,
/// else the last block that can hold a marker in the last eligible user or assistant turn.
fn messages_target(body: &str) -> Option<Result<String, String>> {
    let messages = rawjson::get(body, "messages");
    if messages.kind() != rawjson::Kind::Array {
        return None;
    }
    let msgs = messages.array();
    let idx = msgs
        .iter()
        .rposition(|m| matches!(m.get("role").str(), "user" | "assistant") && eligible_for_rolling_cache(m))?;
    if let Some(last) = msgs.last()
        && last.get("role").str() == "system"
        && last.get("content").kind() == rawjson::Kind::String
        && !last.get("content").str().trim().is_empty()
    {
        return Some(Err(format!("messages.{}.content", msgs.len() - 1)));
    }
    let content = msgs[idx].get("content");
    match content.kind() {
        rawjson::Kind::String => Some(Err(format!("messages.{idx}.content"))),
        _ => last_holder(&content.array(), false).map(|i| Ok(format!("messages.{idx}.content.{i}"))),
    }
}

/// The string at `path` as one text block carrying `marker`.
fn wrap_with_marker(body: &str, path: &str, marker: &str) -> String {
    let text = js_string(rawjson::get(body, path).str());
    rawjson::set_raw(
        body,
        path,
        &format!(r#"[{{"type":"text","text":{text},"cache_control":{marker}}}]"#),
    )
}

fn inject_messages_cache(body: &str) -> String {
    match messages_target(body) {
        Some(Err(path)) => wrap_with_marker(body, &path, MARKER),
        Some(Ok(block)) => {
            let content = block.rsplit_once('.').map_or("", |(turn, _)| turn);
            let marked = rawjson::get(body, content)
                .array()
                .iter()
                .any(|b| valid_cache_control(&b.get("cache_control")));
            if marked {
                return body.to_owned();
            }
            rawjson::set_raw(body, &format!("{block}.cache_control"), MARKER)
        }
        None => body.to_owned(),
    }
}

/// The last block in the conversation that can hold a breakpoint, in any turn (a
/// trailing system turn included): `Ok` a block path, `Err` a string turn to wrap.
fn last_turn_holder(body: &str) -> Option<Result<String, String>> {
    let messages = rawjson::get(body, "messages");
    if messages.kind() != rawjson::Kind::Array {
        return None;
    }
    messages.array().iter().enumerate().rev().find_map(|(idx, m)| {
        let content = m.get("content");
        match content.kind() {
            rawjson::Kind::String if !content.str().trim().is_empty() => Some(Err(format!("messages.{idx}.content"))),
            rawjson::Kind::Array => {
                last_holder(&content.array(), false).map(|i| Ok(format!("messages.{idx}.content.{i}")))
            }
            _ => None,
        }
    })
}

/// A top-level `cache_control` (automatic caching) placed on the block it would land on,
/// the last one that can hold a breakpoint in messages, else system, else tools, and
/// removed from the top level. Automatic caching takes one of the four slots, and the
/// last block must not carry a marker with another TTL (platform docs, Prompt caching),
/// so a request whose breakpoints CPA owns must count and order it with the others. A
/// marker already on that block stays, unless it is bare and the top-level one is not.
/// With no such block the field stays and the API skips caching.
pub(crate) fn materialize_top_level_cache(body: String) -> String {
    let top = rawjson::get(&body, "cache_control");
    if !valid_cache_control(&top) {
        return body;
    }
    let top = top.json().to_owned();
    let bare = |cc: &rawjson::Value| {
        let mut keys = 0;
        cc.each(|_, _| {
            keys += 1;
            true
        });
        valid_cache_control(cc) && keys == 1
    };
    let apply = |path: String| {
        let existing = rawjson::get(&body, &format!("{path}.cache_control"));
        if !valid_cache_control(&existing) || (bare(&existing) && !bare(&rawjson::parse(&top))) {
            rawjson::set_raw(&body, &format!("{path}.cache_control"), &top)
        } else {
            body.clone()
        }
    };
    let system = rawjson::get(&body, "system");
    let placed = match last_turn_holder(&body) {
        Some(Ok(block)) => apply(block),
        Some(Err(path)) => wrap_with_marker(&body, &path, &top),
        None => match system.kind() {
            rawjson::Kind::Array if last_holder(&system.array(), true).is_some() => apply(format!(
                "system.{}",
                last_holder(&system.array(), true).unwrap_or_default()
            )),
            rawjson::Kind::String if !system.str().trim().is_empty() => wrap_with_marker(&body, "system", &top),
            _ => match rawjson::get(&body, "tools")
                .array()
                .iter()
                .rposition(|t| !t.get("defer_loading").bool())
            {
                Some(i) => apply(format!("tools.{i}")),
                None => return body,
            },
        },
    };
    rawjson::delete(&placed, "cache_control")
}

/// `generateFakeUserIDWithSessionID` for non-CLI-profile cloaking.
pub(crate) fn fake_user_id(session_id: &str) -> String {
    let session = uuid::Uuid::parse_str(session_id)
        .map(|_| session_id.to_owned())
        .unwrap_or_else(|_| super::session::new_v4());
    let mut bytes = [0u8; 32];
    let _ = getrandom::fill(&mut bytes);
    format!(
        r#"{{"device_id":"{}","account_uuid":"","session_id":{}}}"#,
        super::session::hex(&bytes),
        rawjson::go_string(&session)
    )
}

/// Case-insensitive literal matcher, longest words first (`BuildSensitiveWordMatcher`).
pub(crate) struct SensitiveWords(regex::Regex);

impl SensitiveWords {
    pub fn new(words: &[String]) -> Option<Self> {
        let mut valid: Vec<&str> = words
            .iter()
            .map(|w| w.trim())
            .filter(|w| w.chars().count() >= 2 && !w.contains('\u{200B}'))
            .collect();
        if valid.is_empty() {
            return None;
        }
        // Go's sort.Slice is not stable; equal-length order cannot change which
        // text matches because alternatives of equal length cannot both match at
        // one position unless they are equal ignoring case.
        valid.sort_by_key(|w| std::cmp::Reverse(w.len()));
        let pattern = format!(
            "(?i){}",
            valid.iter().map(|w| regex::escape(w)).collect::<Vec<_>>().join("|")
        );
        regex::Regex::new(&pattern).ok().map(Self)
    }

    fn apply(&self, text: &str) -> String {
        self.0
            .replace_all(text, |c: &regex::Captures<'_>| {
                let word = &c[0];
                let mut chars = word.chars();
                match chars.next() {
                    Some(first) if !word.contains('\u{200B}') && first.len_utf8() < word.len() => {
                        format!("{first}\u{200B}{}", chars.as_str())
                    }
                    _ => word.to_owned(),
                }
            })
            .into_owned()
    }

    /// `ObfuscateSensitiveWords`: system (except billing) and message text.
    pub fn obfuscate(&self, body: &str) -> String {
        let mut body = body.to_owned();
        let system = rawjson::get(&body, "system");
        match system.kind() {
            rawjson::Kind::Array => {
                let edits: Vec<(String, String)> = system
                    .array()
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| b.get("type").str() == "text" && !b.get("text").str().starts_with(BILLING_PREFIX))
                    .filter_map(|(i, b)| {
                        let text = b.get("text").str().to_owned();
                        let out = self.apply(&text);
                        (out != text).then(|| (format!("system.{i}.text"), out))
                    })
                    .collect();
                for (path, text) in edits {
                    body = rawjson::set_str(&body, &path, &text);
                }
            }
            rawjson::Kind::String if !system.str().starts_with(BILLING_PREFIX) => {
                let text = system.str().to_owned();
                let out = self.apply(&text);
                if out != text {
                    body = rawjson::set_str(&body, "system", &out);
                }
            }
            _ => {}
        }
        let count = rawjson::get(&body, "messages").items().len();
        for m in 0..count {
            let path = format!("messages.{m}.content");
            let content = rawjson::get(&body, &path).json().to_owned();
            let parsed = rawjson::parse(&content);
            match parsed.kind() {
                rawjson::Kind::String => {
                    let out = self.apply(parsed.str());
                    if out != parsed.str() {
                        body = rawjson::set_str(&body, &path, &out);
                    }
                }
                rawjson::Kind::Array => {
                    for (i, block) in parsed.array().iter().enumerate() {
                        if block.get("type").str() != "text" {
                            continue;
                        }
                        let text = block.get("text").str().to_owned();
                        let out = self.apply(&text);
                        if out != text {
                            body = rawjson::set_str(&body, &format!("{path}.{i}.text"), &out);
                        }
                    }
                }
                _ => {}
            }
        }
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn billing_header_matches_go_capture() {
        let b = Billing {
            signed: true,
            version: "2.1.280",
            message: "Local question",
            entrypoint: "cli",
            workload: "",
            subagent: false,
            prev_req: "",
            prompt_id: "83ec619f-ab81-4d70-9c67-44c3865291b9",
            human_turn: true,
        };
        assert_eq!(
            billing_header(&b),
            "x-anthropic-billing-header: cc_version=2.1.280.b43; cc_entrypoint=cli; cch=00000; cc_prompt_id=83ec619f-ab81-4d70-9c67-44c3865291b9; cc_turn_origin=human;"
        );
    }

    /// The `cc_version` suffix Claude Code 2.1.325 computed for its own requests
    /// (testdata/claude_code_2_1_325.json): it samples the prompt, after the reminder
    /// block the fixture keeps verbatim so that the fingerprint has to skip it.
    #[test]
    fn billing_fingerprint_matches_claude_code_2_1_325_capture() {
        let doc = include_str!("testdata/claude_code_2_1_325.json");
        let mut checked = 0;
        for capture in rawjson::get(doc, "captures").array() {
            let body = capture.get("body");
            let billing = body.get("system.0.text").str().to_owned();
            let Some(version) = billing
                .strip_prefix("x-anthropic-billing-header: cc_version=")
                .and_then(|rest| rest.split(';').next())
            else {
                continue;
            };
            let message = fingerprint_message(body.json());
            assert_eq!(message, "say ok");
            assert_eq!(
                format!("2.1.325.{}", fingerprint(&message, "2.1.325")),
                version,
                "{}",
                capture.get("name").str()
            );
            checked += 1;
        }
        assert_eq!(checked, 9, "every capture but the quota probe");
    }

    /// "Empty text blocks cannot be cached" and thinking blocks cannot carry a marker
    /// (platform docs, Prompt caching): CPA's breakpoints go on the last block that can
    /// hold one, and a section with none gets none.
    #[test]
    fn cpa_breakpoints_skip_blocks_that_cannot_hold_one() {
        let marked = |body: &str| -> Vec<String> {
            super::signals::blocks(body)
                .into_iter()
                .filter(|(_, b)| valid_cache_control(&rawjson::get(b, "cache_control")))
                .map(|(path, _)| path)
                .collect()
        };
        let tools = r#""tools":[{"name":"t","input_schema":{"type":"object"}}]"#;
        // System: the last non-empty text block; an all-empty system leaves the tools
        // prefix to the tools marker.
        let body = format!(
            r#"{{{tools},"system":[{{"type":"text","text":"rules"}},{{"type":"text","text":"  "}}],"messages":[{{"role":"user","content":"hi"}}]}}"#
        );
        assert_eq!(
            marked(&ensure_cache_control(body)),
            ["system.0", "messages.0.content.0"]
        );
        let body = format!(
            r#"{{{tools},"system":[{{"type":"text","text":""}}],"messages":[{{"role":"user","content":"hi"}}]}}"#
        );
        assert_eq!(marked(&ensure_cache_control(body)), ["tools.0", "messages.0.content.0"]);
        // Messages: the last block that can hold a marker, skipping empty text.
        let body =
            r#"{"messages":[{"role":"user","content":[{"type":"text","text":"real"},{"type":"text","text":" \n"}]}]}"#;
        assert_eq!(marked(&ensure_cache_control(body.into())), ["messages.0.content.0"]);
        // A blank string turn is passed over for the previous eligible turn.
        let body = r#"{"messages":[{"role":"user","content":"first"},{"role":"assistant","content":"ok"},{"role":"user","content":"   "}]}"#;
        let out = ensure_cache_control(body.into());
        assert_eq!(marked(&out), ["messages.1.content.0"]);
        assert_eq!(rawjson::get(&out, "messages.2.content").json(), r#""   ""#);
        // A tool-only tail keeps its marker on the tool result.
        let body =
            r#"{"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"x","content":"r"}]}]}"#;
        assert_eq!(marked(&ensure_cache_control(body.into())), ["messages.0.content.0"]);
        // The date reminder does not mark a blank first user text.
        let out = inject_current_date(r#"{"messages":[{"role":"user","content":"  "}]}"#, "2026-11-14", false);
        assert!(marked(&out).is_empty(), "{out}");
        let out = inject_current_date(
            r#"{"messages":[{"role":"user","content":[{"type":"text","text":" "},{"type":"text","text":"q"}]}]}"#,
            "2026-11-14",
            false,
        );
        assert_eq!(marked(&out), ["messages.0.content.2"], "{out}");
    }

    #[test]
    fn top_level_cache_control_lands_on_the_last_cacheable_block() {
        let top = |cc: &str, rest: &str| materialize_top_level_cache(format!(r#"{{"cache_control":{cc},{rest}}}"#));
        let ttl = r#"{"type":"ephemeral","ttl":"1h"}"#;
        // The last block that can hold it, skipping empty text; a string turn is wrapped.
        let out = top(
            ttl,
            r#""messages":[{"role":"user","content":[{"type":"text","text":"q"},{"type":"text","text":""}]}]"#,
        );
        assert_eq!(
            out,
            r#"{"messages":[{"role":"user","content":[{"type":"text","text":"q","cache_control":{"type":"ephemeral","ttl":"1h"}},{"type":"text","text":""}]}]}"#
        );
        let out = top(ttl, r#""messages":[{"role":"user","content":"q"}]"#);
        assert_eq!(
            out,
            r#"{"messages":[{"role":"user","content":[{"type":"text","text":"q","cache_control":{"type":"ephemeral","ttl":"1h"}}]}]}"#
        );
        // A marker already there stays, unless it is bare and the top-level one is not.
        let out = top(
            r#"{"type":"ephemeral"}"#,
            r#""messages":[{"role":"user","content":[{"type":"text","text":"q","cache_control":{"type":"ephemeral","ttl":"1h"}}]}]"#,
        );
        assert_eq!(rawjson::get(&out, "messages.0.content.0.cache_control.ttl").str(), "1h");
        let out = top(
            ttl,
            r#""messages":[{"role":"user","content":[{"type":"text","text":"q","cache_control":{"type":"ephemeral"}}]}]"#,
        );
        assert_eq!(rawjson::get(&out, "messages.0.content.0.cache_control.ttl").str(), "1h");
        // A trailing system turn is the last cacheable block.
        let out = top(
            ttl,
            r#""messages":[{"role":"user","content":"q"},{"role":"system","content":[{"type":"text","text":"now"}]}]"#,
        );
        assert_eq!(
            rawjson::get(&out, "messages.1.content.0.cache_control.ttl").str(),
            "1h",
            "{out}"
        );
        assert!(
            !rawjson::get(&out, "messages.0.content.0.cache_control").exists(),
            "{out}"
        );
        // Without a cacheable turn: system, then the last tool that is not deferred.
        let out = top(ttl, r#""system":"rules","messages":[{"role":"user","content":" "}]"#);
        assert_eq!(rawjson::get(&out, "system.0.cache_control.ttl").str(), "1h");
        assert!(!rawjson::get(&out, "cache_control").exists());
        let out = top(
            ttl,
            r#""tools":[{"name":"a"},{"name":"b","defer_loading":true}],"messages":[{"role":"user","content":" "}]"#,
        );
        assert_eq!(rawjson::get(&out, "tools.0.cache_control.ttl").str(), "1h");
        // Nothing can hold it: the request is left as sent.
        let body = r#"{"cache_control":{"type":"ephemeral"},"messages":[{"role":"user","content":" "}]}"#;
        assert_eq!(materialize_top_level_cache(body.into()), body);
        let body = r#"{"messages":[{"role":"user","content":"q"}]}"#;
        assert_eq!(materialize_top_level_cache(body.into()), body);
    }

    #[test]
    fn cache_control_walk_reads_each_block_in_go_order() {
        // Evaluation order is tools, system, messages; the 1h marker on tools comes
        // first and stays, the system marker is 5m, so the later 1h message marker loses
        // its ttl. Text that mentions cache_control and a string content are not blocks.
        let body = r#"{"messages":[{"role":"user","content":"cache_control"},{"role":"user","content":[{"type":"text","text":"\"cache_control\":{}"},{"type":"text","text":"b","cache_control":{"type":"ephemeral","ttl":"1h"}}]}],"system":[{"type":"text","text":"s","cache_control":{"type":"ephemeral"}}],"tools":[{"name":"t","cache_control":{"type":"ephemeral","ttl":"1h"}}]}"#;
        assert_eq!(count_cache_controls(body), 3);
        assert!(signals::has_1h_ttl(body));
        assert_eq!(
            normalize_ttl(body.to_owned()),
            body.replace(
                r#""text":"b","cache_control":{"type":"ephemeral","ttl":"1h"}"#,
                r#""text":"b","cache_control":{"type":"ephemeral"}"#
            )
        );
        assert_eq!(strip_ttl(body.to_owned()).matches(r#""ttl""#).count(), 0);
        let upgraded = upgrade_ttl(body.to_owned(), "1h");
        assert!(
            upgraded
                .contains(r#""system":[{"type":"text","text":"s","cache_control":{"type":"ephemeral","ttl":"1h"}}]"#)
        );
        assert_eq!(upgraded.matches(r#""ttl":"1h""#).count(), 3);
        let paths: Vec<String> = signals::blocks(body).into_iter().map(|(p, _)| p).collect();
        assert_eq!(
            paths,
            ["tools.0", "system.0", "messages.1.content.0", "messages.1.content.1"]
        );
    }

    #[test]
    fn sensitive_words_split_first_rune() {
        let m = SensitiveWords::new(&["Proxy".into(), "x".into(), "proxy api".into()]).unwrap();
        assert_eq!(
            m.apply("my PROXY API and proxy"),
            "my P\u{200B}ROXY API and p\u{200B}roxy"
        );
    }
}
