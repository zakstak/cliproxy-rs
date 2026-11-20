//! Integration contracts for supported client translations. The matrix uses
//! hand-authored message and tool histories, independent of recorded Go output.
//! Pure multi-agent and orphan rewrites have exhaustive cpa-common tests.

use super::*;
use serde_json::{Value, json};

fn contains_string(value: &Value, expected: &str) -> bool {
    match value {
        Value::String(s) => s == expected,
        Value::Array(items) => items.iter().any(|v| contains_string(v, expected)),
        Value::Object(object) => object.values().any(|v| contains_string(v, expected)),
        _ => false,
    }
}

fn tool_schema(value: &Value) -> Option<&Value> {
    match value {
        Value::Object(object) => {
            if object.get("name").and_then(Value::as_str) == Some("exec_command")
                && let Some(schema) = object.get("parameters").or_else(|| object.get("input_schema"))
            {
                return Some(schema);
            }
            object.values().find_map(tool_schema)
        }
        Value::Array(items) => items.iter().find_map(tool_schema),
        _ => None,
    }
}

fn history(format: Format) -> Value {
    let schema =
        json!({"type":"object","properties":{"max_output_tokens":{"type":"number"},"custom":{"type":"number"}}});
    match format {
        Format::OpenAI => json!({
            "model":"client-model","messages":[
                {"role":"user","content":"question-contract"},
                {"role":"assistant","content":"answer-contract","tool_calls":[{"id":"call-contract","type":"function","function":{"name":"exec_command","arguments":"{\"command\":\"echo hello\"}"}}]},
                {"role":"tool","tool_call_id":"call-contract","content":"result-contract"},
                {"role":"user","content":"followup-contract"}
            ],"tools":[{"type":"function","function":{"name":"exec_command","description":"tool-contract","parameters":schema}}]
        }),
        Format::Claude => json!({
            "model":"client-model","max_tokens":128,"messages":[
                {"role":"user","content":"question-contract"},
                {"role":"assistant","content":[{"type":"text","text":"answer-contract"},{"type":"tool_use","id":"call-contract","name":"exec_command","input":{"command":"echo hello"}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"call-contract","content":"result-contract"},{"type":"text","text":"followup-contract"}]}
            ],"tools":[{"name":"exec_command","description":"tool-contract","input_schema":schema}]
        }),
        Format::OpenAIResponse => json!({
            "model":"client-model","input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"question-contract"}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer-contract"}]},
                {"type":"function_call","call_id":"call-contract","name":"exec_command","arguments":"{\"command\":\"echo hello\"}"},
                {"type":"function_call_output","call_id":"call-contract","output":"result-contract"},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"followup-contract"}]}
            ],"tools":[{"type":"function","name":"exec_command","description":"tool-contract","parameters":schema}]
        }),
        Format::Codex => unreachable!(),
    }
}

#[test]
fn supported_translation_pairs_preserve_multiturn_text_tool_identity_and_targeted_schema_types() {
    for (from, to) in [
        (Format::OpenAIResponse, Format::Claude),
        (Format::OpenAIResponse, Format::Codex),
        (Format::OpenAIResponse, Format::OpenAI),
        (Format::Claude, Format::Codex),
        (Format::Claude, Format::OpenAI),
        (Format::OpenAI, Format::Claude),
        (Format::OpenAI, Format::Codex),
    ] {
        let input = history(from).to_string();
        for user_agent in ["codex_cli_rs/0.1.0", "curl/8"] {
            for target in ["", "codex", "codex-websockets", "codex_websockets"] {
                for compat in [false, true] {
                    let mut headers = HeaderMap::new();
                    headers.insert(http::header::USER_AGENT, user_agent.parse().unwrap());
                    headers.insert("x-openai-subagent", "collab_spawn".parse().unwrap());
                    let client = Client {
                        headers: &headers,
                        settings: Settings {
                            optimize_multi_agent_v2: true,
                            orphan_delegation: true,
                        },
                        target_executor: target,
                        is_compat: compat,
                    };
                    let ctx = RequestCtx {
                        registry: None,
                        model: "routed-model",
                        stream: true,
                        tool_search: false,
                        system_turns: false,
                    };
                    let output = translate_request(from, to, &ctx, input.as_bytes(), &client).unwrap();
                    let output: Value = serde_json::from_slice(&output).unwrap();
                    // Responses conversion keeps the supplied model; the executor owns routing.
                    let model = if from == Format::OpenAIResponse && to == Format::Codex {
                        "client-model"
                    } else {
                        "routed-model"
                    };
                    assert_eq!(output["model"], model, "{from:?}->{to:?}");
                    assert_eq!(output["stream"], true, "{from:?}->{to:?}");
                    for text in [
                        "question-contract",
                        "answer-contract",
                        "result-contract",
                        "followup-contract",
                        "call-contract",
                        "tool-contract",
                    ] {
                        assert!(contains_string(&output, text), "{from:?}->{to:?} lost {text}: {output}");
                    }
                    let schema = tool_schema(&output)
                        .unwrap_or_else(|| panic!("{from:?}->{to:?}: tool schema missing: {output}"));
                    let expected = if user_agent.starts_with("codex") && target.is_empty() {
                        "integer"
                    } else {
                        "number"
                    };
                    assert_eq!(
                        schema["properties"]["max_output_tokens"]["type"], expected,
                        "{from:?}->{to:?} {user_agent} {target} {compat}"
                    );
                    assert_eq!(
                        schema["properties"]["custom"]["type"], "number",
                        "unrecognized fields are not normalized"
                    );
                }
            }
        }
    }
}

/// Orphan delegation is a shared `upstream.codex` setting (Go 52d5507d): API keys
/// read it from every spelling, the historical `oauth.providers.codex` one included.
#[test]
fn orphan_delegation_is_shared_with_api_keys() {
    for yaml in [
        "upstream:\n  codex:\n    orphan-delegation-compatibility: true\n",
        "oauth:\n  providers:\n    codex:\n      orphan-delegation-compatibility: true\n",
        "codex:\n  orphan-delegation-compatibility: true\n",
    ] {
        let cfg = Config::parse(&format!("client:\n  codex:\n    optimize-multi-agent-v2: true\n{yaml}")).unwrap();
        assert!(Settings::from_config(&cfg).orphan_delegation, "{yaml}");
        let api_key = Settings::from_config(&cfg.for_api_key());
        assert!(api_key.orphan_delegation && api_key.optimize_multi_agent_v2, "{yaml}");
    }
}
