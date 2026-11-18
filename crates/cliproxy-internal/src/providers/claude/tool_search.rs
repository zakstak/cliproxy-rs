//! Codex client `tool_search` on Claude Responses routes (`upstream.claude.tool-search`):
//! Go helps/responses_tool_search.go, helps/claude_tool_search_model.go and the Claude
//! executor's `applyNativeToolSearchFallback`.

use crate::protocol::json::{self as gj, Res};
use crate::text::GoStr;
use bytes::Bytes;

fn kind<'a>(item: &Res<'a>) -> String {
    item.get("type").str().trim().to_owned()
}

fn is_client_declaration(tool: &Res<'_>) -> bool {
    kind(tool) == "tool_search" && tool.get("execution").str().trim() == "client"
}

fn has_client_declaration(tools: &Res<'_>) -> bool {
    let mut found = false;
    tools.each(|_, tool| {
        found = is_client_declaration(&tool);
        !found
    });
    found
}

/// `withoutClientToolSearch`: the array without client tool_search declarations, when
/// it had any.
fn without_client_declarations(tools: &Res<'_>) -> Option<Vec<u8>> {
    if !tools.is_array() {
        return None;
    }
    let items = tools.array();
    let kept: Vec<&[u8]> = items
        .iter()
        .filter(|tool| !is_client_declaration(tool))
        .map(Res::raw)
        .collect();
    (kept.len() != items.len()).then(|| gj::join(&kept))
}

/// `ResponsesHasToolSearch`: a client tool_search declaration (top level or in an
/// `additional_tools` item), or a tool_search_call / tool_search_output input item.
pub(crate) fn has(payload: &[u8]) -> bool {
    if has_client_declaration(&gj::get(payload, "tools")) {
        return true;
    }
    let mut found = false;
    gj::get(payload, "input").each(|_, item| {
        match kind(&item).as_str() {
            "tool_search_call" | "tool_search_output" => found = true,
            "additional_tools" => found = has_client_declaration(&item.get("tools")),
            _ => {}
        }
        !found
    });
    found
}

/// `StripResponsesToolSearch`: the request as if the client had never used tool_search.
pub(crate) fn strip(payload: &[u8]) -> Vec<u8> {
    let mut out = payload.to_vec();
    if let Some(kept) = without_client_declarations(&gj::get(&out, "tools")) {
        gj::set_raw(&mut out, "tools", kept);
    }
    let input = gj::get(&out, "input");
    if !input.is_array() {
        return out;
    }
    let mut changed = false;
    let mut items: Vec<Vec<u8>> = vec![];
    for item in input.array() {
        match kind(&item).as_str() {
            "tool_search_call" | "tool_search_output" => {
                changed = true;
                continue;
            }
            "additional_tools" => {
                if let Some(kept) = without_client_declarations(&item.get("tools"))
                    && let Ok(updated) = gj::try_set_raw(item.raw(), "tools", kept)
                {
                    items.push(updated);
                    changed = true;
                    continue;
                }
            }
            _ => {}
        }
        items.push(item.raw().to_vec());
    }
    if changed {
        gj::set_raw(&mut out, "input", gj::join(&items));
    }
    out
}

/// `ClaudeModelLacksToolSearch`: models that predate Anthropic tool search.
pub(crate) fn model_lacks(model: &str) -> bool {
    let model = crate::protocol::thinking::parse_suffix(model)
        .model_name
        .trim()
        .go_lower();
    let model = model.rsplit_once('/').map_or(model.as_str(), |(_, last)| last);
    let model = model.split_once('[').map_or(model, |(head, _)| head).trim();
    if model.is_empty() {
        return false;
    }
    matches!(
        model,
        "claude-opus-4"
            | "claude-opus-4-0"
            | "claude-opus-4-20250514"
            | "claude-opus-4-1"
            | "claude-opus-4-1-20250805"
            | "claude-sonnet-4"
            | "claude-sonnet-4-0"
            | "claude-sonnet-4-20250514"
    ) || ["claude-3", "claude-2", "claude-instant"]
        .iter()
        .any(|prefix| model.starts_with(prefix))
}

/// The body without tool_search, or `None` when it needs no change.
pub(crate) fn stripped(body: &Bytes) -> Option<Bytes> {
    let out = strip(body);
    (out != body[..]).then(|| Bytes::from(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fallback removes client search declarations and their transcript, while
    /// ordinary tools and caller messages keep their meaning.
    #[test]
    fn fallback_removes_client_search_and_keeps_ordinary_tools_and_messages() {
        for (payload, want) in [
            (
                r#"{"tools":[{"type":"tool_search","execution":"client"}],"input":"hi"}"#,
                true,
            ),
            (
                r#"{"input":[{"type":"additional_tools","tools":[{"type":"tool_search","execution":" client "}]}]}"#,
                true,
            ),
            (
                r#"{"input":[{"type":"tool_search_call","call_id":"c1","execution":"client","arguments":{}}]}"#,
                true,
            ),
            (
                r#"{"input":[{"type":"tool_search_output","call_id":"c1","tools":[]}]}"#,
                true,
            ),
            (r#"{"tools":[{"type":"tool_search"}],"input":"hi"}"#, false),
            (
                r#"{"tools":[{"type":"function","name":"a"}],"input":[{"type":"message","role":"user","content":"hi"}]}"#,
                false,
            ),
        ] {
            assert_eq!(has(payload.as_bytes()), want, "{payload}");
        }
        for (payload, tools, input) in [
            (
                r#"{"tools":[{"type":"function","name":"a"},{"type":"tool_search","execution":"client"}],"input":"hi"}"#,
                r#"[{"type":"function","name":"a"}]"#,
                r#""hi""#,
            ),
            (
                r#"{"tools":[{"type":"tool_search","execution":"client"}],"input":"hi"}"#,
                "[]",
                r#""hi""#,
            ),
            (
                r#"{"tools":[{"type":"tool_search","name":"ToolSearch"},{"type":"tool_search","execution":"client"}],"input":"hi"}"#,
                r#"[{"type":"tool_search","name":"ToolSearch"}]"#,
                r#""hi""#,
            ),
            (
                r#"{"input":[{"type":"additional_tools","tools":[{"type":"tool_search","execution":"client"},{"type":"function","name":"b"}]},{"type":"message","role":"user","content":"hi"},{"type":"tool_search_call","call_id":"c1","execution":"client","arguments":{"query":"x"}},{"type":"tool_search_output","call_id":"c1","execution":"client","tools":[]}]}"#,
                "",
                r#"[{"type":"additional_tools","tools":[{"type":"function","name":"b"}]},{"type":"message","role":"user","content":"hi"}]"#,
            ),
        ] {
            let got = strip(payload.as_bytes());
            if !tools.is_empty() {
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(gj::get(&got, "tools").raw()).unwrap(),
                    serde_json::from_str::<serde_json::Value>(tools).unwrap(),
                    "{payload}"
                );
            }
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(gj::get(&got, "input").raw()).unwrap(),
                serde_json::from_str::<serde_json::Value>(input).unwrap(),
                "{payload}"
            );
            assert!(!has(&got), "fallback retained client search: {payload}");
            assert_eq!(strip(&got), got, "fallback is idempotent");
        }
        let unchanged =
            Bytes::from_static(br#"{"tools":[{"type":"function","name":"a"},{"type":"tool_search"}],"input":[{"type":"message","role":"user","content":"hi"}]}"#);
        assert!(stripped(&unchanged).is_none());
        let preserved = br#"{ "opaque": 9007199254740993, "tools":[{"type":"function","name":"a"},{"type":"tool_search","execution":"client"}], "input":"hello" }"#;
        let edited = strip(preserved);
        assert!(String::from_utf8_lossy(&edited).contains(r#""opaque": 9007199254740993"#));
        assert_eq!(gj::get(&edited, "input").raw(), br#""hello""#);
    }

    /// Supported native-search model policy, including routed model IDs.
    #[test]
    fn legacy_models_lack_tool_search() {
        for model in [
            "claude-opus-4-1-20250805",
            "anthropic/claude-3-7-sonnet-20250219",
            "claude-sonnet-4-20250514(high)",
            "claude-sonnet-4[1m]",
            "CLAUDE-OPUS-4",
            "team/claude-opus-4-1",
            "claude-opus-4-0",
            "claude-sonnet-4-0",
            "claude-3-5-haiku-20241022",
            "claude-2.1",
            "claude-instant-1.2",
        ] {
            assert!(model_lacks(model), "{model}");
        }
        for model in [
            "claude-sonnet-4-5",
            "claude-sonnet-4-5-20250929",
            "claude-haiku-4-5-20251001",
            "claude-opus-4-5-20251101",
            "claude-opus-4-6",
            "claude-fable-5-1",
            "glm-4.6",
            "",
        ] {
            assert!(!model_lacks(model), "{model}");
        }
    }
}
