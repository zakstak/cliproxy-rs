//! Claude stop reasons on the Chat Completions and Responses routes (#72).
//!
//! The stop reasons are Anthropic's documented `StopReason` literals and the refusal
//! detail its `RefusalStopDetails` (anthropic-sdk-python 5139cf2e,
//! `src/anthropic/types/stop_reason.py`, `message.py`, `refusal_stop_details.py`).
//! Each case runs through the streamed and the buffered translator and checks the
//! client-visible ending, the partial output and the usage, so both routes agree.
use cpa_core::format::Format;
use cpa_translate::{RequestCtx, ResponseCtx, pair, translate_request};
use serde_json::{Value, json};

/// Every `StopReason` literal Anthropic documents.
const STOP_REASONS: [&str; 7] = [
    "end_turn",
    "max_tokens",
    "stop_sequence",
    "tool_use",
    "pause_turn",
    "refusal",
    "model_context_window_exceeded",
];

fn ctx<'a>(request: &'a [u8]) -> ResponseCtx<'a> {
    ResponseCtx {
        model: "claude-test",
        original_request: request,
        translated_request: b"{}",
        tool_search: false,
    }
}

/// A Claude message that wrote some text and stopped with `reason`, as SSE events.
fn text_events(reason: &str, stop_details: Option<Value>) -> Vec<Value> {
    let mut delta = json!({"stop_reason": reason, "stop_sequence": null});
    if let Some(details) = stop_details {
        delta["stop_details"] = details;
    }
    vec![
        json!({"type":"message_start","message":{"id":"msg_c","type":"message","role":"assistant","model":"claude-test","content":[],"stop_reason":null,"usage":{"input_tokens":5,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":delta,"usage":{"output_tokens":3}}),
        json!({"type":"message_stop"}),
    ]
}

fn sse(events: &[Value]) -> Vec<Vec<u8>> {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()).into_bytes())
        .collect()
}

/// The buffered upstream body: the SSE events joined, as the executor collects them.
fn buffered(events: &[Value]) -> Vec<u8> {
    sse(events).concat()
}

/// The `data:` payloads a stream wrote, with their SSE event names.
fn stream(client: Format, request: &[u8], events: &[Value]) -> Vec<(String, Value)> {
    let pair = pair(client, Format::Claude).unwrap();
    let mut s = (pair.stream)(&ctx(request));
    let mut chunks = vec![];
    for event in sse(events) {
        chunks.extend(s.event(&event).unwrap());
    }
    chunks.extend(s.finish().unwrap());
    let mut out = vec![];
    for chunk in chunks {
        let text = String::from_utf8(chunk.to_vec()).unwrap();
        let mut name = String::new();
        for line in text.lines() {
            if let Some(n) = line.strip_prefix("event: ") {
                name = n.to_owned();
            } else if let Some(data) = line.strip_prefix("data: ")
                && data != "[DONE]"
            {
                out.push((name.clone(), serde_json::from_str(data).unwrap()));
            }
        }
    }
    out
}

fn non_stream(client: Format, request: &[u8], events: &[Value]) -> Value {
    let pair = pair(client, Format::Claude).unwrap();
    serde_json::from_slice(&(pair.non_stream)(&ctx(request), &buffered(events)).unwrap()).unwrap()
}

fn chat_finish(reason: &str) -> &'static str {
    match reason {
        "tool_use" => "tool_calls",
        "max_tokens" | "model_context_window_exceeded" => "length",
        "refusal" => "content_filter",
        _ => "stop",
    }
}

#[test]
fn chat_maps_every_documented_stop_reason_in_stream_and_buffer() {
    for reason in STOP_REASONS {
        let events = text_events(reason, None);
        let chunks = stream(Format::OpenAI, b"{}", &events);
        assert!(
            chunks.iter().all(|(_, c)| c.get("error").is_none()),
            "{reason}: {chunks:?}"
        );
        let finishes: Vec<&str> = chunks
            .iter()
            .filter_map(|(_, c)| c["choices"][0]["finish_reason"].as_str())
            .collect();
        assert_eq!(finishes, [chat_finish(reason)], "{reason} stream");
        let text: String = chunks
            .iter()
            .filter_map(|(_, c)| c["choices"][0]["delta"]["content"].as_str())
            .collect();
        assert_eq!(text, "partial", "{reason}: partial output survives");
        // The finish chunk precedes the usage-only trailing chunk.
        let finish_at = chunks
            .iter()
            .position(|(_, c)| c["choices"][0]["finish_reason"].is_string())
            .unwrap();
        let usage_at = chunks.iter().rposition(|(_, c)| c["usage"].is_object()).unwrap();
        assert!(finish_at <= usage_at, "{reason}: {chunks:?}");
        assert_eq!(chunks[usage_at].1["usage"]["completion_tokens"], 3, "{reason}");

        let body = non_stream(Format::OpenAI, b"{}", &events);
        assert_eq!(
            body["choices"][0]["finish_reason"],
            chat_finish(reason),
            "{reason} buffered"
        );
        assert_eq!(body["choices"][0]["message"]["content"], "partial", "{reason}");
        assert_eq!(body["usage"]["completion_tokens"], 3, "{reason}");
        assert!(body.get("error").is_none(), "{reason}: {body}");
    }
}

/// A tool call truncated by the output or context limit keeps `length` in the buffered
/// answer, as the stream reports it; a finished one stays `tool_calls`.
#[test]
fn chat_truncated_tool_call_keeps_length() {
    for (reason, want) in [
        ("max_tokens", "length"),
        ("model_context_window_exceeded", "length"),
        ("tool_use", "tool_calls"),
        ("refusal", "tool_calls"),
    ] {
        let events = vec![
            json!({"type":"message_start","message":{"id":"msg_t","model":"claude-test","usage":{"input_tokens":5,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"write","input":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":reason},"usage":{"output_tokens":9}}),
            json!({"type":"message_stop"}),
        ];
        let body = non_stream(Format::OpenAI, b"{}", &events);
        assert_eq!(body["choices"][0]["finish_reason"], want, "{reason}");
        assert_eq!(
            body["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"], "{\"path\":\"a",
            "{reason}: partial arguments survive"
        );
        let stream_finish: Vec<String> = stream(Format::OpenAI, b"{}", &events)
            .into_iter()
            .filter_map(|(_, c)| c["choices"][0]["finish_reason"].as_str().map(str::to_owned))
            .collect();
        let stream_want = if reason == "refusal" { "content_filter" } else { want };
        assert_eq!(stream_finish, [stream_want], "{reason} stream");
    }
}

/// (status, incomplete_details, end_turn) of a Responses answer for each stop reason.
fn responses_ending(reason: &str) -> (&'static str, Value, Value) {
    match reason {
        "max_tokens" | "model_context_window_exceeded" => {
            ("incomplete", json!({"reason":"max_output_tokens"}), Value::Null)
        }
        "refusal" => ("incomplete", json!({"reason":"content_filter"}), Value::Null),
        "pause_turn" => ("completed", Value::Null, json!(false)),
        _ => ("completed", Value::Null, Value::Null),
    }
}

#[test]
fn responses_maps_every_documented_stop_reason_in_stream_and_buffer() {
    for reason in STOP_REASONS {
        let (status, details, end_turn) = responses_ending(reason);
        let events = text_events(reason, None);
        let check = |label: &str, response: &Value| {
            assert_eq!(response["status"], status, "{reason} {label}: {response}");
            let got = response.get("incomplete_details").cloned().unwrap_or(Value::Null);
            assert_eq!(got, details, "{reason} {label}");
            assert_eq!(
                response.get("end_turn").cloned().unwrap_or(Value::Null),
                end_turn,
                "{reason} {label}"
            );
            assert!(response["error"].is_null(), "{reason} {label}: {response}");
            assert_eq!(
                response["output"][0]["content"][0]["text"], "partial",
                "{reason} {label}"
            );
            let item_status = if status == "incomplete" {
                "incomplete"
            } else {
                "completed"
            };
            assert_eq!(response["output"][0]["status"], item_status, "{reason} {label}");
            assert_eq!(response["usage"]["output_tokens"], 3, "{reason} {label}");
        };

        let events_out = stream(Format::OpenAIResponse, b"{}", &events);
        let terminals: Vec<&(String, Value)> = events_out
            .iter()
            .filter(|(name, _)| {
                matches!(
                    name.as_str(),
                    "response.completed" | "response.incomplete" | "response.failed" | "error"
                )
            })
            .collect();
        assert_eq!(terminals.len(), 1, "{reason}: {events_out:?}");
        let (name, terminal) = terminals[0];
        assert_eq!(name, &format!("response.{status}"), "{reason}");
        assert_eq!(
            events_out.last().map(|(n, _)| n),
            Some(name),
            "{reason}: the terminal event is last"
        );
        // Output items finish before the terminal event, with the same status.
        let done = events_out
            .iter()
            .find(|(n, _)| n == "response.output_item.done")
            .unwrap();
        assert_eq!(
            done.1["item"]["status"], terminal["response"]["output"][0]["status"],
            "{reason}"
        );
        check("stream", &terminal["response"]);

        check("buffered", &non_stream(Format::OpenAIResponse, b"{}", &events));
    }
}

/// A refusal with sparse or missing `stop_details` is still a refusal, and a
/// successful one: the translators keep the output and usage and report no error.
#[test]
fn sparse_refusal_identity_stays_a_successful_refusal() {
    for details in [
        None,
        Some(Value::Null),
        Some(json!({"type":"refusal"})),
        Some(json!({"type":"refusal","category":null,"explanation":null})),
        Some(json!({"type":"refusal","category":"cyber","explanation":"policy"})),
    ] {
        let events = text_events("refusal", details.clone());
        let body = non_stream(Format::OpenAI, b"{}", &events);
        assert_eq!(body["choices"][0]["finish_reason"], "content_filter", "{details:?}");
        assert_eq!(body["usage"]["completion_tokens"], 3);
        let body = non_stream(Format::OpenAIResponse, b"{}", &events);
        assert_eq!(body["status"], "incomplete", "{details:?}");
        assert_eq!(body["incomplete_details"]["reason"], "content_filter");
        assert!(body["error"].is_null());
        let out = stream(Format::OpenAIResponse, b"{}", &events);
        assert_eq!(out.last().unwrap().0, "response.incomplete", "{details:?}");
    }
}

fn search_events(reason: &str, with_result: bool) -> Vec<Value> {
    let mut events = vec![
        json!({"type":"message_start","message":{"id":"msg_ws","model":"claude-test","usage":{"input_tokens":1,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"rust\"}"}}),
        json!({"type":"content_block_stop","index":0}),
    ];
    if with_result {
        events.push(json!({"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":[{"type":"web_search_result","title":"R","url":"https://example.com","encrypted_content":"ENC"}]}}));
        events.push(json!({"type":"content_block_stop","index":1}));
    }
    events.push(json!({"type":"message_delta","delta":{"stop_reason":reason},"usage":{"output_tokens":4}}));
    events.push(json!({"type":"message_stop"}));
    events
}

/// A search left without its result by `pause_turn` or a deferring `tool_use` stays
/// `in_progress`; one with its result, or ended any other way, takes the response's
/// item status.
#[test]
fn deferred_web_search_stays_in_progress() {
    for (reason, with_result, want) in [
        ("pause_turn", false, "in_progress"),
        ("tool_use", false, "in_progress"),
        ("pause_turn", true, "completed"),
        ("tool_use", true, "completed"),
        ("end_turn", false, "completed"),
        ("max_tokens", false, "incomplete"),
    ] {
        let events = search_events(reason, with_result);
        let body = non_stream(Format::OpenAIResponse, b"{}", &events);
        assert_eq!(body["output"][0]["type"], "web_search_call");
        assert_eq!(body["output"][0]["status"], want, "{reason} {with_result} buffered");
        assert_eq!(body["output"][0]["action"]["query"], "rust");
        let out = stream(Format::OpenAIResponse, b"{}", &events);
        let done = out.iter().find(|(n, _)| n == "response.output_item.done").unwrap();
        assert_eq!(done.1["item"]["status"], want, "{reason} {with_result} stream");
        let terminal = &out.last().unwrap().1["response"];
        assert_eq!(terminal["output"][0]["status"], want, "{reason} {with_result} terminal");
    }
}

/// The continuation of a paused search returns its result under the search's ID.
#[test]
fn continuation_result_keeps_the_paused_search_id() {
    let events = vec![
        json!({"type":"message_start","message":{"id":"msg_c2","usage":{"input_tokens":1,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_paused","content":[{"type":"web_search_result","url":"https://example.com","title":"E","encrypted_content":"ENC_P"}]}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"done"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
        json!({"type":"message_stop"}),
    ];
    let check = |response: &Value| {
        let item = &response["output"][0];
        assert_eq!(item["type"], "web_search_call", "{response}");
        assert_eq!(item["id"], "ws_srvtoolu_paused");
        assert_eq!(item["status"], "completed");
        assert_eq!(item["results"][0]["encrypted_content"], "ENC_P");
        assert_eq!(response["output"][1]["content"][0]["text"], "done");
    };
    check(&non_stream(Format::OpenAIResponse, b"{}", &events));
    let out = stream(Format::OpenAIResponse, b"{}", &events);
    check(&out.last().unwrap().1["response"]);
}

fn claude_request(model: &str, input: Value) -> Value {
    let body = json!({"model": model, "input": input}).to_string();
    let ctx = RequestCtx {
        model,
        stream: true,
        tool_search: false,
    };
    serde_json::from_slice(&translate_request(Format::OpenAIResponse, Format::Claude, &ctx, body.as_bytes()).unwrap())
        .unwrap()
}

fn paused_search(id: &str, query: Option<&str>) -> Value {
    let mut action = json!({"type":"search"});
    if let Some(q) = query {
        action["query"] = json!(q);
    }
    json!({"type":"web_search_call","id":id,"status":"in_progress","action":action})
}

/// Anthropic continues a `pause_turn` response sent back as is, so a paused search at
/// the tail goes back as a `server_tool_use` without an invented result, even for a
/// model that rejects assistant prefill.
#[test]
fn paused_search_replays_without_result_and_survives_prefill_drop() {
    let input = json!([
        {"role":"user","content":"search rust"},
        paused_search("ws_srvtoolu_1", Some("rust")),
    ]);
    for model in ["claude-test", "claude-opus-5", "claude-fable-5-2"] {
        let out = claude_request(model, input.clone());
        let messages = out["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2, "{model}: {out}");
        let content = messages[1]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1, "{model}: {out}");
        assert_eq!(content[0]["type"], "server_tool_use");
        assert_eq!(content[0]["id"], "srvtoolu_1");
        assert_eq!(content[0]["input"]["query"], "rust");
    }
    // An ordinary assistant prefill is still dropped for those models.
    let out = claude_request(
        "claude-opus-5",
        json!([{"role":"user","content":"hi"},{"role":"assistant","content":"prefill"}]),
    );
    assert_eq!(out["messages"].as_array().unwrap().len(), 1, "{out}");
}

/// The paused item is replaced by its continuation within the same assistant run, which
/// keeps the paused query; tool outputs do not end that run, a user turn does.
#[test]
fn paused_search_pairs_with_its_continuation() {
    let continuation = json!({"type":"web_search_call","id":"ws_srvtoolu_1","status":"completed","action":{"type":"search"},"results":[{"type":"web_search_result","url":"https://e.com","title":"E","encrypted_content":"ENC"}]});
    let input = json!([
        {"role":"user","content":"search"},
        paused_search("ws_srvtoolu_1", Some("rust")),
        {"type":"function_call","call_id":"call_1","name":"f","arguments":"{}"},
        {"type":"function_call_output","call_id":"call_1","output":"ok"},
        continuation,
    ]);
    let out = claude_request("claude-test", input);
    let blocks: Vec<&Value> = out["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["content"].as_array().into_iter().flatten())
        .filter(|b| matches!(b["type"].as_str(), Some("server_tool_use" | "web_search_tool_result")))
        .collect();
    assert_eq!(blocks.len(), 2, "{out}");
    assert_eq!(blocks[0]["type"], "server_tool_use");
    assert_eq!(blocks[0]["input"]["query"], "rust", "the paused query carries over");
    assert_eq!(blocks[1]["type"], "web_search_tool_result");
    assert_eq!(blocks[1]["content"][0]["encrypted_content"], "ENC");

    // A user turn between them keeps each item as its own pair.
    let input = json!([
        paused_search("ws_srvtoolu_1", Some("rust")),
        {"role":"user","content":"next"},
        paused_search("ws_srvtoolu_1", None),
    ]);
    let out = claude_request("claude-test", input);
    let messages = out["messages"].as_array().unwrap();
    let first = messages[0]["content"].as_array().unwrap();
    assert_eq!(first.len(), 2, "an earlier open search keeps its pair: {out}");
    assert_eq!(first[1]["type"], "web_search_tool_result");
    let last = messages.last().unwrap()["content"].as_array().unwrap();
    assert_eq!(last.len(), 1, "the tail search has no result: {out}");
    assert!(
        last[0]["input"].get("query").is_none(),
        "no query across a user turn: {out}"
    );
}

/// The history scan is linear: many paused searches with distinct IDs replay quickly.
#[test]
fn paused_search_history_scales_linearly() {
    let mut input = vec![json!({"role":"user","content":"go"})];
    for i in 0..20_000 {
        input.push(paused_search(&format!("ws_srvtoolu_{i}"), Some("q")));
    }
    let start = std::time::Instant::now();
    let out = claude_request("claude-test", Value::Array(input));
    assert!(
        start.elapsed() < std::time::Duration::from_secs(20),
        "{:?}",
        start.elapsed()
    );
    let content = out["messages"][1]["content"].as_array().unwrap();
    assert_eq!(content.len(), 20_000);
}
