//! Request-independent response fields mapped between the public schemas. Source
//! payloads and expected content, call identity, arguments, usage, and completion state
//! are written here. Historical recordings are not inputs to these assertions.
//!
//! Responses source: docs/codex/CONTRACT.md, "Sources of authority" and
//! "Contract fixtures and test helpers"; the pinned first-party SSE parser
//! codex-rs/codex-api/src/sse/responses.rs and protocol ResponseItem types cover
//! the text, reasoning, function-call, usage and completed event fields used here.
//! These test translations do not assert endpoint acceptance of buffered Codex HTTP.
use cpa_core::format::Format;
use cpa_translate::{ResponseCtx, pair};
use serde_json::{Value, json};

fn original(client: Format) -> Value {
    match client {
        Format::Claude => {
            json!({"model":"chosen","tools":[{"name":"lookup","input_schema":{"type":"object","properties":{"path":{"type":"string"}}}}],"messages":[{"role":"user","content":"question"}]})
        }
        Format::OpenAI => {
            json!({"model":"chosen","tools":[{"type":"function","function":{"name":"lookup","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}}],"messages":[{"role":"user","content":"question"}]})
        }
        _ => {
            json!({"model":"chosen","tools":[{"type":"function","name":"lookup","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}],"input":"question"})
        }
    }
}
fn response(upstream: Format) -> Value {
    match upstream {
        Format::Claude => json!({"id":"message-1","type":"message","role":"assistant","model":"chosen","content":[
            {"type":"thinking","thinking":"analysis","signature":"signed"},
            {"type":"text","text":"answer 🙂"},
            {"type":"tool_use","id":"call-1","name":"lookup","input":{"path":"a.txt"}}
        ],"stop_reason":"tool_use","usage":{"input_tokens":3,"output_tokens":5}}),
        Format::OpenAI => {
            json!({"id":"message-1","object":"chat.completion","model":"chosen","created":1700000000,"choices":[{"index":0,"message":{
            "role":"assistant","content":"answer 🙂","reasoning_content":"analysis",
            "tool_calls":[{"id":"call-1","type":"function","function":{"name":"lookup","arguments":r#"{"path":"a.txt"}"#}}]
        },"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":3,"completion_tokens":5,"total_tokens":8}})
        }
        _ => {
            json!({"type":"response.completed","response":{"id":"message-1","object":"response","model":"chosen","created_at":1700000000,"status":"completed","output":[
            {"type":"reasoning","id":"r-1","summary":[{"type":"summary_text","text":"analysis"}],"encrypted_content":"signed"},
            {"type":"message","id":"m-1","role":"assistant","content":[{"type":"output_text","text":"answer 🙂"}]},
            {"type":"function_call","id":"item-1","call_id":"call-1","name":"lookup","arguments":r#"{"path":"a.txt"}"#}
        ],"usage":{"input_tokens":3,"output_tokens":5,"total_tokens":8}}})
        }
    }
}
fn pairs() -> [(Format, Format); 7] {
    use Format::{Claude, Codex, OpenAI, OpenAIResponse};
    [
        (OpenAI, Claude),
        (OpenAIResponse, Claude),
        (Claude, OpenAI),
        (OpenAIResponse, OpenAI),
        (OpenAI, Codex),
        (Claude, Codex),
        (OpenAIResponse, Codex),
    ]
}

#[test]
fn buffered_pairs_keep_text_reasoning_tool_identity_arguments_and_usage() {
    for (client, upstream) in pairs() {
        let request = serde_json::to_vec(&original(client)).unwrap();
        let ctx = ResponseCtx {
            model: "chosen",
            original_request: &request,
            translated_request: &request,
            tool_search: false,
        };
        let raw = serde_json::to_vec(&response(upstream)).unwrap();
        let out = (pair(client, upstream).unwrap().non_stream)(&ctx, &raw).unwrap();
        let out: Value = serde_json::from_slice(&out).unwrap();
        assert!(out["id"].as_str().is_some_and(|id| !id.is_empty()));
        assert_eq!(out["model"], "chosen", "{client:?} <- {upstream:?}");
        let (text, reasoning, call, input_count, output_count) = match client {
            Format::OpenAI => {
                let message = &out["choices"][0]["message"];
                assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
                let calls = message["tool_calls"].as_array().unwrap();
                assert_eq!(calls.len(), 1);
                (
                    message["content"].as_str().unwrap().to_owned(),
                    message["reasoning_content"].as_str().unwrap().to_owned(),
                    json!({"id":calls[0]["id"],"name":calls[0]["function"]["name"],"arguments":serde_json::from_str::<Value>(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap()}),
                    out["usage"]["prompt_tokens"].as_i64().unwrap(),
                    out["usage"]["completion_tokens"].as_i64().unwrap(),
                )
            }
            Format::Claude => {
                assert_eq!(out["stop_reason"], "tool_use");
                let content = out["content"].as_array().unwrap();
                let text: String = content
                    .iter()
                    .filter(|b| b["type"] == "text")
                    .map(|b| b["text"].as_str().unwrap())
                    .collect();
                let reasoning: String = content
                    .iter()
                    .filter(|b| b["type"] == "thinking")
                    .map(|b| b["thinking"].as_str().unwrap())
                    .collect();
                let calls: Vec<&Value> = content.iter().filter(|b| b["type"] == "tool_use").collect();
                assert_eq!(calls.len(), 1);
                (
                    text,
                    reasoning,
                    json!({"id":calls[0]["id"],"name":calls[0]["name"],"arguments":calls[0]["input"]}),
                    out["usage"]["input_tokens"].as_i64().unwrap(),
                    out["usage"]["output_tokens"].as_i64().unwrap(),
                )
            }
            _ => {
                assert_eq!(out["status"], "completed");
                let output = out["output"].as_array().unwrap();
                let text: String = output
                    .iter()
                    .filter(|i| i["type"] == "message")
                    .flat_map(|i| i["content"].as_array().unwrap())
                    .filter(|p| p["type"] == "output_text")
                    .map(|p| p["text"].as_str().unwrap())
                    .collect();
                let reasoning: String = output
                    .iter()
                    .filter(|i| i["type"] == "reasoning")
                    .flat_map(|i| i["summary"].as_array().unwrap())
                    .map(|p| p["text"].as_str().unwrap())
                    .collect();
                let calls: Vec<&Value> = output.iter().filter(|i| i["type"] == "function_call").collect();
                assert_eq!(calls.len(), 1);
                (
                    text,
                    reasoning,
                    json!({"id":calls[0]["call_id"],"name":calls[0]["name"],"arguments":serde_json::from_str::<Value>(calls[0]["arguments"].as_str().unwrap()).unwrap()}),
                    out["usage"]["input_tokens"].as_i64().unwrap(),
                    out["usage"]["output_tokens"].as_i64().unwrap(),
                )
            }
        };
        assert_eq!(text, "answer 🙂", "{client:?} <- {upstream:?}");
        assert_eq!(reasoning.trim(), "analysis", "{client:?} <- {upstream:?}");
        assert_eq!(
            call,
            json!({"id":"call-1","name":"lookup","arguments":{"path":"a.txt"}}),
            "{client:?} <- {upstream:?}"
        );
        assert_eq!((input_count, output_count), (3, 5), "{client:?} <- {upstream:?}");
    }
}

fn source_events(upstream: Format) -> Vec<Value> {
    match upstream {
        Format::Claude => vec![
            json!({"type":"message_start","message":{"id":"message-1","model":"chosen","usage":{"input_tokens":3,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"analysis"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"signed"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"answer 🙂"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"call-1","name":"lookup","input":{}}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":r#"{"path":"#}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":r#""a.txt"}"#}}),
            json!({"type":"content_block_stop","index":2}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}),
            json!({"type":"message_stop"}),
        ],
        Format::OpenAI => vec![
            json!({"id":"message-1","model":"chosen","created":1700000000,"choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"analysis"},"finish_reason":null}]}),
            json!({"id":"message-1","choices":[{"index":0,"delta":{"content":"answer 🙂"},"finish_reason":null}]}),
            json!({"id":"message-1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"lookup","arguments":r#"{"path":"#}}]},"finish_reason":null}]}),
            json!({"id":"message-1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":r#""a.txt"}"#}}]},"finish_reason":null}]}),
            json!({"id":"message-1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":3,"completion_tokens":5,"total_tokens":8}}),
        ],
        _ => vec![
            json!({"type":"response.created","response":{"id":"message-1","model":"chosen","created_at":1700000000}}),
            json!({"type":"response.reasoning_summary_text.delta","delta":"analysis","output_index":0,"item_id":"r-1","summary_index":0}),
            json!({"type":"response.output_text.delta","delta":"answer 🙂","output_index":1,"item_id":"m-1","content_index":0}),
            json!({"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"item-1","call_id":"call-1","name":"lookup","arguments":""}}),
            json!({"type":"response.function_call_arguments.delta","item_id":"item-1","output_index":2,"delta":r#"{"path":"#}),
            json!({"type":"response.function_call_arguments.delta","item_id":"item-1","output_index":2,"delta":r#""a.txt"}"#}),
            json!({"type":"response.output_item.done","output_index":2,"item":{"type":"function_call","id":"item-1","call_id":"call-1","name":"lookup","arguments":r#"{"path":"a.txt"}"#}}),
            response(upstream),
        ],
    }
}

#[test]
fn streaming_pairs_assemble_text_reasoning_and_split_tool_arguments() {
    for (client, upstream) in pairs() {
        let mut request = original(client);
        request["stream"] = json!(true);
        let request = serde_json::to_vec(&request).unwrap();
        let ctx = ResponseCtx {
            model: "chosen",
            original_request: &request,
            translated_request: &request,
            tool_search: false,
        };
        let mut stream = (pair(client, upstream).unwrap().stream)(&ctx);
        let mut frames = vec![];
        for event in source_events(upstream) {
            frames.extend(stream.event(format!("data: {event}\n\n").as_bytes()).unwrap());
        }
        if upstream == Format::OpenAI {
            frames.extend(stream.event(b"data: [DONE]\n\n").unwrap());
        }
        frames.extend(stream.finish().unwrap());
        let bytes: Vec<u8> = frames.iter().flat_map(|f| f.iter().copied()).collect();
        let events: Vec<Value> = bytes
            .split(|&b| b == b'\n')
            .filter_map(|l| l.strip_prefix(b"data: "))
            .filter_map(|d| serde_json::from_slice(d).ok())
            .collect();
        match client {
            Format::OpenAI => {
                assert!(events.iter().any(|e| e["model"] == "chosen"));
                let text: String = events
                    .iter()
                    .filter_map(|e| e["choices"][0]["delta"]["content"].as_str())
                    .collect();
                let reasoning: String = events
                    .iter()
                    .filter_map(|e| e["choices"][0]["delta"]["reasoning_content"].as_str())
                    .collect();
                let calls: Vec<&Value> = events
                    .iter()
                    .flat_map(|e| e["choices"][0]["delta"]["tool_calls"].as_array().into_iter().flatten())
                    .collect();
                let args: String = calls
                    .iter()
                    .filter_map(|c| c["function"]["arguments"].as_str())
                    .collect();
                assert_eq!(text, "answer 🙂", "{client:?} <- {upstream:?}");
                assert_eq!(reasoning.trim(), "analysis", "{client:?} <- {upstream:?}");
                assert!(
                    calls
                        .iter()
                        .any(|c| c["id"] == "call-1" && c["function"]["name"] == "lookup")
                );
                assert_eq!(serde_json::from_str::<Value>(&args).unwrap(), json!({"path":"a.txt"}));
                assert!(events.iter().any(|e| e["choices"][0]["finish_reason"] == "tool_calls"));
                let usage = events.iter().rev().find_map(|e| e.get("usage")).unwrap();
                assert_eq!(usage["prompt_tokens"], 3);
                assert_eq!(usage["completion_tokens"], 5);
            }
            Format::Claude => {
                assert!(events.iter().any(|e| e["message"]["model"] == "chosen"));
                let text: String = events.iter().filter_map(|e| e["delta"]["text"].as_str()).collect();
                let reasoning: String = events.iter().filter_map(|e| e["delta"]["thinking"].as_str()).collect();
                let args: String = events
                    .iter()
                    .filter_map(|e| e["delta"]["partial_json"].as_str())
                    .collect();
                assert_eq!(text, "answer 🙂", "{client:?} <- {upstream:?}");
                assert_eq!(reasoning.trim(), "analysis", "{client:?} <- {upstream:?}");
                assert_eq!(serde_json::from_str::<Value>(&args).unwrap(), json!({"path":"a.txt"}));
                assert!(events.iter().any(|e| e["content_block"]["type"] == "tool_use"
                    && e["content_block"]["id"] == "call-1"
                    && e["content_block"]["name"] == "lookup"));
                assert!(events.iter().any(|e| e["delta"]["stop_reason"] == "tool_use"));
                assert!(events.iter().any(|e| e["usage"]["output_tokens"] == 5));
            }
            _ => {
                let completed = events.iter().find(|e| e["type"] == "response.completed").unwrap();
                assert_eq!(completed["response"]["status"], "completed");
                assert!(events.iter().any(|e| e["response"]["model"] == "chosen"));
                let output = completed["response"]["output"].as_array().unwrap();
                let text: String = output
                    .iter()
                    .filter(|i| i["type"] == "message")
                    .flat_map(|i| i["content"].as_array().unwrap())
                    .filter(|p| p["type"] == "output_text")
                    .map(|p| p["text"].as_str().unwrap())
                    .collect();
                assert_eq!(text, "answer 🙂", "{client:?} <- {upstream:?}");
                let calls: Vec<&Value> = output.iter().filter(|i| i["type"] == "function_call").collect();
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0]["call_id"], "call-1");
                assert_eq!(calls[0]["name"], "lookup");
                assert_eq!(
                    serde_json::from_str::<Value>(calls[0]["arguments"].as_str().unwrap()).unwrap(),
                    json!({"path":"a.txt"})
                );
                assert_eq!(completed["response"]["usage"]["input_tokens"], 3);
                assert_eq!(completed["response"]["usage"]["output_tokens"], 5);
                assert!(
                    events
                        .iter()
                        .any(|e| e["type"] == "response.output_text.delta" && e["delta"] == "answer 🙂")
                );
            }
        }
    }
}
