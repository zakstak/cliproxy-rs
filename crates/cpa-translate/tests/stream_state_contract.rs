//! Request-local streaming state follows the public event schemas. Assertions describe
//! client-visible tool identity, finality and content, independently of recorded chunks.
//! Codex event provenance: docs/codex/CONTRACT.md and the pinned first-party
//! codex-rs/codex-api/src/sse/responses.rs parser and ResponseItem protocol types.
use cpa_core::format::Format;
use cpa_translate::{ResponseCtx, pair};
use serde_json::{Value, json};

fn run(client: Format, upstream: Format, request: &Value, events: Vec<Value>, done: bool) -> Vec<Value> {
    let request = serde_json::to_vec(request).unwrap();
    let ctx = ResponseCtx {
        model: "chosen",
        original_request: &request,
        translated_request: &request,
        tool_search: false,
    };
    let mut state = (pair(client, upstream).unwrap().stream)(&ctx);
    let mut frames = vec![];
    for event in events {
        frames.extend(state.event(format!("data: {event}\n\n").as_bytes()).unwrap());
    }
    if done {
        frames.extend(state.event(b"data: [DONE]\n\n").unwrap());
    }
    frames.extend(state.finish().unwrap());
    frames
        .iter()
        .flat_map(|f| f.split(|&b| b == b'\n'))
        .filter_map(|l| l.strip_prefix(b"data: "))
        .filter_map(|d| serde_json::from_slice(d).ok())
        .collect()
}
fn chat(delta: Value, finish: Value) -> Value {
    json!({"id":"message-1","model":"chosen","created":1700000000,"choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
}
fn request() -> Value {
    json!({"model":"chosen","stream":true,"input":"question","tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}}]})
}
fn terminal(calls: Value) -> Value {
    json!({"type":"response.completed","response":{"id":"response-1","status":"completed","output":calls,"usage":{"input_tokens":3,"output_tokens":5,"total_tokens":8}}})
}

#[test]
fn chat_tools_bind_late_names_ids_and_split_arguments_before_completion() {
    for late_name in [false, true] {
        for late_id in [false, true] {
            let mut initial = json!({"index":0,"type":"function","function":{"arguments":r#"{"path":"#}});
            if !late_name {
                initial["function"]["name"] = json!("lookup");
            }
            if !late_id {
                initial["id"] = json!("call-1");
            }
            let mut last = json!({"index":0,"function":{"arguments":r#""a"}"#}});
            if late_name {
                last["function"]["name"] = json!("lookup");
            }
            if late_id {
                last["id"] = json!("call-1");
            }
            let out = run(
                Format::OpenAIResponse,
                Format::OpenAI,
                &request(),
                vec![
                    chat(json!({"role":"assistant","tool_calls":[initial]}), Value::Null),
                    chat(json!({"tool_calls":[last]}), Value::Null),
                    chat(json!({}), json!("tool_calls")),
                ],
                true,
            );
            let completed = out.iter().find(|e| e["type"] == "response.completed").unwrap();
            let calls: Vec<_> = completed["response"]["output"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|i| i["type"] == "function_call")
                .collect();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0]["call_id"], "call-1");
            assert_eq!(calls[0]["name"], "lookup");
            assert_eq!(
                serde_json::from_str::<Value>(calls[0]["arguments"].as_str().unwrap()).unwrap(),
                json!({"path":"a"})
            );
            let done = out
                .iter()
                .find(|e| e["type"] == "response.output_item.done" && e["item"]["type"] == "function_call")
                .unwrap();
            assert_eq!(done["item"], *calls[0]);
        }
    }
}

#[test]
fn chat_choices_with_the_same_local_call_index_keep_distinct_output_slots() {
    let out = run(
        Format::OpenAIResponse,
        Format::OpenAI,
        &request(),
        vec![
            json!({"id":"message-1","choices":[
                {"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-a","function":{"name":"lookup","arguments":"{}"}}]},"finish_reason":null},
                {"index":1,"delta":{"tool_calls":[{"index":0,"id":"call-b","function":{"name":"lookup","arguments":"{}"}}]},"finish_reason":null}
            ]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"},{"index":1,"delta":{},"finish_reason":"tool_calls"}]}),
        ],
        true,
    );
    let done: Vec<_> = out
        .iter()
        .filter(|e| e["type"] == "response.output_item.done")
        .collect();
    assert_eq!(done.len(), 2);
    assert_ne!(done[0]["output_index"], done[1]["output_index"]);
    let completed = out.iter().find(|e| e["type"] == "response.completed").unwrap();
    let calls = completed["response"]["output"].as_array().unwrap();
    assert_eq!(
        calls.iter().map(|c| c["call_id"].as_str().unwrap()).collect::<Vec<_>>(),
        ["call-a", "call-b"]
    );
}

#[test]
fn unfinished_chat_streams_never_claim_success_and_limit_finishes_stay_incomplete() {
    for done in [false, true] {
        for delta in [
            json!({"reasoning_content":"thinking"}),
            json!({"tool_calls":[{"index":0,"id":"c","function":{"name":"lookup","arguments":"{"}}]}),
        ] {
            let out = run(
                Format::OpenAIResponse,
                Format::OpenAI,
                &request(),
                vec![chat(delta, Value::Null)],
                done,
            );
            assert!(!out.iter().any(|e| e["type"] == "response.completed"));
        }
    }
    for (finish, reason) in [("length", "max_output_tokens"), ("content_filter", "content_filter")] {
        let out = run(
            Format::OpenAIResponse,
            Format::OpenAI,
            &request(),
            vec![
                chat(json!({"content":"partial"}), Value::Null),
                chat(json!({}), json!(finish)),
            ],
            true,
        );
        assert!(!out.iter().any(|e| e["type"] == "response.completed"));
        let incomplete = out.iter().find(|e| e["type"] == "response.incomplete").unwrap();
        assert_eq!(incomplete["response"]["status"], "incomplete");
        assert_eq!(incomplete["response"]["incomplete_details"]["reason"], reason);
        assert_eq!(incomplete["response"]["output"][0]["content"][0]["text"], "partial");
    }
}

#[test]
fn responses_terminal_hydrates_late_tool_identity_and_arguments_for_claude() {
    let request = json!({"model":"chosen","stream":true,"messages":[],"tools":[{"name":"lookup","input_schema":{"type":"object"}}]});
    for terminal_only in [false, true] {
        let call = json!({"type":"function_call","id":"item-1","call_id":"call-1","name":"lookup","arguments":r#"{"path":"a"}"#});
        let mut events = vec![
            json!({"type":"response.created","response":{"id":"response-1","model":"chosen"}}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"item-1","call_id":"call-1","name":"","arguments":""}}),
            json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"item-1","delta":r#"{"path":"#}),
        ];
        if !terminal_only {
            events.push(json!({"type":"response.output_item.done","output_index":0,"item":call.clone()}));
        }
        events.push(terminal(json!([call])));
        let out = run(Format::Claude, Format::Codex, &request, events, false);
        let starts: Vec<_> = out
            .iter()
            .filter(|e| e["type"] == "content_block_start" && e["content_block"]["type"] == "tool_use")
            .collect();
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0]["content_block"]["id"], "call-1");
        assert_eq!(starts[0]["content_block"]["name"], "lookup");
        let args: String = out.iter().filter_map(|e| e["delta"]["partial_json"].as_str()).collect();
        assert_eq!(serde_json::from_str::<Value>(&args).unwrap(), json!({"path":"a"}));
        assert!(out.iter().any(|e| e["delta"]["stop_reason"] == "tool_use"));
        assert_eq!(out.iter().filter(|e| e["type"] == "message_stop").count(), 1);
    }
}

#[test]
fn responses_errors_reach_claude_as_errors_without_successful_completion() {
    // The Responses error event is covered by the first-party SSE parser cited
    // above. Anthropic Messages represents a stream error with type:error and a
    // nested error type/message; no message_stop success may be synthesized.
    let request = json!({"stream":true,"messages":[]});
    for (source, kind, message) in [
        (
            json!({"type":"error","error":{"type":"api_error","message":"upstream unavailable"}}),
            "api_error",
            "upstream unavailable",
        ),
        (
            json!({"type":"error","error":{"type":"invalid_request","code":"policy","message":"request refused"}}),
            "invalid_request_error",
            "request refused",
        ),
    ] {
        let out = run(Format::Claude, Format::Codex, &request, vec![source], false);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["type"], "error");
        assert_eq!(out[0]["error"]["type"], kind);
        assert_eq!(out[0]["error"]["message"], message);
        assert!(
            !out.iter()
                .any(|e| e["type"] == "message_stop" || e["type"] == "response.completed")
        );
    }
    let out = run(
        Format::Claude,
        Format::Codex,
        &request,
        vec![json!({"type":"error","error":{},"error_type":"overloaded_error"})],
        false,
    );
    assert_eq!(out[0]["error"]["type"], "overloaded_error");
    assert!(!out[0]["error"]["message"].as_str().unwrap().is_empty());
}
