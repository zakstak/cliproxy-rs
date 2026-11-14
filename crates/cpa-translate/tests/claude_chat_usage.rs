//! Claude thinking-token usage on the Chat Completions route (#80).
//!
//! Anthropic reports the reasoning share of `usage.output_tokens` as the optional
//! `usage.output_tokens_details.thinking_tokens`; `output_tokens` stays the inclusive
//! total (anthropic-sdk-python 5139cf2e, `src/anthropic/types/usage.py` and
//! `output_tokens_details.py`). OpenAI Chat Completions reports the same share as
//! `usage.completion_tokens_details.reasoning_tokens`. Each case runs through the
//! streamed and the buffered translator and checks every usage object a client sees:
//! the count moves over as reported, an explicit 0 stays 0, an unreported count stays
//! absent, and the other token counts do not change.
use cpa_core::format::Format;
use cpa_translate::{ResponseCtx, pair};
use serde_json::{Value, json};

/// Ways a client can ask for stream usage. The translator reports usage whatever the
/// request says, so the reasoning count must not depend on it either.
const REQUESTS: [&[u8]; 3] = [
    br#"{"stream":true,"stream_options":{"include_usage":true}}"#,
    br#"{"stream":true,"stream_options":{"include_usage":false}}"#,
    br#"{"stream":true}"#,
];

fn ctx(request: &[u8]) -> ResponseCtx<'_> {
    ResponseCtx {
        model: "claude-test",
        original_request: request,
        translated_request: b"{}",
        tool_search: false,
    }
}

/// A Claude message that thought, wrote some text and ended, as SSE events. `start`
/// and each of `deltas` are the usage objects of `message_start` and of successive
/// `message_delta` events.
fn claude_events(start: Value, deltas: &[Value]) -> Vec<Value> {
    let mut events = vec![
        json!({"type":"message_start","message":{"id":"msg_u","type":"message","role":"assistant","model":"claude-test","content":[],"stop_reason":null,"usage":start}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"let me think"}}),
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

/// The `data:` payloads a Chat stream wrote, in order.
fn stream(request: &[u8], events: &[Value]) -> Vec<Value> {
    let pair = pair(Format::OpenAI, Format::Claude).unwrap();
    let mut s = (pair.stream)(&ctx(request));
    let mut chunks = vec![];
    for event in sse(events) {
        chunks.extend(s.event(&event).unwrap());
    }
    chunks.extend(s.finish().unwrap());
    let mut out = vec![];
    for chunk in chunks {
        for line in String::from_utf8(chunk.to_vec()).unwrap().lines() {
            if let Some(data) = line.strip_prefix("data: ")
                && data != "[DONE]"
            {
                out.push(serde_json::from_str(data).unwrap());
            }
        }
    }
    out
}

fn non_stream(request: &[u8], body: &[u8]) -> Value {
    let pair = pair(Format::OpenAI, Format::Claude).unwrap();
    serde_json::from_slice(&(pair.non_stream)(&ctx(request), body).unwrap()).unwrap()
}

/// The usage objects of a Chat stream, in order.
fn usages(chunks: &[Value]) -> Vec<Value> {
    chunks
        .iter()
        .filter(|c| c["usage"].is_object())
        .map(|c| c["usage"].clone())
        .collect()
}

/// The Chat usage the counts below produce, without `completion_tokens_details`.
/// `prompt_tokens` adds the cache buckets to `input_tokens`, as before #80.
fn base_usage() -> Value {
    json!({
        "prompt_tokens": 5 + 2 + 3,
        "completion_tokens": 10,
        "total_tokens": 5 + 2 + 3 + 10,
        "prompt_tokens_details": {"cached_tokens": 3, "cached_creation_tokens": 2, "cache_write_tokens": 2},
    })
}

fn with_reasoning(tokens: i64) -> Value {
    let mut usage = base_usage();
    usage["completion_tokens_details"] = json!({"reasoning_tokens": tokens});
    usage
}

/// The `message_start` usage: input and cache counts, and the first output token.
fn start_usage() -> Value {
    json!({"input_tokens":5,"cache_creation_input_tokens":2,"cache_read_input_tokens":3,"output_tokens":1})
}

/// Asserts the stream's usage objects (the finish chunk's and the trailing usage-only
/// chunk's) and the buffered answer's usage all equal `want`.
fn assert_usage(events: &[Value], want: &Value) {
    for request in REQUESTS {
        let label = String::from_utf8_lossy(request);
        let chunks = stream(request, events);
        let seen = usages(&chunks);
        assert_eq!(seen.len(), 2, "{label}: finish and trailing usage: {chunks:?}");
        for usage in &seen {
            assert_eq!(usage, want, "{label} stream: {chunks:?}");
        }
        let trailing = chunks.last().unwrap();
        assert_eq!(trailing["choices"], json!([]), "{label}: trailing chunk last");

        let body = non_stream(request, &sse(events).concat());
        assert_eq!(body["usage"], *want, "{label} buffered: {body}");
        assert_eq!(body["choices"][0]["message"]["reasoning_content"], "let me think");
    }
}

#[test]
fn reported_thinking_tokens_become_reasoning_tokens() {
    let events = claude_events(
        start_usage(),
        &[json!({"output_tokens":10,"output_tokens_details":{"thinking_tokens":7}})],
    );
    assert_usage(&events, &with_reasoning(7));
}

#[test]
fn reported_zero_stays_zero() {
    let events = claude_events(
        start_usage(),
        &[json!({"output_tokens":10,"output_tokens_details":{"thinking_tokens":0}})],
    );
    assert_usage(&events, &with_reasoning(0));
}

/// Thinking text without a reported count yields no reasoning count: the proxy never
/// estimates one from the text.
#[test]
fn unreported_count_stays_absent_despite_thinking_text() {
    for delta in [
        json!({"output_tokens":10}),
        json!({"output_tokens":10,"output_tokens_details":{}}),
        json!({"output_tokens":10,"output_tokens_details":null}),
        json!({"output_tokens":10,"output_tokens_details":{"thinking_tokens":null}}),
        json!({"output_tokens":10,"output_tokens_details":{"thinking_tokens":"7"}}),
    ] {
        let events = claude_events(start_usage(), std::slice::from_ref(&delta));
        assert_usage(&events, &base_usage());
    }
}

/// A count from `message_start` survives a `message_delta` that leaves it out, and a
/// later explicit count, 0 included, replaces it. Each stream chunk reports the
/// count known at that point.
#[test]
fn partial_usage_events_keep_and_update_the_count() {
    let mut start = start_usage();
    start["output_tokens_details"] = json!({"thinking_tokens":4});
    let deltas = [
        json!({"output_tokens":6}),
        json!({"output_tokens":10,"output_tokens_details":{"thinking_tokens":0}}),
    ];
    let events = claude_events(start.clone(), &deltas);
    for request in REQUESTS {
        let chunks = stream(request, &events);
        let reasoning: Vec<Value> = usages(&chunks)
            .iter()
            .map(|u| u["completion_tokens_details"]["reasoning_tokens"].clone())
            .collect();
        assert_eq!(reasoning, [json!(4), json!(0), json!(0)], "{chunks:?}");
        assert_eq!(usages(&chunks).last().unwrap(), &with_reasoning(0));
    }
    let body = non_stream(REQUESTS[0], &sse(&events).concat());
    assert_eq!(body["usage"], with_reasoning(0), "{body}");

    // Without the later explicit 0, the start count stays.
    let events = claude_events(start, &[json!({"output_tokens":10})]);
    assert_usage(&events, &with_reasoning(4));
}

/// The buffered route also accepts a whole Claude message as JSON; its usage maps the
/// same way.
#[test]
fn buffered_json_message_maps_the_count() {
    let message = |details: Option<Value>| {
        let mut usage = start_usage();
        usage["output_tokens"] = json!(10);
        if let Some(details) = details {
            usage["output_tokens_details"] = details;
        }
        json!({"id":"msg_j","type":"message","role":"assistant","model":"claude-test",
            "content":[{"type":"thinking","thinking":"let me think","signature":"sig"},{"type":"text","text":"answer"}],
            "stop_reason":"end_turn","stop_sequence":null,"usage":usage})
        .to_string()
    };
    let body = non_stream(b"{}", message(Some(json!({"thinking_tokens":7}))).as_bytes());
    assert_eq!(body["usage"], with_reasoning(7), "{body}");
    let body = non_stream(b"{}", message(Some(json!({"thinking_tokens":0}))).as_bytes());
    assert_eq!(body["usage"], with_reasoning(0), "{body}");
    let body = non_stream(b"{}", message(None).as_bytes());
    assert_eq!(body["usage"], base_usage(), "{body}");
}
