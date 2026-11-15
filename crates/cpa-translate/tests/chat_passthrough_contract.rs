//! Same-format Chat requests preserve every field except the selected model. Buffered
//! responses preserve their bytes. A streaming response preserves each data payload
//! until its first DONE marker. None of these expectations uses recorded outputs.
use cpa_core::format::Format;
use cpa_translate::{RequestCtx, ResponseCtx, go_stream, pair, translate_request};
use serde_json::{Value, json};

fn corpus() -> Vec<Value> {
    let doc: Value = serde_json::from_str(include_str!("fixtures/chat_passthrough_inputs.json")).unwrap();
    doc["cases"].as_array().unwrap().clone()
}
fn input(case: &Value, key: &str) -> Vec<u8> {
    let text = case[key].as_str().unwrap_or_default();
    if case["bytes"].as_bool().unwrap_or(false) {
        text.chars().map(|c| u8::try_from(c as u32).unwrap()).collect()
    } else {
        text.as_bytes().to_vec()
    }
}

#[test]
fn every_unambiguous_chat_request_keeps_all_fields_except_the_selected_model() {
    for case in corpus().into_iter().filter(|c| c["path"] == "request") {
        let raw = input(&case, "input");
        let Ok(mut expected) = serde_json::from_slice::<Value>(&raw) else {
            // A model rewrite of syntactically invalid JSON has no client contract.
            // Opaque passthrough bytes are tested separately below.
            continue;
        };
        if !expected.is_object() {
            continue;
        }
        // Duplicate object keys have no unique JSON meaning. A parser's first-key
        // or last-key choice is not part of the supported client contract.
        let mut model_keys = 0;
        cpa_common::json::object_members(&raw, |key, _, _| {
            model_keys += usize::from(
                serde_json::from_slice::<String>(&[b"\"".as_slice(), key, b"\""].concat()).unwrap() == "model",
            );
            true
        });
        if model_keys > 1 {
            continue;
        }
        let model = case["model"].as_str().unwrap();
        expected["model"] = json!(model);
        let ctx = RequestCtx {
            model,
            stream: case["stream"].as_bool().unwrap_or(false),
            tool_search: false,
            system_turns: false,
        };
        let actual = translate_request(Format::OpenAI, Format::OpenAI, &ctx, &raw).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&actual).unwrap(),
            expected,
            "{}",
            case["name"]
        );
    }
}

#[test]
fn buffered_chat_responses_keep_all_bytes_including_invalid_json() {
    let ctx = ResponseCtx {
        model: "m",
        original_request: b"",
        translated_request: b"",
        tool_search: false,
    };
    for case in corpus().into_iter().filter(|c| c["path"] == "non_stream") {
        let raw = input(&case, "input");
        assert_eq!(
            (pair(Format::OpenAI, Format::OpenAI).unwrap().non_stream)(&ctx, &raw).unwrap(),
            raw,
            "{}",
            case["name"]
        );
    }
    for raw in [
        b"not json".as_slice(),
        b"{\"x\":\"\xff\"}",
        br#"{ "n":1.500, "unknown" : "<>&" }"#,
    ] {
        assert_eq!(
            (pair(Format::OpenAI, Format::OpenAI).unwrap().non_stream)(&ctx, raw).unwrap(),
            raw
        );
    }
}

#[test]
fn streaming_chat_payloads_are_opaque_until_the_first_done_marker() {
    let ctx = ResponseCtx {
        model: "m",
        original_request: b"",
        translated_request: b"",
        tool_search: false,
    };
    for case in corpus().into_iter().filter(|c| c["path"] == "stream") {
        let mut stream = go_stream(Format::OpenAI, Format::OpenAI).unwrap()(&ctx);
        let mut terminal = false;
        for line in case["lines"].as_array().unwrap() {
            let encoded = json!({"bytes":case["bytes"],"line":line});
            let raw = input(&encoded, "line");
            let payload = raw.strip_prefix(b"data:").map(<[u8]>::trim_ascii).unwrap_or(&raw);
            let expected = if terminal || payload == b"[DONE]" {
                terminal = true;
                vec![]
            } else {
                vec![payload.to_vec()]
            };
            assert_eq!(stream.line(&raw).unwrap(), expected, "{}: {raw:?}", case["name"]);
        }
    }
}

#[test]
fn a_model_noop_keeps_spacing_unknown_fields_and_numeric_spelling() {
    let ctx = RequestCtx {
        model: "chosen",
        stream: true,
        tool_search: false,
        system_turns: false,
    };
    for raw in [
        br#"{ "model" : "chosen", "unknown":{"x":1.500}, "text":"<>&" }"#.as_slice(),
        b"{\"model\":\"chosen\",\"opaque\":\"\xff\"}",
    ] {
        assert_eq!(
            translate_request(Format::OpenAI, Format::OpenAI, &ctx, raw).unwrap(),
            raw
        );
    }
}
