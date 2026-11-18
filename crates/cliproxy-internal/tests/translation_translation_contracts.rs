//! Translator registration, framing, byte preservation and deep-input contracts.
use cliproxy_internal::format::Format;
use cliproxy_internal::translation::{
    RequestCtx, ResponseCtx, pair, sse::Framer, token_count, translate_request, translate_token_count,
};
use serde_json::Value;

#[test]
fn fragmented_sse_keeps_tool_state_request_local() {
    let pair = pair(Format::OpenAI, Format::Claude).unwrap();
    let ctx = ResponseCtx {
        model: "m",
        original_request: b"{}",
        translated_request: b"{}",
        tool_search: false,
    };
    let input = b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":3,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call-x\",\"name\":\"f\"}}\n\ndata: {\"type\":\"content_block_delta\",\"index\":3,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"x\\\":1}\"}}\r\n\r\ndata: {\"type\":\"content_block_stop\",\"index\":3}\n\n";
    for width in [1, 7, 23, input.len()] {
        let mut framer = Framer::default();
        let mut stream = (pair.stream)(&ctx);
        let mut output = Vec::new();
        for chunk in input.chunks(width) {
            for event in framer.push(chunk).unwrap() {
                output.extend(stream.event(&event).unwrap());
            }
        }
        assert_eq!(output.len(), 1);
        let raw = std::str::from_utf8(&output[0]).unwrap();
        assert!(
            raw.starts_with("data: ") && raw.ends_with("\n\n"),
            "OpenAI clients get data frames"
        );
        let value: Value = serde_json::from_str(raw.trim().strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(
            value["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "{\"x\":1}"
        );
        assert!(
            (pair.stream)(&ctx)
                .event(b"data: {\"type\":\"content_block_stop\",\"index\":3}\n\n")
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn openai_passthrough_is_byte_oriented() {
    // Invalid UTF-8 must neither error nor be repaired.
    let pair = pair(Format::OpenAI, Format::OpenAI).unwrap();
    let ctx = ResponseCtx {
        model: "m",
        original_request: b"",
        translated_request: b"",
        tool_search: false,
    };
    let mut s = (pair.stream)(&ctx);
    let out = s.event(b"data: {\"x\":\"\xff\"}\n\n").unwrap();
    assert_eq!(out, [bytes::Bytes::from_static(b"data: {\"x\":\"\xff\"}\n\n")]);
    assert!(s.event(b"data: [DONE]\n\n").unwrap().is_empty());
    assert!(s.event(b"data: {}\n\n").unwrap().is_empty(), "nothing after [DONE]");
    let body = b"{\"model\":\"m\",\"x\":\"\xff\"}";
    let req = RequestCtx {
        model: "m",
        stream: true,
        system_turns: false,
        tool_search: false,
    };
    assert_eq!(
        translate_request(Format::OpenAI, Format::OpenAI, &req, body).unwrap(),
        body
    );
}

#[test]
fn registry_adapters_and_unregistered_model_rewrites_are_explicit() {
    for upstream in [Format::Claude, Format::OpenAI] {
        let p = pair(Format::OpenAI, upstream).unwrap();
        assert!(p.count_tokens.is_none() && token_count(Format::OpenAI, upstream).is_none());
    }
    assert!(pair(Format::Claude, Format::Claude).is_none());
    // Unregistered pairs fall back to a model rewrite only.
    let ctx = RequestCtx {
        model: "new",
        system_turns: false,
        stream: false,
        tool_search: false,
    };
    assert_eq!(
        translate_request(Format::Claude, Format::Claude, &ctx, br#"{"model":"old","x":"<"}"#).unwrap(),
        br#"{"model":"new","x":"<"}"#
    );
    assert_eq!(
        translate_request(Format::Claude, Format::Claude, &ctx, br#"{"model":"new"}"#).unwrap(),
        br#"{"model":"new"}"#
    );
    assert_eq!(translate_token_count(Format::Claude, Format::Claude, 5, b"raw"), b"raw");
}

#[test]
fn deeply_nested_client_bodies_translate_without_overflowing() {
    let depth = 50_000;
    let schema = [vec![b'['; depth], vec![b']'; depth]].concat();
    let body = [
        &br#"{"tools":[{"name":"t","input_schema":{"type":"object","properties":{"x":"#[..],
        &schema,
        b"}}}],\"messages\":[{\"role\":\"user\",\"content\":\"x\"}]}",
    ]
    .concat();
    let ctx = RequestCtx {
        system_turns: false,
        model: "gpt-test",
        stream: false,
        tool_search: false,
    };
    let out = translate_request(Format::Claude, Format::OpenAI, &ctx, &body).unwrap();
    assert!(out.starts_with(br#"{"model":"gpt-test","messages":"#));
    assert!(out.windows(depth).any(|w| w.iter().all(|&c| c == b'[')));
}

#[test]
fn deeply_nested_upstream_bodies_translate_without_overflowing() {
    let depth = 50_000;
    let nested = [vec![b'['; depth], vec![b']'; depth]].concat();
    let rctx = ResponseCtx {
        model: "m",
        original_request: b"{}",
        translated_request: b"{}",
        tool_search: false,
    };
    // Non-stream: a Claude body whose tool input nests deeply.
    let body = [
        &br#"{"id":"m","type":"message","role":"assistant","model":"c","content":[{"type":"tool_use","id":"t","name":"f","input":{"x":"#[..],
        &nested,
        b"}}],\"stop_reason\":\"tool_use\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}",
    ]
    .concat();
    let out = (pair(Format::OpenAI, Format::Claude).unwrap().non_stream)(&rctx, &body).unwrap();
    assert!(
        out.starts_with(br#"{"id":"m""#),
        "{}",
        String::from_utf8_lossy(&out[..80])
    );
    // Stream: a deeply nested content block as one upstream event.
    let mut stream = (pair(Format::OpenAIResponse, Format::Claude).unwrap().stream)(&rctx);
    let event = [
        &br#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"f","input":{"x":"#[..],
        &nested,
        b"}}}\n\n",
    ]
    .concat();
    stream.event(&event).unwrap();
}

#[test]
fn deeply_nested_retained_state_translates_without_overflowing() {
    let depth = 50_000;
    let deep_run = |out: &[u8]| out.windows(depth).any(|w| w.iter().all(|&c| c == b'['));
    // Tool arguments that only nest once the stream's string deltas are joined.
    let ctx = ResponseCtx {
        model: "claude-sonnet-4-5",
        original_request: br#"{"model":"m","input":"q"}"#,
        translated_request: b"{}",
        tool_search: false,
    };
    let mut stream = (pair(Format::OpenAIResponse, Format::Claude).unwrap().stream)(&ctx);
    let ev = |json: &str| format!("data: {json}\n\n").into_bytes();
    let mut out = vec![];
    out.extend(
        stream
            .event(&ev(r#"{"type":"message_start","message":{"id":"m1","model":"c"}}"#))
            .unwrap(),
    );
    out.extend(
        stream
            .event(&ev(
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"f","input":{}}}"#,
            ))
            .unwrap(),
    );
    for part in [&b"{\"x\":"[..], &vec![b'['; depth], &vec![b']'; depth], b"}"] {
        for piece in part.chunks(5_000) {
            let delta = serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":String::from_utf8(piece.to_vec()).unwrap()}});
            out.extend(stream.event(&ev(&delta.to_string())).unwrap());
        }
    }
    for json in [
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":1}}"#,
        r#"{"type":"message_stop"}"#,
    ] {
        out.extend(stream.event(&ev(json)).unwrap());
    }
    out.extend(stream.finish().unwrap());
    let completed = out
        .iter()
        .find(|f| f.starts_with(b"event: response.completed"))
        .unwrap();
    assert!(deep_run(completed));
}
