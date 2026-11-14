//! Responses reasoning reaching a Claude client (#168), through the `(Claude, Codex)`
//! pair that Grok, Muse and the Codex translated route share.
//!
//! A reasoning item's summary parts reach the client as one thinking block whose text
//! joins the parts with a blank line, streamed and buffered alike. A whitespace-only
//! summary is no thinking text: it opens no block, and a signed item still gets its
//! signature-only block so the encrypted reasoning round-trips.
use cpa_core::format::Format;
use cpa_translate::{ResponseCtx, pair};
use serde_json::{Value, json};

const CLAUDE: &[u8] = br#"{"stream":true,"messages":[{"role":"user","content":"hi"}]}"#;

fn ctx() -> ResponseCtx<'static> {
    ResponseCtx {
        model: "m",
        original_request: CLAUDE,
        translated_request: b"{}",
        tool_search: false,
    }
}

/// The Claude content blocks a stream builds: deltas folded into their start block.
fn streamed(events: &[Value]) -> Vec<Value> {
    let pair = pair(Format::Claude, Format::Codex).unwrap();
    let mut s = (pair.stream)(&ctx());
    let mut chunks = vec![];
    for e in events {
        let frame = format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap());
        chunks.extend(s.event(frame.as_bytes()).unwrap());
    }
    chunks.extend(s.finish().unwrap());
    let text: String = chunks.iter().map(|c| String::from_utf8_lossy(c).into_owned()).collect();
    let mut blocks: Vec<Value> = vec![];
    for data in text.lines().filter_map(|l| l.strip_prefix("data: ")) {
        let e: Value = serde_json::from_str(data).unwrap();
        match e["type"].as_str().unwrap() {
            "content_block_start" => {
                assert_eq!(e["index"], blocks.len(), "{text}");
                blocks.push(e["content_block"].clone());
            }
            "content_block_delta" => {
                let block = &mut blocks[e["index"].as_u64().unwrap() as usize];
                let delta = &e["delta"];
                let (field, value) = match delta["type"].as_str().unwrap() {
                    "thinking_delta" => ("thinking", &delta["thinking"]),
                    "text_delta" => ("text", &delta["text"]),
                    "signature_delta" => ("signature", &delta["signature"]),
                    other => panic!("unexpected delta {other}"),
                };
                let joined = block[field].as_str().unwrap_or_default().to_owned() + value.as_str().unwrap();
                block[field] = json!(joined);
            }
            _ => {}
        }
    }
    blocks
}

/// The Claude content blocks of the buffered answer to `completed`.
fn buffered(completed: &Value) -> Vec<Value> {
    let pair = pair(Format::Claude, Format::Codex).unwrap();
    let body: Value =
        serde_json::from_slice(&(pair.non_stream)(&ctx(), completed.to_string().as_bytes()).unwrap()).unwrap();
    body["content"].as_array().unwrap().clone()
}

/// One reasoning item at `index`: each part's deltas after its `part.added`, then the
/// done item carrying the parts' texts (and `encrypted_content` when signed).
fn reasoning(index: u64, parts: &[&[&str]], signature: Option<&str>) -> (Vec<Value>, Value) {
    let id = format!("rs_{index}");
    let mut events = vec![
        json!({"type":"response.output_item.added","output_index":index,"item":{"type":"reasoning","id":id,"summary":[]}}),
    ];
    let mut summary = vec![];
    for (part, deltas) in parts.iter().enumerate() {
        events.push(json!({"type":"response.reasoning_summary_part.added","output_index":index,"summary_index":part,"part":{"type":"summary_text","text":""}}));
        for delta in *deltas {
            events.push(json!({"type":"response.reasoning_summary_text.delta","output_index":index,"summary_index":part,"delta":delta}));
        }
        summary.push(json!({"type":"summary_text","text":deltas.concat()}));
    }
    let mut item = json!({"type":"reasoning","id":id,"summary":summary});
    if let Some(signature) = signature {
        item["encrypted_content"] = json!(signature);
    }
    events.push(json!({"type":"response.output_item.done","output_index":index,"item":item}));
    (events, item)
}

/// A whole answer: `items`, then a message `hi`, then `response.completed` listing every
/// output item.
fn answer(items: Vec<(Vec<Value>, Value)>) -> (Vec<Value>, Value) {
    let mut events = vec![json!({"type":"response.created","response":{"id":"resp","model":"m"}})];
    let mut output = vec![];
    for (item_events, item) in items {
        events.extend(item_events);
        output.push(item);
    }
    let index = output.len();
    let message =
        json!({"type":"message","id":"msg","role":"assistant","content":[{"type":"output_text","text":"hi"}]});
    events.extend([
        json!({"type":"response.output_item.added","output_index":index,"item":{"type":"message","id":"msg","role":"assistant","content":[]}}),
        json!({"type":"response.content_part.added","output_index":index,"content_index":0,"part":{"type":"output_text","text":""}}),
        json!({"type":"response.output_text.delta","output_index":index,"content_index":0,"delta":"hi"}),
        json!({"type":"response.content_part.done","output_index":index,"content_index":0,"part":{"type":"output_text","text":"hi"}}),
        json!({"type":"response.output_item.done","output_index":index,"item":message}),
    ]);
    output.push(message);
    let completed = json!({"type":"response.completed","response":{"id":"resp","model":"m","status":"completed","output":output,"usage":{"input_tokens":1,"output_tokens":1}}});
    events.push(completed.clone());
    (events, completed)
}

fn assert_both(items: Vec<(Vec<Value>, Value)>, want: &[Value]) {
    let (events, completed) = answer(items);
    let mut want = want.to_vec();
    want.push(json!({"type":"text","text":"hi"}));
    assert_eq!(streamed(&events), want, "streamed: {events:?}");
    assert_eq!(buffered(&completed), want, "buffered: {completed}");
}

#[test]
fn summary_parts_join_with_a_blank_line_streamed_and_buffered() {
    assert_both(
        vec![reasoning(0, &[&["one"], &["tw", "o"]], Some("enc"))],
        &[json!({"type":"thinking","thinking":"one\n\ntwo","signature":"enc"})],
    );
    assert_both(
        vec![reasoning(0, &[&["one"], &["two"], &["three"]], None)],
        &[json!({"type":"thinking","thinking":"one\n\ntwo\n\nthree"})],
    );
}

/// Whitespace inside a part that has text is the part's own text and is kept, leading
/// whitespace included.
#[test]
fn whitespace_inside_a_part_with_text_is_kept() {
    assert_both(
        vec![reasoning(0, &[&["\n", " one", "\n"], &["two"]], Some("enc"))],
        &[json!({"type":"thinking","thinking":"\n one\n\n\ntwo","signature":"enc"})],
    );
}

#[test]
fn a_whitespace_only_summary_opens_no_thinking_block() {
    // Unsigned: nothing at all.
    assert_both(vec![reasoning(0, &[&["\n"]], None)], &[]);
    assert_both(vec![reasoning(0, &[&[" ", "\n\t"], &[]], None)], &[]);
    // A part that receives no text.
    assert_both(vec![reasoning(0, &[&[]], None)], &[]);
    // Signed: the signature-only block that carries the encrypted reasoning.
    assert_both(
        vec![reasoning(0, &[&["\n"]], Some("enc"))],
        &[json!({"type":"thinking","thinking":"","signature":"enc"})],
    );
}

/// A whitespace-only part among parts with text adds no separator of its own.
#[test]
fn whitespace_only_parts_between_parts_with_text_are_skipped() {
    assert_both(
        vec![reasoning(
            0,
            &[&["\n"], &["one"], &[" "], &[], &["two"], &["\n"]],
            Some("enc"),
        )],
        &[json!({"type":"thinking","thinking":"one\n\ntwo","signature":"enc"})],
    );
}

#[test]
fn each_reasoning_item_keeps_its_own_block() {
    assert_both(
        vec![
            reasoning(0, &[&["one"], &["two"]], Some("enc_a")),
            reasoning(1, &[&[" \n"]], Some("enc_b")),
            reasoning(2, &[&["\n"], &[]], None),
            reasoning(3, &[&["three"]], None),
        ],
        &[
            json!({"type":"thinking","thinking":"one\n\ntwo","signature":"enc_a"}),
            json!({"type":"thinking","thinking":"","signature":"enc_b"}),
            json!({"type":"thinking","thinking":"three"}),
        ],
    );
}

/// Reasoning text that arrives only on the done item streams with the buffered join.
#[test]
fn done_item_text_streams_with_the_same_join() {
    let item = json!({"type":"reasoning","id":"rs_0","encrypted_content":"enc","summary":[
        {"type":"summary_text","text":"one"},{"type":"summary_text","text":"\n"},{"type":"summary_text","text":"two"}]});
    let events = vec![json!({"type":"response.output_item.done","output_index":0,"item":item})];
    assert_both(
        vec![(events, item)],
        &[json!({"type":"thinking","thinking":"one\n\ntwo","signature":"enc"})],
    );
    let blank = json!({"type":"reasoning","id":"rs_0","summary":[{"type":"summary_text","text":" \n"}]});
    let events = vec![json!({"type":"response.output_item.done","output_index":0,"item":blank})];
    assert_both(vec![(events, blank)], &[]);
}
