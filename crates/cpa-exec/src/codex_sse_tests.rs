//! Go `helps/codex_sse_scanner_test.go` at v8.0.35, case by case.

use super::*;

/// The lines Go's `Scan` delivers for `stream`: physical lines split as `bufio.ScanLines`
/// splits them (a trailing CR dropped, a final unterminated line kept).
fn scan(stream: &str, max: usize) -> Vec<String> {
    let mut lines: Vec<&[u8]> = stream.as_bytes().split(|b| *b == b'\n').collect();
    if stream.ends_with('\n') {
        lines.pop();
    }
    let mut scanner = DataLines::new(max);
    let mut out = Vec::new();
    for line in lines {
        scanner.push(line.strip_suffix(b"\r").unwrap_or(line), &mut out);
    }
    scanner.finish(&mut out);
    out.iter()
        .map(|l| String::from_utf8(l.line.to_vec()).unwrap())
        .collect()
}

/// `TestCodexSSEScannerAssemblesEventData`.
#[test]
fn assembles_event_data() {
    let marker = "body-bytes-must-not-be-logged";
    let cases: Vec<(&str, String, Vec<String>)> = vec![
        (
            "one-line events pass through as read",
            "event: response.created\ndata: {\"type\":\"response.created\"}\n\n: keep-alive\n\ndata: {\"type\":\"x\",\"v\":\"a b\"}\n\n".into(),
            vec!["event: response.created".into(), r#"data: {"type":"response.created"}"#.into(), "".into(), ": keep-alive".into(), "".into(), r#"data: {"type":"x","v":"a b"}"#.into(), "".into()],
        ),
        (
            "CRLF line endings",
            "event: a\r\ndata: {\"type\":\"a\"}\r\n\r\n".into(),
            vec!["event: a".into(), r#"data: {"type":"a"}"#.into(), "".into()],
        ),
        (
            // Go trims a data value with bytes.TrimSpace, which strips Unicode spaces.
            "a value after a Unicode space starts an event",
            "data:\u{a0}{\"type\":\"response.completed\",\ndata: \"response\":{\"output\":[]}}\n\n".into(),
            vec!["data:\u{a0}{\"type\":\"response.completed\",\ndata: \"response\":{\"output\":[]}}".into(), "".into()],
        ),
        (
            "data lines of one event are joined and compacted",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\ndata: \"response\":{\"id\":\"r\",\"output\":[]}}\n\n".into(),
            vec!["event: response.completed".into(), r#"data: {"type":"response.completed","response":{"id":"r","output":[]}}"#.into(), "".into()],
        ),
        (
            "joined CRLF event",
            "data: {\"a\":\r\ndata: [1,\r\ndata: 2]}\r\n\r\n".into(),
            vec![r#"data: {"a":[1,2]}"#.into(), "".into()],
        ),
        (
            "comment and field lines inside an event come before its data",
            "event: e\ndata: {\"a\":\n: keep-alive\nid: 7\nretry: 3000\ndata: 1}\n\n".into(),
            vec!["event: e".into(), ": keep-alive".into(), "id: 7".into(), "retry: 3000".into(), r#"data: {"a":1}"#.into(), "".into()],
        ),
        (
            "unterminated one-line event at end of stream",
            "data: {\"type\":\"response.completed\"}".into(),
            vec![r#"data: {"type":"response.completed"}"#.into()],
        ),
        (
            "unterminated multi-line event at end of stream",
            "data: {\"type\":\ndata: \"response.completed\"}\n".into(),
            vec![r#"data: {"type":"response.completed"}"#.into()],
        ),
        (
            "complete events without blank separators stay separate",
            "data: {\"a\":1}\ndata: {\"b\":2}\n".into(),
            vec![r#"data: {"a":1}"#.into(), r#"data: {"b":2}"#.into()],
        ),
        (
            "split event followed by an event without a blank separator",
            "data: {\"a\":\ndata: 1}\ndata: {\"b\":2}\n".into(),
            vec![r#"data: {"a":1}"#.into(), r#"data: {"b":2}"#.into()],
        ),
        (
            "an invalid fragment absorbs the following data lines up to the blank line",
            format!("data: {{\"a\":\"{marker}\ndata: {{\"b\":2}}\n\n"),
            vec![format!("data: {{\"a\":\"{marker}\ndata: {{\"b\":2}}"), "".into()],
        ),
        (
            "only the first invalid event in a stream is described",
            "data: {\"a\":\n\ndata: {\"b\":\n\ndata: {\"c\":3}\n\n".into(),
            vec!["data: {\"a\":".into(), "".into(), "data: {\"b\":".into(), "".into(), r#"data: {"c":3}"#.into(), "".into()],
        ),
        (
            "a complete object on its own line inside an open array stays in the event",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[\ndata: {\"id\":\"m\",\"type\":\"message\"}\ndata: ]}}\n\n".into(),
            vec![r#"data: {"type":"response.completed","response":{"output":[{"id":"m","type":"message"}]}}"#.into(), "".into()],
        ),
        (
            "unterminated incomplete event at end of stream is still delivered",
            format!("data: {{\"type\":\"response.completed\",\"t\":\"{marker}\","),
            vec![format!("data: {{\"type\":\"response.completed\",\"t\":\"{marker}\",")],
        ),
        (
            "empty and non-JSON data lines take the stock path",
            "data:\n\ndata: \n\ndata: ping\n\n".into(),
            vec!["data:".into(), "".into(), "data: ".into(), "".into(), "data: ping".into(), "".into()],
        ),
        ("done sentinel", "data: [DONE]\n\n".into(), vec!["data: [DONE]".into(), "".into()]),
        (
            "invalid one-line event passes through with one warning",
            format!("data: {{\"type\":\"response.completed\",\"t\":\"{marker}\",\n\n"),
            vec![format!("data: {{\"type\":\"response.completed\",\"t\":\"{marker}\","), "".into()],
        ),
        (
            "event split inside a string stays invalid and keeps its data lines",
            format!("data: {{\"t\":\"{marker}\ndata: {marker}\"}}\n\n"),
            vec![format!("data: {{\"t\":\"{marker}\ndata: {marker}\"}}"), "".into()],
        ),
    ];
    for (name, stream, want) in cases {
        assert_eq!(scan(&stream, 1 << 20), want, "{name}");
    }
}

/// `TestCodexSSEScannerBoundsAnEventToTheLineLimit`: the collected fragment is delivered
/// as an invalid event; the line that would pass the limit is delivered on its own.
#[test]
fn bounds_an_event_to_the_line_limit() {
    let (x, y, z) = ("x".repeat(40), "y".repeat(40), "z".repeat(40));
    let stream = format!("data: {{\"a\":\"{x}\ndata: {y}\ndata: {z}\"}}\n\ndata: {{\"b\":2}}\n\n");
    let want = vec![
        format!("data: {{\"a\":\"{x}"),
        format!("data: {y}"),
        format!("data: {z}\"}}"),
        String::new(),
        r#"data: {"b":2}"#.to_owned(),
        String::new(),
    ];
    assert_eq!(scan(&stream, 64), want);
}

/// `TestCodexSSEScannerAssemblesAnEventOfThousandsOfLinesQuickly`, without its time bound:
/// 2,000 data lines that
/// each end in `}` assemble into one line; only nesting tells the event is unfinished.
#[test]
fn assembles_an_event_of_thousands_of_lines() {
    let item = |i: usize| {
        let letter = (b'a' + (i % 26) as u8) as char;
        format!(
            r#"{{"type":"message","content":[{{"type":"output_text","text":"{}"}}]}}"#,
            letter.to_string().repeat(960)
        )
    };
    let head = r#"{"type":"response.completed","response":{"id":"resp_big","output":["#;
    let (mut payload, mut stream) = (format!("{head}{}", item(0)), format!("data: {head}{}\n", item(0)));
    for i in 1..2000 {
        payload.push_str(&format!(",{}", item(i)));
        stream.push_str(&format!("data: ,{}\n", item(i)));
    }
    payload.push_str("]}}");
    stream.push_str("data: ]}}\n\n");
    let got = scan(&stream, 52_428_800);
    assert_eq!(got, vec![format!("data: {payload}"), String::new()]);
}

/// `TestCodexSSEScannerMatchesBufioScannerOnWellFormedStreams`: a stream of one-line events
/// reads exactly as split lines, with LF and CRLF.
#[test]
fn matches_line_splitting_on_well_formed_streams() {
    let mut stream =
        "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5.4\"}}\n\n"
            .to_owned();
    for i in 0..6 {
        stream.push_str(&format!(
            ": comment-{i:04}\nevent: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"{}\"}}\n\n",
            "x".repeat(64)
        ));
    }
    stream.push_str("data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[]}}\n\n");
    stream.push_str(
        "data: {\"type\":\"x\"}\ndata: {\"type\":\"y\"}\n: trailing comment\n\nevent: done\ndata: [DONE]\n\n",
    );
    for newline in ["\n", "\r\n"] {
        let input = stream.replace('\n', newline);
        let want: Vec<String> = input.lines().map(str::to_owned).collect();
        assert_eq!(scan(&input, 1 << 20), want, "{newline:?}");
    }
}

/// Go `TestCodexExecuteStreamKeepsAnInvalidSplitEventInvalid`, at the executor's chunk: an
/// event split inside a string keeps its two data lines. (Go's Responses handler framer
/// then drops the frame, as the client stream here does.)
#[test]
fn an_invalid_split_event_stays_invalid() {
    let mut processor = crate::codex_response::Processor::new(false, false);
    let event = b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_bad\",\"status\":\"comp\ndata: leted\",\"output\":[]}}\n\n";
    let crate::codex_response::Step::Terminal(out) = processor.event(event) else {
        panic!("a terminal event");
    };
    assert_eq!(&out[..], &event[..]);
}
