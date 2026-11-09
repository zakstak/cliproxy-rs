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

/// What the first-party build's `Value` (serde_json 1.0.149 with `arbitrary_precision`
/// and `raw_value`, `value/de.rs` `KeyClassifier`) does with an object whose first key is
/// a private token, at each position a known member's value can hold one. This build's
/// `Value` lacks `arbitrary_precision`, so the in-crate oracle cannot judge these; the
/// expectations were checked against that serde_json (event-safety-completion evidence,
/// `oracle-extra`).
const PRIVATE_TOKENS: [(&str, bool); 30] = [
    // A number written as a string: exactly one JSON number, and nothing else.
    (r#"{"$serde_json::private::Number":"1"}"#, true),
    (r#"{"$serde_json::private::Number":"-0"}"#, true),
    (r#"{"$serde_json::private::Number":"-1.5e+3"}"#, true),
    (r#"{"$serde_json::private::Number":"1E400"}"#, true),
    (
        r#"{"$serde_json::private::Number":"123456789012345678901234567890"}"#,
        true,
    ),
    (r#"{ "$serde_json::private::Number" : "2" }"#, true),
    (r#"{"$serde_json::private::Numbe\u0072":"3"}"#, true),
    (r#"{"$serde_json::private::Number":"bad"}"#, false),
    (r#"{"$serde_json::private::Number":12}"#, false),
    (r#"{"$serde_json::private::Number":null}"#, false),
    (r#"{"$serde_json::private::Number":""}"#, false),
    (r#"{"$serde_json::private::Number":" 1"}"#, false),
    (r#"{"$serde_json::private::Number":"1 "}"#, false),
    (r#"{"$serde_json::private::Number":"01"}"#, false),
    (r#"{"$serde_json::private::Number":"1."}"#, false),
    (r#"{"$serde_json::private::Number":".5"}"#, false),
    (r#"{"$serde_json::private::Number":"+1"}"#, false),
    (r#"{"$serde_json::private::Number":"1e"}"#, false),
    (r#"{"$serde_json::private::Number":"-"}"#, false),
    (r#"{"$serde_json::private::Number":"1","x":3}"#, false),
    (r#"{"$serde_json::private::Number":"\ud83d"}"#, false),
    // JSON text written as a string: valid JSON that decodes as a `Value` itself.
    (r#"{"$serde_json::private::RawValue":"{\"a\":[1e400]}"}"#, true),
    (r#"{"$serde_json::private::RawValue":" 7 "}"#, true),
    (
        r#"{"$serde_json::private::RawValue":"{\"$serde_json::private::Number\":\"4\"}"}"#,
        true,
    ),
    (r#"{"$serde_json::private::RawValue":"{"}"#, false),
    (r#"{"$serde_json::private::RawValue":"1 2"}"#, false),
    (r#"{"$serde_json::private::RawValue":{}}"#, false),
    (
        r#"{"$serde_json::private::RawValue":"{\"$serde_json::private::Number\":\"x\"}"}"#,
        false,
    ),
    (r#"{"$serde_json::private::RawValue":"\"\\ud83d\""}"#, false),
    (r#"{"$serde_json::private::RawValue":"1","x":3}"#, false),
];

#[test]
fn private_tokens_decode_as_the_first_party_value_does() {
    for (object, accepted) in PRIVATE_TOKENS {
        for event in [
            format!(r#"{{"type":"x","response":{object}}}"#),
            format!(r#"{{"type":"x","error":{{"a":[{object}]}}}}"#),
            format!(r#"{{"type":"x","safety_buffering":[0,{{"b":{object}}}]}}"#),
            // The positional form, `metadata` third, every member without a default.
            format!(r#"["x",null,{object},null,null,null,null,null,null,null,null,null,null]"#),
        ] {
            assert_eq!(decode(&event).is_some(), accepted, "{event}");
        }
    }
    // Only the first key is read that way, and only inside a known member.
    for event in [
        r#"{"type":"x","response":{"id":"r","$serde_json::private::Number":"bad"}}"#,
        r#"{"type":"x","future":{"$serde_json::private::Number":"bad"}}"#,
        r#"{"type":"x","$serde_json::private::Number":"bad"}"#,
        r#"{"$serde_json::private::Number":"bad","type":"x"}"#,
        r#"{"type":"x","response":{"s":"{\"$serde_json::private::Number\":\"bad\"}"}}"#,
    ] {
        assert!(decode(event).is_some(), "{event}");
    }
}

/// A private token's object is one level of nesting; the JSON text of a raw-value token
/// starts a fresh budget.
#[test]
fn private_tokens_count_toward_nesting_as_the_pinned_value_does() {
    let number = r#"{"$serde_json::private::Number":"1"}"#;
    let at = |depth: usize, inner: &str| {
        format!(
            r#"{{"type":"x","response":{}{inner}{}}}"#,
            "[".repeat(depth),
            "]".repeat(depth)
        )
    };
    assert!(decode(&at(MAX_DEPTH - 2, number)).is_some());
    assert!(decode(&at(MAX_DEPTH - 1, number)).is_none());
    let deep = format!("{}0{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
    let raw = format!(r#"{{"$serde_json::private::RawValue":"{deep}"}}"#);
    assert!(decode(&format!(r#"{{"type":"x","response":{raw}}}"#)).is_some());
    let deeper = format!("{}0{}", "[".repeat(MAX_DEPTH + 1), "]".repeat(MAX_DEPTH + 1));
    let raw = format!(r#"{{"$serde_json::private::RawValue":"{deeper}"}}"#);
    assert!(decode(&format!(r#"{{"type":"x","response":{raw}}}"#)).is_none());
}

/// `WrappedWebsocketErrorEvent` as pinned (`endpoint/responses_websocket.rs:684-709`):
/// the in-crate oracle for [`wrapped`]'s shape decision, numbers and private tokens aside.
#[derive(Deserialize)]
#[allow(dead_code)]
struct PinnedWrapped {
    #[serde(rename = "type")]
    kind: String,
    #[serde(alias = "status_code")]
    status: Option<u16>,
    #[serde(default)]
    error: Option<PinnedWrappedError>,
    #[serde(default)]
    headers: Option<serde_json::Map<String, Value>>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct PinnedWrappedError {
    code: Option<String>,
    message: Option<String>,
    #[serde(default)]
    headers: Option<Value>,
}

/// The pinned mapping (`map_wrapped_websocket_error_event`, lines 712-764): a recognised
/// code is retryable first; then a status `http::StatusCode` accepts and that is not a
/// success is an HTTP error; anything else maps to nothing.
#[test]
fn wrapped_frames_map_as_the_pinned_reader_maps_them() {
    use Wrapped::*;
    let cases = [
        (
            r#"{"type":"error","status":400,"error":{"code":"x"}}"#,
            Some(Status(400)),
        ),
        (r#"{"type":"error","status_code":429}"#, Some(Status(429))),
        (r#"{"type":"error","status":100}"#, Some(Status(100))),
        (r#"{"type":"error","status":199}"#, Some(Status(199))),
        (r#"{"type":"error","status":300}"#, Some(Status(300))),
        (r#"{"type":"error","status":999}"#, Some(Status(999))),
        (
            r#"{"type":"error","status":200,"error":{"code":"other"}}"#,
            Some(Notice),
        ),
        (r#"{"type":"error","status":299}"#, Some(Notice)),
        (r#"{"type":"error","status":99}"#, Some(Notice)),
        (r#"{"type":"error","status":0}"#, Some(Notice)),
        (r#"{"type":"error","status":1000}"#, Some(Notice)),
        (r#"{"type":"error","status":65535}"#, Some(Notice)),
        (r#"{"type":"error","error":{"code":"other"}}"#, Some(Statusless)),
        (r#"{"type":"error","status":null}"#, Some(Statusless)),
        (
            r#"{"type":"error","error":{"code":"previous_response_not_found"}}"#,
            Some(Retryable(None)),
        ),
        (
            r#"{"type":"error","status":200,"error":{"code":"websocket_connection_limit_reached"}}"#,
            Some(Retryable(None)),
        ),
        (
            r#"{"type":"error","status":429,"error":{"code":"websocket_connection_limit_reached"}}"#,
            Some(Retryable(Some(429))),
        ),
        (r#"{"type":"error","item_id":23,"status":503}"#, Some(Status(503))),
        // Not the pinned shape.
        (r#"{"type":"error","status":65536}"#, None),
        (r#"{"type":"error","status":-1}"#, None),
        (r#"{"type":"error","status":400.0}"#, None),
        (r#"{"type":"error","status":"400"}"#, None),
        (r#"{"type":"error","status":400,"status_code":400}"#, None),
        (r#"{"type":"error","status":400,"status":429}"#, None),
        (r#"{"type":"error","type":"error","status":400}"#, None),
        (r#"{"type":"response.failed","status":400}"#, None),
        (r#"{"type":"error","status":400,"error":"boom"}"#, None),
        (r#"{"type":"error","status":400,"error":{"code":5}}"#, None),
        (r#"{"type":"error","status":400,"error":{"code":"a","code":"b"}}"#, None),
        (r#"{"type":"error","status":400,"headers":[]}"#, None),
        (r#"{"type":"error","status":400,"headers":"x"}"#, None),
        (r#"{"status":400}"#, None),
    ];
    for (frame, want) in cases {
        assert_eq!(wrapped(frame), want, "{frame}");
        let pinned = serde_json::from_str::<PinnedWrapped>(frame).is_ok_and(|w| w.kind == "error");
        assert_eq!(wrapped(frame).is_some(), pinned, "{frame}");
    }
}

/// The top-level `headers` is a `Map`, whose own keys are plain and whose values sit at
/// the second level; `error.headers` is a `Value` at the third.
#[test]
fn wrapped_header_values_decode_at_their_own_level() {
    let nest = |depth: usize| format!("{}0{}", "[".repeat(depth), "]".repeat(depth));
    let top = |depth| format!(r#"{{"type":"error","status":400,"headers":{{"x":{}}}}}"#, nest(depth));
    let inner = |depth| {
        format!(
            r#"{{"type":"error","status":400,"error":{{"code":"x","headers":{{"x":{}}}}}}}"#,
            nest(depth)
        )
    };
    for (frame, accepted) in [
        (top(MAX_DEPTH - 2), true),
        (top(MAX_DEPTH - 1), false),
        (inner(MAX_DEPTH - 3), true),
        (inner(MAX_DEPTH - 2), false),
    ] {
        let pinned = serde_json::from_str::<PinnedWrapped>(&frame).is_ok();
        assert_eq!(pinned, accepted, "oracle: {frame}");
        assert_eq!(wrapped(&frame).is_some(), accepted, "{frame}");
    }
    let marker = r#"{"$serde_json::private::Number":"bad"}"#;
    // The map's own first key is a plain key; a `Value` inside it is read as a `Value`.
    assert!(wrapped(&format!(r#"{{"type":"error","status":400,"headers":{marker}}}"#)).is_some());
    assert!(
        wrapped(&format!(
            r#"{{"type":"error","status":400,"headers":{{"x":{marker}}}}}"#
        ))
        .is_none()
    );
    assert!(
        wrapped(&format!(
            r#"{{"type":"error","status":400,"error":{{"headers":{marker}}}}}"#
        ))
        .is_none()
    );
}

#[test]
fn last_wins_keeps_the_last_of_each_repeated_key() {
    for (payload, view) in [
        (
            r#"{"type":"response.failed","response":{"error":{"type":"a","type":"b","code":"c"}}}"#,
            r#"{"type":"response.failed","response":{"error":{"type":"b","code":"c"}}}"#,
        ),
        (r#"{"a":1,"b":2,"a":3}"#, r#"{"b":2,"a":3}"#),
        (r#"{"a":1,"a":2,"a":3}"#, r#"{"a":3}"#),
        (r#"{ "a" : 1 , "a" : {"x":1,"x":2} }"#, r#"{ "a" : {"x":2} }"#),
        (r#"{"a":{"x":1,"x":2},"a":5}"#, r#"{"a":5}"#),
        (r#"{"typ\u0065":"x","type":"error"}"#, r#"{"type":"error"}"#),
        (r#"[{"k":1,"k":2},{"k":3}]"#, r#"[{"k":2},{"k":3}]"#),
        (
            r#"{"s":"\"a\":1,\"a\":2","n":1.50,"n":1e3}"#,
            r#"{"s":"\"a\":1,\"a\":2","n":1e3}"#,
        ),
    ] {
        let got = pinned_view(payload);
        assert!(matches!(got, Cow::Owned(_)), "{payload}");
        assert_eq!(got, view, "{payload}");
        let pinned: Value = serde_json::from_str(payload).unwrap();
        let read: Value = serde_json::from_str(&got).unwrap();
        assert_eq!(read, pinned, "{payload}");
    }
    for unchanged in [
        r#"{"a":1,"b":{"a":2}}"#,
        r#"{"type":"error","error":{"code":"x"}}"#,
        r#"{"a":1,"a":2"#,
        "not json",
        "",
    ] {
        assert!(
            matches!(pinned_view(unchanged), Cow::Borrowed(s) if s == unchanged),
            "{unchanged}"
        );
    }
}

#[test]
fn member_reads_the_last_key_as_value_get_does() {
    assert_eq!(member(r#"{"id":"a","x":1,"id":"b"}"#, "id"), Some(r#""b""#));
    assert_eq!(member(r#"{"i\u0064":"a"}"#, "id"), Some(r#""a""#));
    assert_eq!(member(r#"{"x":{"id":"a"}}"#, "id"), None);
    assert_eq!(member(r#"["id"]"#, "id"), None);
    assert_eq!(member("null", "id"), None);
}

/// `ResponseCompleted` (`sse/responses.rs:109-159` at 44ccf394573c) as the pinned
/// reader reads a `response`: a derived struct, from the first-party `Value`
/// (`arbitrary_precision`, `raw_value`), with the last of a repeated key.
#[test]
fn completed_reads_as_the_pinned_struct_does() {
    let usage = |u: &str| format!(r#"{{"id":"r","usage":{u}}}"#);
    let ok = [
        r#"{"id":"r"}"#.to_owned(),
        r#"{"id":"r","usage":null,"usage_metadata":null,"end_turn":null,"extra":[1]}"#.to_owned(),
        r#"{"id":5,"id":"r"}"#.to_owned(),
        r#"["r",null,null]"#.to_owned(),
        r#"["r",{"input_tokens":1,"output_tokens":2,"total_tokens":3},{"amount":"a"},false]"#.to_owned(),
        r#"{"id":{"$serde_json::private::RawValue":"\"r\""}}"#.to_owned(),
        r#"{"id":"r","usage":{"$serde_json::private::RawValue":"null"}}"#.to_owned(),
        r#"{"id":"r","usage_metadata":{"amount":"0.1","metadata":{"a":[1e400]}},"end_turn":true}"#.to_owned(),
        r#"{"id":"r","usage_metadata":{}}"#.to_owned(),
        usage(r#"{"input_tokens":1,"output_tokens":2,"total_tokens":3}"#),
        usage(r#"{"input_tokens":-0,"output_tokens":0,"total_tokens":9223372036854775807}"#),
        usage(
            r#"{"input_tokens":1,"input_tokens_details":{"cached_tokens":1},"output_tokens":2,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":3}"#,
        ),
        usage(
            r#"{"input_tokens":1,"input_tokens_details":{"cached_tokens":1,"cache_write_tokens":2},"output_tokens":2,"total_tokens":3}"#,
        ),
        usage(
            r#"{"input_tokens":1,"input_tokens_details":null,"output_tokens":2,"output_tokens_details":null,"total_tokens":3}"#,
        ),
        usage(r#"{"input_tokens":1,"output_tokens":2,"total_tokens":3,"codex_rollout_budget_units":1e400}"#),
        usage(r#"{"input_tokens":1,"output_tokens":2,"total_tokens":3,"codex_rollout_budget_units":-1.5e-3}"#),
        usage(r#"{"input_tokens":1,"output_tokens":2,"total_tokens":3,"codex_rollout_budget_units":null}"#),
        usage(r#"{"input_tokens":{"$serde_json::private::Number":"5"},"output_tokens":2,"total_tokens":3}"#),
        usage(r#"{"input_tokens":"x","input_tokens":1,"output_tokens":2,"total_tokens":3}"#),
        usage(r#"[1,null,2,null,3]"#),
    ];
    let unreadable = [
        r#"{}"#.to_owned(),
        r#"{"id":5}"#.to_owned(),
        r#"{"id":null}"#.to_owned(),
        r#"{"id":"r","id":5}"#.to_owned(),
        r#"[]"#.to_owned(),
        r#"["r"]"#.to_owned(),
        r#"["r",null,null,null,1]"#.to_owned(),
        r#""r""#.to_owned(),
        r#"5"#.to_owned(),
        r#"{"id":{"$serde_json::private::Number":"5"}}"#.to_owned(),
        r#"{"id":"r","usage_metadata":{"amount":5}}"#.to_owned(),
        r#"{"id":"r","usage_metadata":[]}"#.to_owned(),
        r#"{"id":"r","end_turn":"yes"}"#.to_owned(),
        r#"{"id":"r","end_turn":1}"#.to_owned(),
        usage(r#"{}"#),
        usage(r#"[]"#),
        usage(r#""x""#),
        usage(r#"{"input_tokens":"1","output_tokens":2,"total_tokens":3}"#),
        usage(r#"{"input_tokens":1.0,"output_tokens":2,"total_tokens":3}"#),
        usage(r#"{"input_tokens":1e2,"output_tokens":2,"total_tokens":3}"#),
        usage(r#"{"input_tokens":9223372036854775808,"output_tokens":2,"total_tokens":3}"#),
        usage(r#"{"input_tokens":1,"output_tokens":2}"#),
        usage(r#"{"input_tokens":null,"output_tokens":2,"total_tokens":3}"#),
        usage(r#"{"input_tokens":1,"input_tokens_details":{},"output_tokens":2,"total_tokens":3}"#),
        usage(r#"{"input_tokens":1,"input_tokens_details":{"cached_tokens":null},"output_tokens":2,"total_tokens":3}"#),
        usage(
            r#"{"input_tokens":1,"input_tokens_details":{"cached_tokens":1,"cache_write_tokens":null},"output_tokens":2,"total_tokens":3}"#,
        ),
        usage(r#"{"input_tokens":1,"output_tokens":2,"output_tokens_details":{},"total_tokens":3}"#),
        usage(r#"{"input_tokens":1,"output_tokens":2,"total_tokens":3,"codex_rollout_budget_units":"1"}"#),
        usage(r#"{"input_tokens":{"$serde_json::private::Number":"5.0"},"output_tokens":2,"total_tokens":3}"#),
        usage(r#"{"input_tokens":1,"input_tokens":"x","output_tokens":2,"total_tokens":3}"#),
    ];
    for response in ok {
        let event = format!(r#"{{"type":"response.completed","response":{response}}}"#);
        assert!(decode(&event).is_some(), "{event}");
        assert!(completes(&response), "{response}");
    }
    for response in unreadable {
        let event = format!(r#"{{"type":"response.completed","response":{response}}}"#);
        assert!(decode(&event).is_some(), "{event}");
        assert!(!completes(&response), "{response}");
    }
}

#[test]
fn outcome_and_accounted_follow_the_pinned_reader() {
    use Outcome::*;
    for (event, outcome, accounted) in [
        (r#"{"type":"response.completed","response":{"id":"r"}}"#, Ends, true),
        (
            r#"{"type":"response.completed","response":{"status":"completed"}}"#,
            Recorded,
            true,
        ),
        (r#"{"type":"response.completed","response":[]}"#, Recorded, true),
        (r#"{"type":"response.completed","response":null}"#, Continues, false),
        (r#"{"type":"response.completed"}"#, Continues, false),
        (r#"{"type":"response.incomplete"}"#, Ends, true),
        (r#"{"type":"response.incomplete","response":null}"#, Ends, true),
        (
            r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":"x"}}}"#,
            Ends,
            true,
        ),
        (
            r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":5}}}"#,
            Ends,
            true,
        ),
        (
            r#"{"type":"response.incomplete","response":{"incomplete_details":null}}"#,
            Ends,
            true,
        ),
        (
            r#"{"type":"response.incomplete","response":[{"incomplete_details":{"reason":"content_filter"}}]}"#,
            Ends,
            true,
        ),
        (
            r#"{"type":"response.incomplete","response":{"id":"r","incomplete_details":{"reason":"interrupted"}}}"#,
            Ends,
            true,
        ),
        (
            r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"interrupted"}}}"#,
            Recorded,
            true,
        ),
        (
            r#"{"type":"response.incomplete","response":{"id":"r","incomplete_details":{"reason":"content_filter"}}}"#,
            Recorded,
            true,
        ),
        (
            r#"{"type":"response.incomplete","response":{"id":"r","incomplete_details":{"reason":"content<BS>u005ffilter"}}}"#,
            Recorded,
            true,
        ),
        (
            r#"{"type":"response.incomplete","response":{"id":"r","incomplete_details":{"reason":"content_filter","reason":"interrupted"}}}"#,
            Ends,
            true,
        ),
        (
            r#"{"type":"response.incomplete","response":{"id":"r","incomplete_details":{"reason":"interrupted"},"incomplete_details":{"reason":"content_filter"}}}"#,
            Recorded,
            true,
        ),
        (
            r#"{"type":"response.incomplete","response":{"id":"r","incomplete_details":{"$serde_json::private::RawValue":"{\"reason\":\"content_filter\"}"}}}"#,
            Recorded,
            true,
        ),
        (r#"{"type":"response.done","response":{"id":"r"}}"#, Continues, false),
        (r#"{"type":"response.failed","response":{"id":"r"}}"#, Continues, true),
        (r#"{"type":"response.output_text.delta","delta":"x"}"#, Continues, true),
    ] {
        let event = event.replace("<BS>", "\\");
        let decoded = decode(&event).expect("decodes");
        assert_eq!(decoded.outcome(), outcome, "{event}");
        assert_eq!(decoded.outcome().ends_turn(), outcome != Continues, "{event}");
        assert_eq!(decoded.accounted(), accounted, "{event}");
    }
    let event = decode(r#"{"type":"response.completed","response":{"id":"a","id":"b"}}"#).unwrap();
    assert_eq!(event.response_id().as_deref(), Some("b"));
    let event = decode(r#"{"type":"response.completed","response":{"id":5}}"#).unwrap();
    assert_eq!(event.response_id(), None);
}

/// The classification view resolves serde_json's private values as the first-party
/// `Value` reads them (`value/de.rs` `visit_map` at serde_json 1.0.149 with
/// `arbitrary_precision` and `raw_value`): an object whose first key is the private
/// number token is that number, digits as sent; one whose first key is the private raw
/// value token is the JSON text it holds, read in turn (nested raw values, private
/// numbers, and the last of a repeated key inside it, root included). It also keeps the
/// last of a repeated key around them. What the pinned `Value` would not read that way
/// stays as sent: the event's own first key (the event is a struct), a token object with
/// another member, a token that is not the first key, an invalid number or JSON text, and
/// the token as a string.
#[test]
fn pinned_view_resolves_private_values_as_the_first_party_value_does() {
    for (payload, view) in [
        (
            r#"{"type":"response.failed","response":{"id":"r1","error":{"$serde_json::private::RawValue":"{\"type\":\"rate_limit_error\",\"token\":\"at-FAKE\"}"}}}"#,
            r#"{"type":"response.failed","response":{"id":"r1","error":{"type":"rate_limit_error","token":"at-FAKE"}}}"#,
        ),
        (
            r#"{"type":"error","error":{"status":{"$serde_json::private::Number":"429"},"n":{"$serde_json::private::Number":"-0"},"e":{"$serde_json::private::Number":"1e400"},"f":{ "$serde_json::private::Number" : "1.50" }}}"#,
            r#"{"type":"error","error":{"status":429,"n":-0,"e":1e400,"f":1.50}}"#,
        ),
        (
            r#"{"type":"x","error":{"$serde_json::private::RawValue":"{\"$serde_json::private::RawValue\":\"{\\\"code\\\":\\\"c\\\"}\"}"}}"#,
            r#"{"type":"x","error":{"code":"c"}}"#,
        ),
        (
            r#"{"type":"x","error":{"$serde_json::private::RawValue":"{\"type\":\"a\",\"type\":\"b\"}"}}"#,
            r#"{"type":"x","error":{"type":"b"}}"#,
        ),
        (
            r#"{"type":"x","error":{"$serde_json::private::RawValue":"{\"$serde_json::private::Number\":\"7\"}"}}"#,
            r#"{"type":"x","error":7}"#,
        ),
        (
            r#"{"type":"x","response":{"id":{"$serde_json::private::RawValue":"\"r1\""},"u":{"$serde_json::private::RawValue":" null "},"a":[{"$serde_json::private::Number":"1"},{"$serde_json::private::RawValue":"[1,{\"k\":1,\"k\":2}]"}]}}"#,
            r#"{"type":"x","response":{"id":"r1","u":null,"a":[1,[1,{"k":2}]]}}"#,
        ),
        (
            r#"{"type":"x","error":{"$serde_json::private::RawValue":"{\"type\":\"rate_limit_error\"}"},"error":{"code":"other"}}"#,
            r#"{"type":"x","error":{"code":"other"}}"#,
        ),
        (
            r#"{"type":"x","error":{"code":"other"},"error":{"$serde_json::private::RawValue":"{\"type\":\"rate_limit_error\"}"}}"#,
            r#"{"type":"x","error":{"type":"rate_limit_error"}}"#,
        ),
        (
            r#"{"type":"x","error":{"$serde_json::private::\u0052awValue":"{\"code\":\"c\"}"}}"#,
            r#"{"type":"x","error":{"code":"c"}}"#,
        ),
    ] {
        // A repeated `error` is a repeated known member: such a frame does not decode, and
        // its view is what the account check of an undecodable frame reads.
        assert_eq!(
            decode(payload).is_some(),
            payload.matches(r#""error":"#).count() < 2,
            "{payload}"
        );
        let got = pinned_view(payload);
        assert!(matches!(got, Cow::Owned(_)), "{payload}");
        assert_eq!(got, view, "{payload}");
        // Without the repeated-key cut only the private values change: every one is
        // resolved, and cutting afterwards gives the same view.
        let resolved = private_values_resolved(payload);
        assert!(!resolved.contains("private::"), "{payload}: {resolved}");
        assert_eq!(pinned_view(&resolved), view, "{payload}");
    }
    for unchanged in [
        r#"{"$serde_json::private::Number":"5","type":"x"}"#,
        r#"{"type":"x","error":{"$serde_json::private::Number":"5","y":1}}"#,
        r#"{"type":"x","error":{"y":1,"$serde_json::private::Number":"5"}}"#,
        r#"{"type":"x","error":{"$serde_json::private::Number":"05"}}"#,
        r#"{"type":"x","error":{"$serde_json::private::RawValue":"{"}}"#,
        r#"{"type":"x","error":"$serde_json::private::Number"}"#,
    ] {
        assert!(
            matches!(pinned_view(unchanged), Cow::Borrowed(s) if s == unchanged),
            "{unchanged}"
        );
    }
}

/// A response is named by the `response.id` the pinned `Value` reads: the last of a
/// repeated `id`, through a private raw value of the `id` or of the whole `response`.
#[test]
fn response_ids_read_as_the_pinned_value_reads_them() {
    for (event, id) in [
        (
            r#"{"type":"response.completed","response":{"id":"stale","id":"r1"}}"#,
            Some("r1"),
        ),
        (
            r#"{"type":"response.completed","response":{"id":"r1","id":"stale"}}"#,
            Some("stale"),
        ),
        (
            r#"{"type":"response.completed","response":{"id":{"$serde_json::private::RawValue":"\"r1\""}}}"#,
            Some("r1"),
        ),
        (
            r#"{"type":"response.completed","response":{"$serde_json::private::RawValue":"{\"id\":\"a\",\"id\":\"r1\"}"}}"#,
            Some("r1"),
        ),
        (
            r#"{"type":"response.completed","response":{"id":{"$serde_json::private::Number":"5"}}}"#,
            None,
        ),
    ] {
        let event = decode(event).expect("decodes");
        assert_eq!(event.response_id().as_deref(), id);
    }
    let created = decode(
        r#"{"type":"response.created","response":{"id":"r2","previous_response_id":"a","previous_response_id":"r1"}}"#,
    )
    .unwrap();
    assert_eq!(created.response_str("previous_response_id").as_deref(), Some("r1"));
}
