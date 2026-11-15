//! The former recordings are retained as input-only contexts. Public client contracts
//! are specified in request_pair_contract, response_pair_contract, stream_state_contract,
//! patch_contract and the focused usage/stop/search/schema suites. Here every valid
//! request context must carry a fresh user turn and a declared tool schema unchanged.
//! No expected value is read from a recorded output or produced by this proxy.
use cpa_core::format::Format;
use cpa_translate::{RequestCtx, translate_request};
use serde_json::{Value, json};

fn input(case: &Value, key: &str) -> Vec<u8> {
    let text = case[key].as_str().unwrap_or_default();
    if case["bytes"].as_bool().unwrap_or(false) {
        text.chars().map(|c| u8::try_from(c as u32).unwrap()).collect()
    } else {
        text.as_bytes().to_vec()
    }
}
fn has_string(value: &Value, want: &str) -> bool {
    match value {
        Value::String(s) => s.contains(want),
        Value::Array(a) => a.iter().any(|v| has_string(v, want)),
        Value::Object(o) => o.values().any(|v| has_string(v, want)),
        _ => false,
    }
}
fn has_probe_schema(value: &Value) -> bool {
    match value {
        Value::Object(o)
            if o.get("properties")
                .is_some_and(|p| p["audit_probe_value"] == json!({"type":"string","enum":["retain-this-enum"]})) =>
        {
            o.get("required") == Some(&json!(["audit_probe_value"]))
                && o.get("additionalProperties") == Some(&json!(false))
        }
        Value::Array(a) => a.iter().any(has_probe_schema),
        Value::Object(o) => o.values().any(has_probe_schema),
        _ => false,
    }
}
fn convert(
    client: Format,
    upstream: Format,
    ctx: &RequestCtx<'_>,
    raw: &[u8],
    compat: bool,
) -> Result<Vec<u8>, cpa_translate::Error> {
    if !compat {
        return translate_request(client, upstream, ctx, raw);
    }
    match (client, upstream) {
        (Format::OpenAI, Format::Claude) => cpa_translate::openai_to_claude_with_compat(ctx, raw),
        (Format::OpenAIResponse, Format::Claude) => cpa_translate::responses_to_claude_with_compat(ctx, raw),
        (Format::Claude, Format::OpenAI) => cpa_translate::claude_to_openai_with_compat(ctx, raw),
        (Format::Claude, Format::Codex) => cpa_translate::claude_to_codex_with_compat(ctx, raw),
        _ => unreachable!(),
    }
}
#[test]
fn every_request_context_carries_a_fresh_user_turn_and_tool_schema() {
    let refusals: Value = serde_json::from_str(include_str!("fixtures/translation_refusals.json")).unwrap();
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/translation_inputs");
    for path in std::fs::read_dir(directory).unwrap() {
        let doc: Value = serde_json::from_slice(&std::fs::read(path.unwrap().path()).unwrap()).unwrap();
        let client = Format::parse(doc["client"].as_str().unwrap()).unwrap();
        let upstream = Format::parse(doc["upstream"].as_str().unwrap()).unwrap();
        for case in doc["cases"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["path"] == "request" || c["path"] == "request_compat")
        {
            let Ok(mut body) = serde_json::from_slice::<Value>(&input(case, "input")) else {
                // Go repair/coercion of malformed JSON and invalid UTF-8 is no
                // provider requirement. Dedicated byte passthrough tests remain.
                continue;
            };
            if !body.is_object() {
                continue;
            }
            let model = case["model"].as_str().unwrap();
            let ctx = RequestCtx {
                model,
                stream: case["stream"].as_bool().unwrap_or(false),
                tool_search: false,
                system_turns: false,
            };
            let compat = case["path"] == "request_compat";
            let original_result = convert(client, upstream, &ctx, &serde_json::to_vec(&body).unwrap(), compat);
            let invalid_control = upstream == Format::Codex
                && client != Format::OpenAIResponse
                && (["max_tokens", "max_completion_tokens"]
                    .iter()
                    .any(|key| body.get(key).is_some_and(|v| !v.is_null() && !v.is_number()))
                    || body
                        .get("parallel_tool_calls")
                        .is_some_and(|v| !v.is_null() && !v.is_boolean())
                    || body["tool_choice"]
                        .get("disable_parallel_tool_use")
                        .is_some_and(|v| !v.is_null() && !v.is_boolean()));
            let pair_name = format!(
                "{}-{}",
                doc["client"].as_str().unwrap(),
                doc["upstream"].as_str().unwrap()
            );
            let named_refusal = refusals["cases"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["pair"] == pair_name && r["name"] == case["name"]);
            if invalid_control || named_refusal.is_some() {
                let error = original_result.expect_err(&format!("{}: explicit schema refusal", case["name"]));
                assert!(error.0.contains(if invalid_control {
                    "invalid request:"
                } else {
                    "unsupported content part:"
                }));
                continue;
            }
            original_result.unwrap_or_else(|error| {
                panic!("{client:?}->{upstream:?} {}: unexpected refusal: {error}", case["name"])
            });
            // This probe explicitly enables the new declaration; restrictive
            // named/allowed/none choices are covered by the choice table.
            body.as_object_mut().unwrap().remove("tool_choice");
            let marker = "audit fresh user turn 🙂";
            let key = if client == Format::OpenAIResponse {
                "input"
            } else {
                "messages"
            };
            if body[key].is_string() {
                body[key] = json!([{"role":"user","content":body[key]}]);
            }
            if !body[key].is_array() {
                body[key] = json!([])
            }
            body[key]
                .as_array_mut()
                .unwrap()
                .push(json!({"role":"user","content":marker}));
            let parameters = json!({"type":"object","properties":{"audit_probe_value":{"type":"string","enum":["retain-this-enum"]}},"required":["audit_probe_value"],"additionalProperties":false});
            let declaration = match client {
                Format::Claude => json!({"name":"audit_probe","input_schema":parameters}),
                Format::OpenAI => json!({"type":"function","function":{"name":"audit_probe","parameters":parameters}}),
                _ => json!({"type":"function","name":"audit_probe","parameters":parameters}),
            };
            if !body["tools"].is_array() {
                body["tools"] = json!([])
            }
            body["tools"].as_array_mut().unwrap().push(declaration);
            let output = convert(client, upstream, &ctx, &serde_json::to_vec(&body).unwrap(), compat).unwrap();
            let output: Value = serde_json::from_slice(&output).unwrap();
            assert!(
                has_string(&output, marker),
                "{client:?}->{upstream:?} {}: user turn disappeared",
                case["name"]
            );
            assert!(
                has_probe_schema(&output),
                "{client:?}->{upstream:?} {}: declared tool constraints disappeared",
                case["name"]
            );
            if !(client == Format::OpenAIResponse && upstream == Format::Codex) {
                assert_eq!(output["model"], model, "{}", case["name"]);
            }
        }
    }
}

fn body_text(value: &Value, format: Format) -> String {
    match format {
        Format::Claude => value["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect(),
        Format::OpenAI => value["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        _ => {
            let response = value.get("response").unwrap_or(value);
            response["output"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|i| i["type"] == "message")
                .flat_map(|i| i["content"].as_array().into_iter().flatten())
                .filter(|p| p["type"] == "output_text")
                .filter_map(|p| p["text"].as_str())
                .collect()
        }
    }
}
fn delta_text(value: &Value, format: Format) -> &str {
    match format {
        Format::Claude => value["delta"]["text"].as_str().unwrap_or_default(),
        Format::OpenAI => value["choices"][0]["delta"]["content"].as_str().unwrap_or_default(),
        _ if value["type"] == "response.output_text.delta" => value["delta"].as_str().unwrap_or_default(),
        _ => "",
    }
}
#[test]
fn response_contexts_keep_public_text_fields_and_accept_no_recorded_outputs() {
    use cpa_translate::{ResponseCtx, go_stream, pair};
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/translation_inputs");
    for path in std::fs::read_dir(directory).unwrap() {
        let doc: Value = serde_json::from_slice(&std::fs::read(path.unwrap().path()).unwrap()).unwrap();
        let client = Format::parse(doc["client"].as_str().unwrap()).unwrap();
        let upstream = Format::parse(doc["upstream"].as_str().unwrap()).unwrap();
        for case in doc["cases"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["path"] == "non_stream" || c["path"] == "stream" || c["path"] == "token_count")
        {
            let original = input(case, "original");
            let translated = input(case, "translated");
            let raw = input(case, "input");
            let ctx = ResponseCtx {
                model: case["model"].as_str().unwrap(),
                original_request: &original,
                translated_request: &translated,
                tool_search: false,
            };
            if case["path"] == "token_count" {
                let count = case["count"].as_i64().unwrap_or_default();
                let out = cpa_translate::translate_token_count(client, upstream, count, &raw);
                let out: Value = serde_json::from_slice(&out).unwrap();
                assert_eq!(out["input_tokens"], count);
                continue;
            }
            if case["path"] == "non_stream" {
                match (pair(client, upstream).unwrap().non_stream)(&ctx, &raw) {
                    Ok(out) => {
                        if let Ok(source) = serde_json::from_slice::<Value>(&raw) {
                            let text = body_text(&source, upstream);
                            if !text.is_empty() {
                                let out: Value = serde_json::from_slice(&out).unwrap();
                                assert!(
                                    body_text(&out, client).contains(&text),
                                    "{client:?}<-{upstream:?} {}: lost text {text:?}: {out}",
                                    case["name"]
                                );
                            }
                        }
                    }
                    Err(error) => assert_eq!(error.0, cpa_translate::APPLY_PATCH_UPSTREAM_ERROR, "{}", case["name"]),
                }
                continue;
            }
            let mut state = go_stream(client, upstream).unwrap()(&ctx);
            let mut source_text = String::new();
            let mut target_text = String::new();
            let mut buffered_text = String::new();
            let mut source_terminal = false;
            let mut source_complete = false;
            for line in case["lines"].as_array().unwrap() {
                let raw = input(&json!({"bytes":case["bytes"],"line":line}), "line");
                let payload = raw.strip_prefix(b"data:").unwrap_or(&raw).trim_ascii();
                if payload == b"[DONE]" {
                    source_terminal = true;
                }
                if !source_terminal
                    && raw.starts_with(b"data:")
                    && let Ok(value) = serde_json::from_slice::<Value>(payload)
                {
                    source_text.push_str(delta_text(&value, upstream));
                    source_complete |= matches!(
                        value["type"].as_str(),
                        Some("message_stop" | "response.completed" | "response.done" | "response.incomplete")
                    ) || value["choices"]
                        .as_array()
                        .is_some_and(|a| a.iter().any(|c| c["finish_reason"].is_string()));
                }
                for out in state.line(&raw).unwrap() {
                    for line in out.split(|&b| b == b'\n') {
                        let data = line.strip_prefix(b"data:").unwrap_or(line).trim_ascii();
                        if let Ok(value) = serde_json::from_slice::<Value>(data) {
                            target_text.push_str(delta_text(&value, client));
                            let text = body_text(&value, client);
                            if !text.is_empty() {
                                buffered_text = text;
                            }
                        }
                    }
                }
            }
            // A buffered Chat adapter publishes only after the transport sentinel.
            // Complete the transport for an input corpus that ended at finish_reason;
            // truncated lifecycle behavior is specified separately in state tests.
            if upstream == Format::OpenAI && source_complete && !source_terminal {
                for out in state.line(b"data: [DONE]").unwrap() {
                    for line in out.split(|&b| b == b'\n') {
                        let data = line.strip_prefix(b"data:").unwrap_or(line).trim_ascii();
                        if let Ok(value) = serde_json::from_slice::<Value>(data) {
                            target_text.push_str(delta_text(&value, client));
                            let text = body_text(&value, client);
                            if !text.is_empty() {
                                buffered_text = text;
                            }
                        }
                    }
                }
            }
            // Full lifecycle/terminal/call assertions live in the typed state tests.
            // These contexts also retain a direct field-preservation assertion when
            // they exercise an ordinary incremental text stream.
            if target_text.is_empty() {
                target_text = buffered_text;
            }
            // Historical Chat->Claude calls also fed delta chunks to a client
            // explicitly requesting a buffered Chat response. Those inputs are
            // schema-mismatched, not a requirement to reinterpret delta as message.
            let delta_mode = !(client == Format::Claude && upstream == Format::OpenAI)
                || serde_json::from_slice::<Value>(&original).is_ok_and(|r| r["stream"] == true);
            if delta_mode && source_complete && !source_text.is_empty() {
                assert!(
                    target_text.contains(&source_text),
                    "{client:?}<-{upstream:?} {}: {source_text:?} became {target_text:?}",
                    case["name"]
                );
            }
        }
    }
}
