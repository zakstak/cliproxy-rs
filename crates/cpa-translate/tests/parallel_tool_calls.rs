//! Parallel tool calls, their stop reason and multiple reasoning parts on the retained
//! translated routes (#80).
//!
//! Go's v8.0.67 fixes for omitted-index tool-call deltas (`c449529e`), thinking
//! content-part arrays (`6044c5d1`) and whitespace-only reasoning (`69637a66`) change
//! the OpenAI Chat upstream translators, which only the removed OpenAI-compatible
//! executor used (#49). The retained upstreams always identify each call: Claude by its
//! content-block `index`, Responses (Grok, Muse) by `output_index` and `item_id`. These
//! tests pin that two calls in one answer stay two calls, streamed and buffered, that
//! the answer ends as a tool call, and that each reasoning part arrives. Grok and Muse
//! hand Chat and Claude clients the shared `(client, Codex)` pair's output unchanged
//! (`cpa-exec` `xai.rs`, `meta.rs`), so the pair is what they send.
use cpa_core::format::Format;
use cpa_translate::{ResponseCtx, pair};
use serde_json::{Value, json};

fn ctx(request: &[u8]) -> ResponseCtx<'_> {
    ResponseCtx {
        model: "m",
        original_request: request,
        translated_request: b"{}",
        tool_search: false,
    }
}

fn sse(events: &[Value]) -> Vec<Vec<u8>> {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()).into_bytes())
        .collect()
}

/// The `data:` payloads a pair's stream wrote, in order.
fn stream(client: Format, upstream: Format, request: &[u8], events: &[Value]) -> Vec<Value> {
    let pair = pair(client, upstream).unwrap();
    let mut s = (pair.stream)(&ctx(request));
    let mut chunks = vec![];
    for event in sse(events) {
        chunks.extend(s.event(&event).unwrap());
    }
    chunks.extend(s.finish().unwrap());
    let text: String = chunks.iter().map(|c| String::from_utf8_lossy(c).into_owned()).collect();
    text.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter(|d| *d != "[DONE]")
        .map(|d| serde_json::from_str(d).unwrap())
        .collect()
}

fn non_stream(client: Format, upstream: Format, request: &[u8], body: &[u8]) -> Value {
    let pair = pair(client, upstream).unwrap();
    serde_json::from_slice(&(pair.non_stream)(&ctx(request), body).unwrap()).unwrap()
}

const CHAT: &[u8] =
    br#"{"stream":true,"tools":[{"type":"function","function":{"name":"a"}},{"type":"function","function":{"name":"b"}}]}"#;
const RESPONSES: &[u8] = br#"{"stream":true,"tools":[{"type":"function","name":"a"},{"type":"function","name":"b"}]}"#;
const CLAUDE: &[u8] = br#"{"stream":true,"tools":[{"name":"a","input_schema":{"type":"object"}},{"name":"b","input_schema":{"type":"object"}}]}"#;

/// A Claude answer that thinks, calls `a`, thinks again, calls `b` and stops for tools.
fn claude_events() -> Vec<Value> {
    vec![
        json!({"type":"message_start","message":{"id":"msg","type":"message","role":"assistant","model":"m","content":[],"stop_reason":null,"usage":{"input_tokens":5,"output_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"one"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig1"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_a","name":"a","input":{}}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"x\":"}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"1}"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"content_block_start","index":2,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"thinking_delta","thinking":"two"}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"signature_delta","signature":"sig2"}}),
        json!({"type":"content_block_stop","index":2}),
        json!({"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"toolu_b","name":"b","input":{}}}),
        json!({"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"y\":2}"}}),
        json!({"type":"content_block_stop","index":3}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":10}}),
        json!({"type":"message_stop"}),
    ]
}

/// The terminal Responses object of [`responses_events`].
fn responses_completed() -> Value {
    json!({"type":"response.completed","response":{"id":"resp","model":"m","status":"completed","output":[
        {"type":"reasoning","id":"rs","encrypted_content":"enc","summary":[{"type":"summary_text","text":"one"},{"type":"summary_text","text":"two"}]},
        {"type":"function_call","id":"fc_a","call_id":"call_a","name":"a","arguments":"{\"x\":1}"},
        {"type":"function_call","id":"fc_b","call_id":"call_b","name":"b","arguments":"{\"y\":2}"}],
        "usage":{"input_tokens":5,"output_tokens":10,"output_tokens_details":{"reasoning_tokens":7},"total_tokens":15}}})
}

/// A Responses answer (Grok, Muse) with two reasoning summary parts and two calls whose
/// argument deltas interleave.
fn responses_events() -> Vec<Value> {
    vec![
        json!({"type":"response.created","response":{"id":"resp","model":"m"}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs","summary":[]}}),
        json!({"type":"response.reasoning_summary_part.added","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}}),
        json!({"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":0,"delta":"one"}),
        json!({"type":"response.reasoning_summary_part.added","output_index":0,"summary_index":1,"part":{"type":"summary_text","text":""}}),
        json!({"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":1,"delta":"two"}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs","encrypted_content":"enc","summary":[{"type":"summary_text","text":"one"},{"type":"summary_text","text":"two"}]}}),
        json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_a","call_id":"call_a","name":"a","arguments":""}}),
        json!({"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"fc_b","call_id":"call_b","name":"b","arguments":""}}),
        json!({"type":"response.function_call_arguments.delta","output_index":1,"item_id":"fc_a","delta":"{\"x\":"}),
        json!({"type":"response.function_call_arguments.delta","output_index":2,"item_id":"fc_b","delta":"{\"y\":2}"}),
        json!({"type":"response.function_call_arguments.delta","output_index":1,"item_id":"fc_a","delta":"1}"}),
        json!({"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"fc_a","call_id":"call_a","name":"a","arguments":"{\"x\":1}"}}),
        json!({"type":"response.output_item.done","output_index":2,"item":{"type":"function_call","id":"fc_b","call_id":"call_b","name":"b","arguments":"{\"y\":2}"}}),
        responses_completed(),
    ]
}

/// Chat stream tool-call deltas joined per `index`: (index, id, name, arguments).
fn chat_calls(chunks: &[Value]) -> Vec<(i64, String, String, String)> {
    let mut calls: Vec<(i64, String, String, String)> = vec![];
    for chunk in chunks {
        for call in chunk["choices"][0]["delta"]["tool_calls"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let index = call["index"].as_i64().expect("every tool-call delta carries its index");
            let pos = match calls.iter().position(|c| c.0 == index) {
                Some(pos) => pos,
                None => {
                    calls.push((index, String::new(), String::new(), String::new()));
                    calls.len() - 1
                }
            };
            let c = &mut calls[pos];
            c.1 += call["id"].as_str().unwrap_or_default();
            c.2 += call["function"]["name"].as_str().unwrap_or_default();
            c.3 += call["function"]["arguments"].as_str().unwrap_or_default();
        }
    }
    calls
}

fn want_chat_calls(a: &str, b: &str) -> Vec<(i64, String, String, String)> {
    vec![
        (0, a.into(), "a".into(), r#"{"x":1}"#.into()),
        (1, b.into(), "b".into(), r#"{"y":2}"#.into()),
    ]
}

fn finish_reasons(chunks: &[Value]) -> Vec<&str> {
    chunks
        .iter()
        .filter_map(|c| c["choices"][0]["finish_reason"].as_str())
        .collect()
}

fn buffered_chat_calls(body: &Value) -> Vec<(String, String, String)> {
    body["choices"][0]["message"]["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["id"].as_str().unwrap().into(),
                c["function"]["name"].as_str().unwrap().into(),
                c["function"]["arguments"].as_str().unwrap().into(),
            )
        })
        .collect()
}

#[test]
fn claude_to_chat_keeps_parallel_calls_and_reasoning_parts() {
    let chunks = stream(Format::OpenAI, Format::Claude, CHAT, &claude_events());
    assert_eq!(chat_calls(&chunks), want_chat_calls("toolu_a", "toolu_b"), "{chunks:?}");
    assert_eq!(finish_reasons(&chunks), ["tool_calls"]);
    let reasoning: Vec<&str> = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["reasoning_content"].as_str())
        .collect();
    assert_eq!(reasoning, ["one", "two"]);

    let body = non_stream(Format::OpenAI, Format::Claude, CHAT, &sse(&claude_events()).concat());
    assert_eq!(
        buffered_chat_calls(&body),
        [
            ("toolu_a".into(), "a".into(), r#"{"x":1}"#.into()),
            ("toolu_b".into(), "b".into(), r#"{"y":2}"#.into())
        ],
        "{body}"
    );
    assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(body["choices"][0]["message"]["reasoning_content"], "onetwo");
}

/// The function-call and reasoning items of a Responses output, as (type, id-ish, text).
fn responses_items(output: &Value) -> Vec<(String, String, String)> {
    output
        .as_array()
        .unwrap()
        .iter()
        .map(|item| match item["type"].as_str().unwrap() {
            "function_call" => (
                "function_call".into(),
                format!(
                    "{}:{}",
                    item["call_id"].as_str().unwrap(),
                    item["name"].as_str().unwrap()
                ),
                item["arguments"].as_str().unwrap().into(),
            ),
            "reasoning" => (
                "reasoning".into(),
                item["encrypted_content"].as_str().unwrap().into(),
                item["summary"][0]["text"].as_str().unwrap().into(),
            ),
            other => panic!("unexpected item {other}: {item}"),
        })
        .collect()
}

#[test]
fn claude_to_responses_keeps_parallel_calls_and_reasoning_parts() {
    let want: Vec<(String, String, String)> = vec![
        ("reasoning".into(), "sig1".into(), "one".into()),
        ("function_call".into(), "toolu_a:a".into(), r#"{"x":1}"#.into()),
        ("reasoning".into(), "sig2".into(), "two".into()),
        ("function_call".into(), "toolu_b:b".into(), r#"{"y":2}"#.into()),
    ];
    let events = stream(Format::OpenAIResponse, Format::Claude, RESPONSES, &claude_events());
    let added: Vec<i64> = events
        .iter()
        .filter(|e| e["type"] == "response.output_item.added")
        .map(|e| e["output_index"].as_i64().unwrap())
        .collect();
    assert_eq!(added, [0, 1, 2, 3], "one output item per block: {events:?}");
    let completed = events.iter().find(|e| e["type"] == "response.completed").unwrap();
    assert_eq!(responses_items(&completed["response"]["output"]), want);

    let body = non_stream(
        Format::OpenAIResponse,
        Format::Claude,
        RESPONSES,
        &sse(&claude_events()).concat(),
    );
    assert_eq!(responses_items(&body["output"]), want, "{body}");
}

#[test]
fn responses_to_chat_keeps_parallel_calls_and_reasoning_count() {
    let chunks = stream(Format::OpenAI, Format::Codex, CHAT, &responses_events());
    assert_eq!(chat_calls(&chunks), want_chat_calls("call_a", "call_b"), "{chunks:?}");
    assert_eq!(finish_reasons(&chunks), ["tool_calls"]);
    let reasoning: String = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["reasoning_content"].as_str())
        .collect();
    assert!(reasoning.contains("one") && reasoning.contains("two"), "{reasoning:?}");
    let usage = &chunks.iter().rfind(|c| c["usage"].is_object()).unwrap()["usage"];
    assert_eq!(usage["completion_tokens_details"]["reasoning_tokens"], 7, "{usage}");

    let body = non_stream(
        Format::OpenAI,
        Format::Codex,
        CHAT,
        responses_completed().to_string().as_bytes(),
    );
    assert_eq!(
        buffered_chat_calls(&body),
        [
            ("call_a".into(), "a".into(), r#"{"x":1}"#.into()),
            ("call_b".into(), "b".into(), r#"{"y":2}"#.into())
        ],
        "{body}"
    );
    assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(body["usage"]["completion_tokens_details"]["reasoning_tokens"], 7);
}

#[test]
fn responses_to_claude_keeps_parallel_calls_and_reasoning_parts() {
    let events = stream(Format::Claude, Format::Codex, CLAUDE, &responses_events());
    let mut blocks: Vec<(i64, Value)> = events
        .iter()
        .filter(|e| e["type"] == "content_block_start")
        .map(|e| (e["index"].as_i64().unwrap(), e["content_block"].clone()))
        .collect();
    for (index, block) in &mut blocks {
        for e in events
            .iter()
            .filter(|e| e["type"] == "content_block_delta" && e["index"] == *index)
        {
            let delta = &e["delta"];
            match delta["type"].as_str().unwrap() {
                "thinking_delta" => {
                    block["thinking"] = json!(format!(
                        "{}{}",
                        block["thinking"].as_str().unwrap(),
                        delta["thinking"].as_str().unwrap()
                    ));
                }
                "signature_delta" => block["signature"] = delta["signature"].clone(),
                "input_json_delta" => {
                    let partial = block["partial"].as_str().unwrap_or_default().to_owned();
                    block["partial"] = json!(partial + delta["partial_json"].as_str().unwrap());
                }
                other => panic!("unexpected delta {other}"),
            }
        }
    }
    assert_eq!(
        blocks,
        [
            (0, json!({"type":"thinking","thinking":"one\n\ntwo","signature":"enc"})),
            (
                1,
                json!({"type":"tool_use","id":"call_a","name":"a","input":{},"partial":"{\"x\":1}"})
            ),
            (
                2,
                json!({"type":"tool_use","id":"call_b","name":"b","input":{},"partial":"{\"y\":2}"})
            ),
        ],
        "{events:?}"
    );
    let delta = events.iter().find(|e| e["type"] == "message_delta").unwrap();
    assert_eq!(delta["delta"]["stop_reason"], "tool_use");

    let body = non_stream(
        Format::Claude,
        Format::Codex,
        CLAUDE,
        responses_completed().to_string().as_bytes(),
    );
    let content: Vec<Value> = body["content"].as_array().unwrap().clone();
    assert_eq!(content.len(), 3, "{body}");
    assert_eq!(content[0]["type"], "thinking");
    assert_eq!(content[0]["signature"], "enc");
    assert_eq!(
        content[1..],
        [
            json!({"type":"tool_use","id":"call_a","name":"a","input":{"x":1}}),
            json!({"type":"tool_use","id":"call_b","name":"b","input":{"y":2}}),
        ],
        "{body}"
    );
    assert_eq!(body["stop_reason"], "tool_use");
}
