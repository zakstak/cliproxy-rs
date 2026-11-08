//! Replays `tests/fixtures/go_helpers.json` against the Rust ports of Go's translator
//! helpers. Each record is a call one of Go's own tests made to a helper, with the
//! arguments and results Go saw (recorded by `tests/reference/harvest` in value mode; see
//! tests/reference/README.md). Arguments arrive tagged: `s` string, `sb`/`b` base64 bytes
//! (`b` also carries Go's nil), `json` a gjson.Result (`jb` its raw bytes), `bool`, `err`
//! an error, `null` a nil interface.

use base64::Engine;
use cpa_common::json as gj;
use serde_json::Value;

const ERR: &[u8] = b"<error>";

fn bytes(v: &Value) -> Vec<u8> {
    let b64 = |s: &Value| {
        base64::engine::general_purpose::STANDARD
            .decode(s.as_str().unwrap())
            .unwrap()
    };
    if let Some(s) = v.get("s") {
        s.as_str().unwrap().as_bytes().to_vec()
    } else if let Some(s) = v.get("sb").or_else(|| v.get("b")).or_else(|| v.get("jb")) {
        b64(s)
    } else {
        panic!("not a byte value: {v}")
    }
}

fn list(v: &Value) -> Vec<Vec<u8>> {
    v["list"].as_array().unwrap().iter().map(bytes).collect()
}

/// A list of byte strings, length-prefixed so item boundaries count. Go's nil and empty
/// lists compare equal.
fn list_out(items: &[Vec<u8>]) -> Vec<u8> {
    items
        .iter()
        .flat_map(|i| [format!("{}:", i.len()).into_bytes(), i.clone(), b"\n".to_vec()])
        .collect::<Vec<_>>()
        .concat()
}

fn flag(v: &Value) -> Vec<u8> {
    bool_out(v["bool"].as_bool().unwrap())
}

fn bool_out(b: bool) -> Vec<u8> {
    if b { b"true".to_vec() } else { b"false".to_vec() }
}

fn failed(v: &Value) -> bool {
    v.get("err").is_some()
}

/// Recorded helpers of the translators this fork removed (Gemini, Interactions, Antigravity
/// and Devin; issue #49). Their calls stay in the Go fixture and are skipped.
const REMOVED: &[&str] = &[
    "ContainsJSONRef",
    "AntigravityToolNameToUpstream",
    "AntigravityUpstreamToolNameToClient",
    "ObfuscateExecCommandDescription",
    "ObfuscateWriteStdinDescription",
    "IsDevinCodexAppAutomationUpdate",
    "SanitizeDevinToolDescription",
    "GeminiClaudeToolUseID",
    "fixCLIToolResponse",
    "removeEmptyGeminiFunctionTools",
    "rewriteGeminiFunctionNames",
    "rewriteInteractionsFunctionNames",
    "normalizeAntigravityOpenAIThinkingConfig",
    "normalizeClaudeToolSchema",
    "lowercaseClaudeToolSchemaTypes",
    "cleanGeminiCodexToolParameters",
    "cleanedCodexToolParameters",
    "setInteractionsCodexRawIfDifferent",
    "MergeAdjacentGeminiContents",
    "ContentHasGeminiFunctionResponse",
    "ReorderGeminiUserParts",
    "MergeAdjacentGeminiUserContents",
    "SplitGeminiFunctionResponseTurns",
    "SetGeminiFunctionResponseResult",
    "restoreUsageMetadata",
    "CleanClaudeToolArgumentsWithTranslated",
    "NormalizeOpenAIFileData",
];

/// (got, want) for one recorded call; `None` for a function without a replay arm.
fn replay(fun: &str, a: &[Value], r: &[Value]) -> Option<(Vec<u8>, Vec<u8>)> {
    let arg = |i: usize| bytes(&a[i]);
    let want = || bytes(&r[0]);
    Some(match fun {
        "IsCustomTool" => (
            bool_out(crate::apply_patch::is_custom_tool(&gj::parse(&arg(0)))),
            flag(&r[0]),
        ),
        "Description" => (crate::apply_patch::description(&gj::parse(&arg(0))), want()),
        "WrapInput" => (crate::apply_patch::wrap_input(&arg(0)), want()),
        "UnwrapInput" => (
            crate::apply_patch::unwrap_input(&arg(0)).map_or(ERR.to_vec(), String::into_bytes),
            if failed(&r[1]) { ERR.to_vec() } else { want() },
        ),
        "EscapeInputFragment" => (crate::apply_patch::escape_input_fragment(&arg(0)), want()),
        "Parameters" => (crate::apply_patch::PARAMETERS.as_bytes().to_vec(), want()),
        "DeriveClaudeUserID" => (crate::common::derive_claude_user_id(&arg(0)), want()),
        "AttachCacheControl" => (crate::common::attach_cache_control(arg(0), &gj::parse(&arg(1))), want()),
        "AttachMessageCacheControl" => (
            crate::common::attach_message_cache_control(arg(0), &gj::parse(&arg(1))),
            want(),
        ),
        "AttachToolMessageCacheControl" => (
            crate::common::attach_tool_message_cache_control(arg(0), &gj::parse(&arg(1))),
            want(),
        ),
        "SystemReminderText" => (crate::common::system_reminder_text(&arg(0)), want()),
        "RequestModelName" => (crate::common::request_model_name(&arg(0), &arg(1)), want()),
        "BuildClaudeStructuredOutputInstruction" => (
            crate::common::claude_structured_output_instruction(&gj::parse(&arg(0))),
            want(),
        ),
        "NormalizeClaudeToolInputSchema" => {
            let schema = arg(0);
            let nil = a[0]["nil"].as_bool().unwrap_or(false);
            (
                crate::common::normalize_claude_tool_input_schema((!nil).then_some(&schema[..])),
                want(),
            )
        }
        "HasUnsupportedUnicodePropertyEscape" => (
            bool_out(crate::common::has_unsupported_unicode_property_escape(&arg(0))),
            flag(&r[0]),
        ),
        "IsClaudeCodeAttributionSystemText" => (
            bool_out(crate::common::is_claude_code_attribution_text(&arg(0))),
            flag(&r[0]),
        ),
        "AlignOpenAIToolCallMessages" => (
            list_out(&crate::common::align_openai_tool_call_messages_with(
                list(&a[0]),
                &list(&a[1]),
            )),
            list_out(&list(&r[0])),
        ),
        "normalizeToolParameters" => (crate::codex_claude::normalize_tool_parameters(&arg(0)), want()),
        _ => return None,
    })
}

#[test]
fn go_helper_calls_match() {
    let doc: Value = serde_json::from_str(include_str!("../tests/fixtures/go_helpers.json")).unwrap();
    let calls = doc["calls"].as_array().unwrap();
    assert!(calls.len() > 200, "fixture holds {} calls", calls.len());
    let mut failures = Vec::new();
    for call in calls {
        let name = call["name"].as_str().unwrap();
        let fun = call["fn"].as_str().unwrap().rsplit('.').next().unwrap();
        if REMOVED.contains(&fun) {
            continue;
        }
        let (args, results) = (call["args"].as_array().unwrap(), call["results"].as_array().unwrap());
        match replay(fun, args, results) {
            Some((got, want)) if got != want => failures.push(format!(
                "{name} {fun}:\n   got {}\n  want {}",
                String::from_utf8_lossy(&got),
                String::from_utf8_lossy(&want)
            )),
            Some(_) => {}
            None => failures.push(format!("{name}: no replay arm for {fun}")),
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} calls differ:\n{}",
        failures.len(),
        calls.len(),
        failures.join("\n")
    );
}
