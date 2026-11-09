use serde::Deserialize;
use serde_json::Value;

use super::*;

/// `ResponsesStreamEvent` as pinned (`codex-api/src/sse/responses.rs:161-186` at
/// 44ccf394573c): the same members, order, types and attributes, decoded with this
/// build's serde_json. The oracle for [`decode`], except for numbers past `f64`, which
/// this build's `Value` refuses and the first-party build (`arbitrary_precision`) keeps.
#[derive(Deserialize)]
#[allow(dead_code)]
struct Pinned {
    #[serde(rename = "type")]
    kind: String,
    headers: Option<Value>,
    metadata: Option<Value>,
    response: Option<Value>,
    response_id: Option<Value>,
    error: Option<Value>,
    item: Option<Value>,
    item_id: Option<String>,
    call_id: Option<String>,
    delta: Option<String>,
    text: Option<String>,
    summary_index: Option<i64>,
    content_index: Option<i64>,
    #[serde(default, deserialize_with = "present")]
    safety_buffering: Option<Value>,
}

fn present<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

const MEMBERS: [(&str, &str); 14] = [
    ("type", r#""response.completed""#),
    ("headers", r#"{"openai-model":"m"}"#),
    ("metadata", r#"{}"#),
    ("response", r#"{"id":"r1"}"#),
    ("response_id", r#""r1""#),
    ("error", r#"{"code":"x"}"#),
    ("item", r#"{"type":"message"}"#),
    ("item_id", r#""i1""#),
    ("call_id", r#""c1""#),
    ("delta", r#""d""#),
    ("text", r#""t""#),
    ("summary_index", "0"),
    ("content_index", "1"),
    ("safety_buffering", "null"),
];

fn nested(depth: usize) -> String {
    format!("{}0{}", "[".repeat(depth), "]".repeat(depth))
}

/// Events at every edge of the decision: repeated known and unknown members in both
/// orders, member types, roots, nesting and surrogate escapes.
fn corpus() -> Vec<String> {
    let mut all: Vec<String> = [
        r#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#,
        r#"{"type":"response.output_text.delta","item_id":"m","content_index":0,"delta":"a\nb"}"#,
        r#"{"type":"response.completed","type":"response.future","response":{"id":"r1"}}"#,
        r#"{"type":"response.future","type":"response.completed","response":{"id":"r1"}}"#,
        r#"{"type":"response.completed","response":{"id":"old"},"response":{"id":"new"}}"#,
        r#"{"type":"response.completed","typ\u0065":"response.future"}"#,
        r#"{"type":"response.completed","respons\u0065":{"id":"r1"}}"#,
        r#"{"type":"response.completed","future":1,"future":{"a":[1,2]},"response":{"id":"r1"}}"#,
        r#"{"type":"response.completed","response":{"id":"old","id":"new","usage":{"total_tokens":1,"total_tokens":2}}}"#,
        r#"{"type":"response.failed","response":{"error":{"code":"a"},"error":{"code":"b"}}}"#,
        r#"{"type":"response.completed","response":null}"#,
        r#"{"type":"response.completed","item_id":null,"summary_index":null}"#,
        r#"{"type":23}"#,
        r#"{"type":null}"#,
        r#"{"type":{"x":1}}"#,
        r#"{"response":{"id":"r1"}}"#,
        r#"{"type":"x","item_id":5}"#,
        r#"{"type":"x","delta":false}"#,
        r#"{"type":"x","summary_index":1.5}"#,
        r#"{"type":"x","summary_index":"1"}"#,
        r#"{"type":"x","content_index":9223372036854775808}"#,
        r#"{"type":"x","content_index":-9223372036854775808}"#,
        r#"{"type":"response.failed","response":{"error":{"code":"server_error"}}"#,
        r#"{"type":"error","error":{"code":"context_length_exceeded"}"#,
        r#"{"type":"response.completed"} {}"#,
        r#"{"type":"response.completed"}x"#,
        " \t\r\n{\"type\":\"response.completed\"}\n ",
        "\u{feff}{\"type\":\"response.completed\"}",
        "",
        "null",
        "[]",
        r#"["response.completed"]"#,
        r#"[{"type":"response.completed"}]"#,
        r#"{"type":"x","response":{"s":"\ud83d\ude00"}}"#,
        r#"{"type":"x","response":{"s":"\ud83d"}}"#,
        r#"{"type":"x","response":{"s":"\ude00"}}"#,
        r#"{"type":"x","response":{"s":"\ud83d\u0041"}}"#,
        r#"{"type":"x","response":{"s":"\ud83d\n"}}"#,
        r#"{"type":"x","response":{"\ud83d":1}}"#,
        r#"{"type":"x","future":"\ud83d"}"#,
        r#"{"type":"x\ud83d"}"#,
        r#"{"type":"x","delta":"\ude00"}"#,
        r#"{"type":"x","response":{"s":"\\ud83d"}}"#,
        r#"{"type":"x","response":{"n":-0.0,"e":1E-7,"big":12345678901234567890}}"#,
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    // Each known member repeated, with the same and a different value.
    for (name, value) in MEMBERS {
        let first = if name == "type" { "" } else { r#""type":"x","# };
        all.push(format!(r#"{{{first}"{name}":{value},"{name}":{value}}}"#));
        all.push(format!(r#"{{{first}"{name}":{value},"{name}":null}}"#));
    }
    // The positional form.
    let values: Vec<&str> = MEMBERS.iter().map(|(_, v)| *v).collect();
    for n in [13, 14] {
        all.push(format!("[{}]", values[..n].join(",")));
    }
    all.push(format!("[{},null]", values.join(",")));
    // Nesting around serde_json's limit, in a known member and an unknown one.
    for depth in 120..=130 {
        for member in ["response", "item", "headers", "future"] {
            all.push(format!(r#"{{"type":"x","{member}":{}}}"#, nested(depth)));
        }
        all.push(format!(r#"{{"type":"x","response":{{"a":{}}}}}"#, nested(depth)));
        all.push(format!(r#"[{},{}]"#, r#""x",null,null"#, nested(depth)));
    }
    all
}

#[test]
fn decode_accepts_exactly_what_the_pinned_event_shape_decodes() {
    for event in corpus() {
        let pinned = serde_json::from_str::<Pinned>(&event).is_ok();
        assert_eq!(decode(&event).is_some(), pinned, "{event}");
    }
}

#[test]
fn decode_reads_the_unique_members_as_sent() {
    let event = decode(r#"{"type":"response.completed","x":1,"x":2,"response":{"id":"a","id":"b","n":1.50}}"#)
        .expect("decodes");
    assert_eq!(event.kind, "response.completed");
    assert_eq!(event.response, Some(r#"{"id":"a","id":"b","n":1.50}"#));
    assert_eq!(event.error, None);
    let event = decode(r#"{"typ\u0065":"error","err\u006fr":{"code":"x"},"response":null}"#).expect("decodes");
    assert_eq!(event.kind, "error");
    assert_eq!(event.error, Some(r#"{"code":"x"}"#));
    assert_eq!(event.response, None);
}

/// The first-party build's `Value` keeps a number of any size (`arbitrary_precision`);
/// this build's would refuse one past `f64`, so the oracle cannot judge it.
#[test]
fn decode_keeps_numbers_of_any_size() {
    for event in [
        r#"{"type":"response.completed","response":{"x":1e400}}"#,
        r#"{"type":"response.completed","response":{"x":-123456789012345678901234567890e999}}"#,
    ] {
        assert!(decode(event).is_some(), "{event}");
    }
}

/// The decision holds at serde_json's exact recursion limit: the oracle itself refuses
/// the next level, so the corpus straddles it.
#[test]
fn the_corpus_straddles_the_recursion_limit() {
    let at = |depth| format!(r#"{{"type":"x","response":{}}}"#, nested(depth));
    assert!(serde_json::from_str::<Pinned>(&at(MAX_DEPTH - 1)).is_ok());
    assert!(serde_json::from_str::<Pinned>(&at(MAX_DEPTH)).is_err());
    assert!(decode(&at(MAX_DEPTH - 1)).is_some());
    assert!(decode(&at(MAX_DEPTH)).is_none());
}
