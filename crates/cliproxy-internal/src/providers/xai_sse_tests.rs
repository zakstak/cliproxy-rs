//! Frames and terminal failures of a buffered xAI body. Cases marked Go are the
//! bodies of v8.0.67 TestXAIExecutorExecuteReturnsTerminalStreamFailures (949b43b3).

use super::*;

fn frames(body: &str) -> Vec<(String, String)> {
    Frames::new(body.as_bytes())
        .map(|f| {
            (
                String::from_utf8(f.event.to_vec()).unwrap(),
                String::from_utf8(f.data.into_owned()).unwrap(),
            )
        })
        .collect()
}

fn pair(event: &str, data: &str) -> (String, String) {
    (event.to_owned(), data.to_owned())
}

#[test]
fn frames_split_on_blank_lines_and_name_their_event() {
    assert_eq!(
        frames("event: response.created\ndata: {\"a\":1}\n\ndata: {\"b\":2}\n\n"),
        vec![pair("response.created", "{\"a\":1}"), pair("", "{\"b\":2}")]
    );
}

#[test]
fn crlf_frames_match_lf_frames() {
    let lf = "event: e\ndata: {\"a\":\ndata: 1}\n\ndata: [DONE]\n\n";
    assert_eq!(frames(&lf.replace('\n', "\r\n")), frames(lf));
    assert_eq!(frames(lf), vec![pair("e", "{\"a\":\n1}"), pair("", "[DONE]")]);
}

#[test]
fn multiline_data_joins_until_the_value_closes() {
    // Go: "multiline sse data error event".
    let body = "event: error\ndata: {\"type\":\"error\",\ndata: \"error\":{\"code\":\"invalid_argument\"}}\n\n";
    assert_eq!(
        frames(body),
        vec![pair(
            "error",
            "{\"type\":\"error\",\n\"error\":{\"code\":\"invalid_argument\"}}"
        )]
    );
}

/// Without blank lines, each complete data line is its own event (the v8.0.35 line
/// reading); an incomplete one collects the following lines.
#[test]
fn complete_data_lines_need_no_blank_line() {
    assert_eq!(
        frames("data: {\"a\":1}\ndata: {\"b\":\ndata: 2}\ndata: {\"c\":3}"),
        vec![pair("", "{\"a\":1}"), pair("", "{\"b\":\n2}"), pair("", "{\"c\":3}")]
    );
}

#[test]
fn an_event_line_after_data_names_that_data() {
    // Go: "data line before event error header".
    assert_eq!(
        frames("data: {\"code\":\"invalid_argument\"}\nevent: error\n\n"),
        vec![pair("error", "{\"code\":\"invalid_argument\"}")]
    );
    // A second name closes the named frame before it.
    assert_eq!(
        frames("event: a\ndata: x\nevent: b\ndata: y\n"),
        vec![pair("a", "x"), pair("b", "y")]
    );
}

#[test]
fn comments_other_fields_and_empty_bodies_carry_no_frame() {
    assert!(frames("").is_empty());
    assert!(frames(": keep-alive\n\n").is_empty());
    assert!(frames("id: 1\nretry: 10\n\n\n").is_empty());
    assert!(frames("event: error\n\n").is_empty());
}

#[test]
fn an_unterminated_final_frame_is_still_read() {
    assert_eq!(
        frames("data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\""),
        vec![pair("", "{\"type\":\"response.completed\",\"response\":{\"id\":\"r\"")]
    );
}

/// A string holding brackets and escaped quotes does not close the held value early:
/// counted, the `}` of line one would settle the value as invalid and swallow `{"c":2}`.
#[test]
fn nesting_ignores_brackets_inside_strings() {
    assert_eq!(
        frames("data: {\"a\":\"}\\\"\"\ndata: ,\"b\":\ndata: 1}\ndata: {\"c\":2}\n"),
        vec![pair("", "{\"a\":\"}\\\"\"\n,\"b\":\n1}"), pair("", "{\"c\":2}")]
    );
}

/// Held data is validated once per data line or once when its outer value closes, so
/// a body of many short continuation lines frames in linear time.
#[test]
fn many_continuation_lines_frame_in_one_event() {
    let mut body = String::from("data: {\"x\":[\n");
    for _ in 0..100_000 {
        body.push_str("data: 1,\n");
    }
    body.push_str("data: 1]}\n\n");
    let got: Vec<_> = Frames::new(body.as_bytes()).collect();
    assert_eq!(got.len(), 1);
    assert!(gj::valid(&got[0].data));
}

fn failure(event: &str, data: &str) -> Option<(u16, serde_json::Value)> {
    terminal_failure(event.as_bytes(), data.as_bytes()).map(|(s, doc)| (s, serde_json::from_slice(&doc).unwrap()))
}

#[test]
fn error_events_map_to_their_status() {
    let cases = [
        // Go: "error event with invalid_request_error".
        (
            "",
            r#"{"type":"error","error":{"type":"invalid_request_error","code":"invalid_value","message":"Invalid input."},"sequence_number":2}"#,
            400,
            serde_json::json!({"error":{"type":"invalid_request_error","code":"invalid_value","message":"Invalid input."},"sequence_number":2}),
        ),
        // Go: "error event with bad-credentials remaps to 401" (status_error remaps 403).
        (
            "",
            r#"{"type":"error","status":403,"error":{"code":"unauthenticated:bad-credentials","message":"The OAuth2 access token could not be validated."}}"#,
            403,
            serde_json::json!({"error":{"code":"unauthenticated:bad-credentials","message":"The OAuth2 access token could not be validated."}}),
        ),
        // Go: "event error header with flat xai refusal payload".
        (
            "error",
            r#"{"code":"permission-denied","error":"I'm sorry, I can't help with that request."}"#,
            403,
            serde_json::json!({"error":{"message":"I'm sorry, I can't help with that request.","code":"permission-denied"}}),
        ),
        // Go: "data line before event error header".
        (
            "error",
            r#"{"code":"invalid_argument","message":"Invalid input."}"#,
            400,
            serde_json::json!({"error":{"message":"Invalid input.","code":"invalid_argument"}}),
        ),
        // Go: "response.failed event with server_error".
        (
            "",
            r#"{"type":"response.failed","sequence_number":3,"response":{"id":"resp_1","status":"failed","error":{"type":"server_error","code":"server_error","message":"Upstream generation failed."}}}"#,
            502,
            serde_json::json!({"error":{"type":"server_error","code":"server_error","message":"Upstream generation failed."},"sequence_number":3}),
        ),
        // Go: "plain text data under event error header".
        (
            "error",
            "upstream gateway error",
            502,
            serde_json::json!({"error":{"message":"upstream gateway error"}}),
        ),
        // Go: "non-sse json error body on 200".
        (
            "",
            r#"{"code":"permission-denied","error":"Request blocked."}"#,
            403,
            serde_json::json!({"error":{"message":"Request blocked.","code":"permission-denied"}}),
        ),
        (
            "",
            r#"{"error":{"message":"Upstream overloaded","type":"server_error","code":"server_is_overloaded"}}"#,
            503,
            serde_json::json!({"error":{"message":"Upstream overloaded","type":"server_error","code":"server_is_overloaded"}}),
        ),
        (
            "",
            r#"{"type":"error","error":{"code":"subscription:free-usage-exhausted","message":"You've used all the included free usage."}}"#,
            429,
            serde_json::json!({"error":{"code":"subscription:free-usage-exhausted","message":"You've used all the included free usage."}}),
        ),
        (
            "response.failed",
            r#"{"response":{"status":"failed"}}"#,
            502,
            serde_json::json!({"error":{"message":"upstream stream failed without error details"}}),
        ),
        (
            "",
            r#"{"type":"error","error":{"code":"404"}}"#,
            404,
            serde_json::json!({"error":{"code":"404","message":"404"}}),
        ),
    ];
    for (event, data, status, doc) in cases {
        assert_eq!(failure(event, data), Some((status, doc)), "{event:?} {data}");
    }
}

#[test]
fn ordinary_events_are_no_failure() {
    for (event, data) in [
        ("", r#"{"type":"response.created","response":{"id":"r"}}"#),
        ("", r#"{"type":"response.completed","response":{"error":null}}"#),
        // An `error` member beside `output` is a response, not an error body.
        ("", r#"{"error":{"message":"x"},"output":[]}"#),
        ("", r#"{"error":null}"#),
        ("", r#"{"error":""}"#),
        ("", "not json"),
        ("message", "not json"),
        ("error", ""),
    ] {
        assert_eq!(failure(event, data), None, "{event:?} {data}");
    }
}
