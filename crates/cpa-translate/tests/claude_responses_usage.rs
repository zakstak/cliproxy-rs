//! Claude thinking-token usage on the Responses route (#80).
//!
//! Anthropic reports the reasoning share of `usage.output_tokens` as
//! `usage.output_tokens_details.thinking_tokens` (anthropic-sdk-python 5139cf2e,
//! `types/usage.py`, `types/output_tokens_details.py`); Responses reports the same share
//! as `usage.output_tokens_details.reasoning_tokens`. A reported count moves over as
//! sent, 0 included. Without one, the translator keeps Go's estimate (a quarter of the
//! thinking text's bytes). Each case runs through the streamed translator, whose
//! `response.completed` carries the usage, and the buffered one.
use cpa_core::format::Format;
use cpa_translate::{ResponseCtx, pair};
use serde_json::{Value, json};

const REQUEST: &[u8] = br#"{"model":"claude-test","input":"hi","stream":true}"#;

/// The thinking text below: 12 bytes, so Go's estimate is 3.
const THINKING: &str = "let me think";

fn ctx() -> ResponseCtx<'static> {
    ResponseCtx {
        model: "claude-test",
        original_request: REQUEST,
        translated_request: b"{}",
        tool_search: false,
    }
}

/// A Claude message that thought, wrote some text and ended. `start` and `deltas` are
/// the usage objects of `message_start` and of successive `message_delta` events.
fn claude_events(start: Value, deltas: &[Value]) -> Vec<Value> {
    let mut events = vec![
        json!({"type":"message_start","message":{"id":"msg_u","type":"message","role":"assistant","model":"claude-test","content":[],"stop_reason":null,"usage":start}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":THINKING}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"answer"}}),
        json!({"type":"content_block_stop","index":1}),
    ];
    for (i, usage) in deltas.iter().enumerate() {
        let delta = if i + 1 == deltas.len() {
            json!({"stop_reason":"end_turn","stop_sequence":null})
        } else {
            json!({})
        };
        events.push(json!({"type":"message_delta","delta":delta,"usage":usage}));
    }
    events.push(json!({"type":"message_stop"}));
    events
}

fn sse(events: &[Value]) -> Vec<Vec<u8>> {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()).into_bytes())
        .collect()
}

/// The `response` of the stream's `response.completed` event.
fn streamed(events: &[Value]) -> Value {
    let pair = pair(Format::OpenAIResponse, Format::Claude).unwrap();
    let mut s = (pair.stream)(&ctx());
    let mut chunks = vec![];
    for event in sse(events) {
        chunks.extend(s.event(&event).unwrap());
    }
    chunks.extend(s.finish().unwrap());
    let text: String = chunks.iter().map(|c| String::from_utf8_lossy(c).into_owned()).collect();
    let completed: Vec<Value> = text
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .map(|d| serde_json::from_str::<Value>(d).unwrap())
        .filter(|e| e["type"] == "response.completed")
        .collect();
    assert_eq!(completed.len(), 1, "{text}");
    completed[0]["response"].clone()
}

fn buffered(events: &[Value]) -> Value {
    let pair = pair(Format::OpenAIResponse, Format::Claude).unwrap();
    serde_json::from_slice(&(pair.non_stream)(&ctx(), &sse(events).concat()).unwrap()).unwrap()
}

fn start_usage() -> Value {
    json!({"input_tokens":5,"cache_creation_input_tokens":2,"cache_read_input_tokens":3,"output_tokens":1})
}

/// The Responses usage these counts produce: `input_tokens` adds the cache buckets.
fn usage(reasoning: i64) -> Value {
    json!({
        "input_tokens": 10,
        "input_tokens_details": {"cached_tokens": 3},
        "output_tokens": 10,
        "output_tokens_details": {"reasoning_tokens": reasoning},
        "total_tokens": 20,
    })
}

fn assert_usage(events: &[Value], want: &Value) {
    let response = streamed(events);
    assert_eq!(response["usage"], *want, "stream: {response}");
    let body = buffered(events);
    assert_eq!(body["usage"], *want, "buffered: {body}");
    for out in [&response, &body] {
        assert_eq!(out["output"][0]["summary"][0]["text"], THINKING, "{out}");
    }
}

/// Witness: before #80 both paths sent the estimate (3) and dropped the reported 7.
#[test]
fn reported_thinking_tokens_become_reasoning_tokens() {
    let events = claude_events(
        start_usage(),
        &[json!({"output_tokens":10,"output_tokens_details":{"thinking_tokens":7}})],
    );
    assert_usage(&events, &usage(7));
}

#[test]
fn reported_zero_stays_zero() {
    let events = claude_events(
        start_usage(),
        &[json!({"output_tokens":10,"output_tokens_details":{"thinking_tokens":0}})],
    );
    assert_usage(&events, &usage(0));
}

/// Without a reported count, or with a null or non-number one, Go's estimate stays.
#[test]
fn unreported_count_keeps_the_estimate() {
    for delta in [
        json!({"output_tokens":10}),
        json!({"output_tokens":10,"output_tokens_details":{}}),
        json!({"output_tokens":10,"output_tokens_details":null}),
        json!({"output_tokens":10,"output_tokens_details":{"thinking_tokens":null}}),
        json!({"output_tokens":10,"output_tokens_details":{"thinking_tokens":"7"}}),
    ] {
        let events = claude_events(start_usage(), std::slice::from_ref(&delta));
        assert_usage(&events, &usage(THINKING.len() as i64 / 4));
    }
}

/// A count from `message_start` survives a `message_delta` that leaves it out, and a
/// later explicit count, 0 included, replaces it.
#[test]
fn partial_usage_events_keep_and_update_the_count() {
    let mut start = start_usage();
    start["output_tokens_details"] = json!({"thinking_tokens":4});
    let kept = claude_events(start.clone(), &[json!({"output_tokens":10})]);
    assert_usage(&kept, &usage(4));
    let replaced = claude_events(
        start,
        &[
            json!({"output_tokens":6}),
            json!({"output_tokens":10,"output_tokens_details":{"thinking_tokens":0}}),
        ],
    );
    assert_usage(&replaced, &usage(0));
}
