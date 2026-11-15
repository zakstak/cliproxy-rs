//! Fallback WebSocket adapter contracts. These are local transcript/transport rules,
//! not provider acceptance claims. Native Codex frames bypass these rewrites
//! (docs/codex/CONTRACT.md, "Native WebSocket route").

use super::*;
use crate::websocket_tools::{Retained, prepare_fallback_turn};
use serde_json::{Value, json};

fn value(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap()
}

fn normalized(raw: Value, last: Value, output: Value, bypass: bool) -> Value {
    let (request, remembered) = normalize(
        &raw.to_string(),
        &last.to_string(),
        &output.to_string(),
        "r1",
        &[],
        false,
        bypass,
    )
    .unwrap();
    assert_eq!(
        request, remembered,
        "the next turn remembers the transmitted transcript"
    );
    value(&request)
}

#[test]
fn fallback_first_turn_validates_and_initializes_the_transcript() {
    for input in [None, Some(json!([])), Some(json!([{"role":"user","content":"hi"}]))] {
        let mut raw = json!({"type":"response.create","model":"m","generate":true,"unknown":{"n":9}});
        if let Some(input) = &input {
            raw["input"] = input.clone();
        }
        let (request, remembered) = normalize(&raw.to_string(), "", "", "", &[], false, false).unwrap();
        assert_eq!(request, remembered);
        let mut expected = raw;
        expected.as_object_mut().unwrap().remove("type");
        expected["stream"] = json!(true);
        expected["input"] = input.unwrap_or(json!([]));
        assert_eq!(value(&request), expected);
    }
    for (raw, hint) in [
        (json!({"type":"response.create","input":[]}), "model"),
        (json!({"type":"response.create","model":"m","input":"text"}), "input"),
        (json!({"type":"response.cancel","model":"m"}), "type"),
        (json!({"type":"response.append","input":[]}), "before"),
    ] {
        let error = normalize(&raw.to_string(), "", "", "", &[], false, false).unwrap_err();
        assert_eq!(error.status, 400);
        assert!(error.message.contains(hint), "actionable failure: {error:?}");
    }
}

#[test]
fn fallback_continuations_merge_in_order_and_preserve_explicit_settings() {
    let user = json!({"type":"message","role":"user","id":"m1","content":"hello"});
    let assistant = json!({"type":"message","role":"assistant","id":"o1","content":"world"});
    let call = json!({"type":"function_call","call_id":"c1","name":"f","arguments":"{}"});
    let last = json!({"model":"m","instructions":"brief","input":[user.clone()]});
    for kind in ["response.create", "response.append"] {
        let next = json!({"type":"function_call_output","call_id":"c1","output":"ok"});
        let got = normalized(
            json!({"type":kind,"input":[next.clone()],"previous_response_id":"r1"}),
            last.clone(),
            json!([assistant.clone(), call.clone()]),
            false,
        );
        assert_eq!(
            got,
            json!({"model":"m","instructions":"brief","stream":true,
            "input":[user.clone(),assistant.clone(),call.clone(),next]})
        );
    }
    let got = normalized(
        json!({"type":"response.create","model":"other","instructions":"new","input":[]}),
        last.clone(),
        json!([]),
        false,
    );
    assert_eq!(got["model"], "other");
    assert_eq!(got["instructions"], "new");
    for bad in [json!("text"), json!({}), Value::Null] {
        let error = normalize(
            &json!({"type":"response.create","input":bad}).to_string(),
            &last.to_string(),
            "[]",
            "r1",
            &[],
            false,
            false,
        )
        .unwrap_err();
        assert_eq!(error.status, 400);
        assert!(error.message.contains("input"));
    }
    let error = normalize(
        r#"{"type":"response.create","input":[]}"#,
        r#"{"model":"m","input":"bad"}"#,
        "[]",
        "r1",
        &[],
        false,
        false,
    )
    .unwrap_err();
    assert_eq!(error.status, 400);
    assert!(error.message.contains("previous request input"));
}

#[test]
fn full_fallback_transcripts_replace_history_and_compaction_is_explicit() {
    let last = json!({"model":"m","instructions":"brief","input":[{"role":"user","content":"old"}]});
    for input in [
        json!([{"type":"message","role":"assistant","content":"replay"},{"type":"message","role":"user","content":"q"}]),
        json!([{"type":"function_call","call_id":"new","name":"f","arguments":"{}"}]),
    ] {
        let got = normalized(
            json!({"type":"response.create","input":input.clone()}),
            last.clone(),
            json!([]),
            false,
        );
        assert_eq!(got["input"], input);
        assert_eq!(got["model"], "m");
        assert_eq!(got["instructions"], "brief");
    }
    let compacted =
        json!([{"type":"compaction","encrypted_content":"opaque"},{"type":"message","role":"user","content":"after"}]);
    let got = normalized(
        json!({"type":"response.create","input":compacted.clone()}),
        last.clone(),
        json!([]),
        true,
    );
    assert_eq!(got["input"], compacted);
    let got = normalized(
        json!({"type":"response.create","input":compacted}),
        last,
        json!([]),
        false,
    );
    assert_eq!(
        got["input"],
        json!([{"role":"user","content":"old"},{"type":"message","role":"user","content":"after"}])
    );
}

#[test]
fn incremental_fallback_continuation_requires_the_pending_tool_outputs() {
    let last = r#"{"model":"m","instructions":"brief","input":[{"role":"user","content":"old"}]}"#;
    let outputs = json!([{"type":"function_call_output","call_id":"c1","output":"ok"}]);
    let raw = json!({"type":"response.create","input":outputs});
    let (request, remembered) = normalize(&raw.to_string(), last, "[]", "r1", &["c1".into()], true, false).unwrap();
    assert_eq!(request, remembered);
    assert_eq!(
        value(&request),
        json!({"input":outputs,"model":"m","instructions":"brief","stream":true,"previous_response_id":"r1"})
    );
    let (request, _) = normalize(
        r#"{"type":"response.create","input":[{"role":"user","content":"new"}]}"#,
        last,
        "[]",
        "r1",
        &["c1".into()],
        true,
        false,
    )
    .unwrap();
    assert!(value(&request).get("previous_response_id").is_none());
}

#[test]
fn fallback_passthrough_only_routes_the_model_and_enables_streaming() {
    for (raw, model, expected) in [
        (
            r#"{"type":"response.create","previous_response_id":"r1","input":[],"n":1.50}"#,
            "m",
            json!({"type":"response.create","previous_response_id":"r1","input":[],"n":1.5,"model":"m","stream":true}),
        ),
        (
            r#"{"type":"response.append","model":"explicit","input":[]}"#,
            "m",
            json!({"type":"response.append","model":"explicit","input":[],"stream":true}),
        ),
    ] {
        let got = normalize_passthrough(raw, model).unwrap();
        assert_eq!(value(&got), expected);
        if raw.contains("1.50") {
            assert!(got.contains("1.50"), "untouched numeric bytes");
        }
    }
    for (raw, model) in [
        (r#"{"type":"response.create","input":[]}"#, ""),
        (r#"{"type":"response.create","input":[]"#, "m"),
        (r#"{"type":"other","model":"m"}"#, "m"),
    ] {
        let error = normalize_passthrough(raw, model).unwrap_err();
        assert_eq!(error.status, 400);
        assert!(!error.message.is_empty());
    }
}

#[test]
fn fallback_prewarm_followup_merges_unsent_input_without_a_synthetic_parent() {
    let warmup = json!({"model":"m","instructions":"warm","input":[{"id":"w1","role":"user","content":"setup"}]});
    for kind in ["response.create", "response.append"] {
        let next = json!({"id":"w2","role":"user","content":"go"});
        let (request, remembered) = prewarm_followup(
            &json!({"type":kind,"previous_response_id":"resp_prewarm_x","input":[next.clone()]}).to_string(),
            &warmup.to_string(),
        )
        .unwrap();
        assert_eq!(request, remembered);
        assert_eq!(
            value(&request),
            json!({"model":"m","instructions":"warm","stream":true,"input":[warmup["input"][0].clone(),next]})
        );
    }
    let (request, _) = prewarm_followup(
        r#"{"type":"response.append","input":[{"id":"w1","role":"user","content":"replacement"}]}"#,
        &warmup.to_string(),
    )
    .unwrap();
    assert_eq!(
        value(&request)["input"],
        json!([{"id":"w1","role":"user","content":"replacement"}])
    );
    for raw in [
        r#"{"type":"response.append","input":"x"}"#,
        r#"{"type":"bogus","input":[]}"#,
    ] {
        assert_eq!(prewarm_followup(raw, &warmup.to_string()).unwrap_err().status, 400);
    }
}

#[test]
fn prewarm_is_detected_only_for_explicit_false_on_a_fallback_create() {
    // Legacy fallback input accepts these saved-client spellings. This says nothing
    // about a provider's accepted generate type or native Codex prewarm handling.
    for generate in [json!(false), json!("FALSE"), json!(0)] {
        assert!(is_local_prewarm(
            &json!({"type":"response.create","generate":generate}).to_string()
        ));
    }
    for raw in [
        r#"{"type":"response.create","generate":true}"#,
        r#"{"type":"response.create"}"#,
        r#"{"type":"response.append","generate":false}"#,
    ] {
        assert!(!is_local_prewarm(raw));
    }
}

#[test]
fn fallback_prewarm_events_have_consistent_identity_and_zero_usage() {
    for model in [None, Some("m")] {
        let request = model.map_or_else(|| "{}".into(), |m| json!({"model":m}).to_string());
        let events = prewarm_payloads(&request, "resp_prewarm_test", 123);
        for (index, event) in events.iter().enumerate() {
            let event = value(event);
            assert_eq!(event["type"], ["response.created", "response.completed"][index]);
            assert_eq!(event["sequence_number"], index);
            assert_eq!(event["response"]["id"], "resp_prewarm_test");
            assert_eq!(event["response"]["created_at"], 123);
            assert_eq!(event["response"]["status"], ["in_progress", "completed"][index]);
            assert_eq!(event["response"]["output"], json!([]));
            assert_eq!(event["response"]["model"].as_str(), model);
        }
        assert_eq!(
            value(&events[1])["response"]["usage"],
            json!({"input_tokens":0,"input_tokens_details":{"cached_tokens":0},"output_tokens":0,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":0})
        );
    }
}

#[test]
fn stream_chunks_extract_json_payloads_without_changing_their_bytes() {
    for payload in [
        r#"{"a":1.50}"#,
        r#"{"type":"response.completed"}"#,
        r#"{"x":"\u0061","x":"b"}"#,
    ] {
        for chunk in [
            payload.to_owned(),
            format!("event: ignored\ndata: {payload}\r\n\n"),
            format!("data:{payload}"),
        ] {
            assert_eq!(payloads_from_chunk(chunk.as_bytes()), [payload]);
        }
    }
    assert_eq!(
        payloads_from_chunk(b"data: {\"a\":1}\r\ndata: [DONE]\n\ndata: not json\n"),
        [r#"{"a":1}"#]
    );
    assert_eq!(
        payloads_from_chunk(b"{\"a\":1}\n{\"b\":2}"),
        [r#"{"a":1}"#, r#"{"b":2}"#]
    );
    for chunk in [b"  \n\n".as_slice(), b"data: [DONE]", b"event: metadata\nnot json"] {
        assert!(payloads_from_chunk(chunk).is_empty());
    }
}

#[test]
fn websocket_errors_have_semantic_status_details_and_headers() {
    for (status, kind, code) in [
        (400, "invalid_request_error", None),
        (401, "authentication_error", Some("invalid_api_key")),
        (403, "permission_error", Some("insufficient_quota")),
        (404, "invalid_request_error", Some("model_not_found")),
        (408, "server_error", Some("request_timeout")),
        (429, "rate_limit_error", Some("rate_limit_exceeded")),
        (0, "server_error", Some("internal_server_error")),
    ] {
        let got = value(&error_payload(status, "bad <input> & details", None));
        assert_eq!(got["type"], "error");
        assert_eq!(got["status"], if status == 0 { 500 } else { status });
        assert_eq!(got["error"]["message"], "bad <input> & details");
        assert_eq!(got["error"]["type"], kind);
        assert_eq!(got["error"]["code"].as_str(), code);
    }
    for (raw, expected) in [
        (
            json!({"error":{"code":"previous_response_not_found","message":"missing"}}),
            json!({"code":"previous_response_not_found","message":"missing"}),
        ),
        (json!({"detail":"upstream"}), json!({"detail":"upstream"})),
        (json!([1, 2]), json!([1, 2])),
    ] {
        assert_eq!(value(&error_payload(409, &raw.to_string(), None))["error"], expected);
    }
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("x-request-id", "request-test".parse().unwrap());
    let got = value(&error_payload(429, "", Some(&headers)));
    assert_eq!(got["error"]["message"], "Too Many Requests");
    let headers = got["headers"].as_object().unwrap();
    assert_eq!(
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("x-request-id"))
            .unwrap()
            .1,
        "request-test"
    );
}

#[test]
fn request_failures_are_distinct_from_account_and_availability_failures() {
    for (status, body, expected) in [
        (
            400,
            r#"{"error":{"code":"context_length_exceeded","message":"too long"}}"#,
            true,
        ),
        (400, "plain", true),
        (429, r#"{"error":{"type":"invalid_request_error"}}"#, false),
        (404, r#"{"error":{"code":"model_not_found"}}"#, false),
        (401, r#"{"error":{"type":"authentication_error"}}"#, false),
        (502, r#"{"error":{"code":"previous_response_not_found"}}"#, true),
        (
            500,
            "Item with id 'x' not found. Items are not persisted when `store` is set to false.",
            true,
        ),
        (503, "unavailable", false),
        (413, r#"{"error":{"code":"message_too_big"}}"#, true),
    ] {
        assert_eq!(
            crate::classify::is_request_fault(status, body),
            expected,
            "{status} {body}"
        );
    }
}

#[test]
fn fallback_completion_orders_items_and_reconciles_complete_tool_arguments() {
    let message = json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"a < b"}]});
    let call = json!({"type":"function_call","call_id":"c1","name":"f","arguments":"{\"a\":1}"});
    let mut turn = Turn::default();
    for event in [
        json!({"type":"response.created","response":{"id":"r1"}}),
        json!({"type":"response.output_item.done","output_index":1,"item":call.clone()}),
        json!({"type":"response.output_item.done","output_index":0,"item":message.clone()}),
        json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":"incomplete","name":"g"}}),
    ] {
        turn.collect(&event.to_string());
        turn.track_pending(&event.to_string());
    }
    for terminal in ["response.completed", "response.done"] {
        let restored = turn.restore_completion(json!({"type":terminal,"response":{"id":"r1","output":[]}}).to_string());
        assert_eq!(value(&restored)["type"], terminal);
        assert_eq!(
            value(&restored)["response"]["output"],
            json!([message.clone(), call.clone()])
        );
        assert_eq!(
            value(&turn.completed_output(&restored)),
            json!([message.clone(), call.clone()])
        );
        let replaced = turn.restore_completion(json!({"type":terminal,"response":{"output":[{"type":"function_call","call_id":"c1","name":"f","arguments":""},message.clone()]}}).to_string());
        assert_eq!(
            value(&replaced)["response"]["output"],
            json!([call.clone(), message.clone()])
        );
    }
    assert_eq!(turn.pending(), ["c1"]);
    turn.track_pending(
        r#"{"type":"response.output_item.done","item":{"type":"function_call_output","call_id":"c1","output":"ok"}}"#,
    );
    assert!(turn.pending().is_empty());
    turn.reset();
    let payload = json!({"type":"response.completed","response":{"id":"r2","output":[call.clone()]}}).to_string();
    assert_eq!(turn.restore_completion(payload.clone()), payload);
    turn.track_pending(&payload);
    assert_eq!(turn.pending(), ["c1"]);
}

#[test]
fn retained_fallback_tool_history_repairs_only_matching_committed_calls() {
    let key = format!("fallback-contract-{}", uuid::Uuid::new_v4());
    let retained = Retained::new(key.clone());
    let call = json!({"type":"function_call","call_id":"c1","name":"f","arguments":"{}"});
    let user = json!({"type":"message","role":"user"});
    let raw = json!({"input":[user.clone(),call.clone()]}).to_string();
    assert_eq!(
        value(&prepare_fallback_turn(&key, raw.clone()).0),
        json!({"input":[user.clone()]})
    );
    let output = json!({"type":"function_call_output","call_id":"c1","output":"ok"});
    let committed = json!({"input":[output.clone()],"previous_response_id":"r"}).to_string();
    prepare_fallback_turn(&key, committed).1.unwrap().commit();
    assert_eq!(
        value(&prepare_fallback_turn(&key, raw).0),
        json!({"input":[user.clone(),call,output]})
    );
    assert_eq!(
        value(
            &prepare_fallback_turn(
                &key,
                json!({"input":[{"type":"function_call_output","call_id":"c2","output":"orphan"}]}).to_string()
            )
            .0
        ),
        json!({"input":[]})
    );
    let response_call = json!({"type":"function_call","call_id":"c2","name":"g","arguments":"{}"});
    let (_, turn) = prepare_fallback_turn(&key, json!({"input":[user]}).to_string());
    let mut turn = turn.unwrap();
    turn.record_response(&json!({"type":"response.output_item.done","item":response_call.clone()}).to_string());
    turn.commit();
    let outputs = json!([{"type":"function_call_output","call_id":"c2","output":"late"},{"type":"function_call_output","call_id":"c2","output":"again"}]);
    let got = value(&prepare_fallback_turn(&key, json!({"input":outputs}).to_string()).0);
    assert_eq!(
        got["input"],
        json!([response_call, outputs[0].clone(), outputs[1].clone()])
    );
    let metadata_only = json!({"input":[{"type":"function_call_output","output":"heartbeat","name":"hb"},{"type":"function_call_output","output":"nameless"},{"type":"function_call","name":"f","arguments":"{}"}]}).to_string();
    assert_eq!(
        value(&prepare_fallback_turn(&key, metadata_only).0),
        json!({"input":[{"type":"function_call_output","output":"heartbeat","name":"hb"}]})
    );
    let text_input = r#"{"input":"text"}"#;
    assert_eq!(prepare_fallback_turn(&key, text_input.into()).0, text_input);
    assert_eq!(value(&prepare_fallback_turn("", json!({"input":[{"type":"function_call","call_id":"unknown","name":"f","arguments":"{}","id":"x"},{"type":"message","id":"x"}]}).to_string()).0), json!({"input":[{"type":"message","id":"x"}]}));
    drop(retained);
}

#[test]
fn close_reasons_fit_the_byte_limit_on_utf8_boundaries() {
    for reason in [
        "upstream requires HTTP replay".to_owned(),
        "é".repeat(70),
        "🦀 abc".repeat(25),
    ] {
        for max in [0, 1, 2, 3, 4, 5, 122, 123, 124, 256] {
            let got = truncate_reason(&reason, max);
            let end = reason
                .char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(reason.len()))
                .filter(|i| *i <= max)
                .max()
                .unwrap();
            assert_eq!(got, reason[..end]);
            assert!(got.len() <= max);
        }
    }
}

/// The xAI transcript reads frames through `cpa_common::rawjson` (the third-party gjson
/// crate is gone). Item metadata keys match case-insensitively with the last duplicate
/// winning for existing fallback clients, and every item keeps its own bytes: number spellings,
/// escapes and duplicate members included.
#[test]
fn transcript_items_keep_their_bytes() {
    let raw = r#"{"type":"message","TYPE":"function_call","id":"a","id":"b","call_id":"c","n":1e400,"s":"\u0061"}"#;
    let item = Item::parse(&rawjson::parse(raw));
    assert_eq!(
        (item.kind.as_str(), item.id.as_str(), item.call_id.as_str()),
        ("function_call", "b", "c")
    );
    assert_eq!(item.raw, raw);
    let previous = r#"{"input":[{"type":"message","id":"m1","n":1.50}]}"#;
    let append = r#"[{"type":"message","id":"m2","x":123456789012345678901234567890,"s":"\ud83d\ude00"}]"#;
    let merged = merge_input(previous, "", append).unwrap();
    assert_eq!(
        merged,
        r#"[{"type":"message","id":"m1","n":1.50},{"type":"message","id":"m2","x":123456789012345678901234567890,"s":"\ud83d\ude00"}]"#
    );
}
