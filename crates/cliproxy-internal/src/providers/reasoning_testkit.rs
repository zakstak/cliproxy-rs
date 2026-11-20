//! #168 for the xAI and Meta executors, which hand a Claude client the shared
//! `(Claude, Codex)` pair's output: one Responses answer with a two-part summary, a
//! signed whitespace-only summary and an unsigned whitespace-only summary, and the
//! Claude content blocks a client reads from it, streamed or buffered.

use serde_json::{Value, json};

/// The SSE body. Its `response.completed` lists no output, so the buffered answer is
/// built from the done items, as the executors aggregate a stream.
pub(crate) fn answer(model: &str) -> String {
    let events = [
        json!({"type":"response.created","response":{"id":"resp_r","model":model}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_a","summary":[]}}),
        json!({"type":"response.reasoning_summary_part.added","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}}),
        json!({"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":0,"delta":"one"}),
        json!({"type":"response.reasoning_summary_part.added","output_index":0,"summary_index":1,"part":{"type":"summary_text","text":""}}),
        json!({"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":1,"delta":"two"}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_a","encrypted_content":"enc_a","summary":[{"type":"summary_text","text":"one"},{"type":"summary_text","text":"two"}]}}),
        json!({"type":"response.output_item.added","output_index":1,"item":{"type":"reasoning","id":"rs_b","summary":[]}}),
        json!({"type":"response.reasoning_summary_part.added","output_index":1,"summary_index":0,"part":{"type":"summary_text","text":""}}),
        json!({"type":"response.reasoning_summary_text.delta","output_index":1,"summary_index":0,"delta":" \n"}),
        json!({"type":"response.output_item.done","output_index":1,"item":{"type":"reasoning","id":"rs_b","encrypted_content":"enc_b","summary":[{"type":"summary_text","text":" \n"}]}}),
        json!({"type":"response.output_item.added","output_index":2,"item":{"type":"reasoning","id":"rs_c","summary":[]}}),
        json!({"type":"response.reasoning_summary_part.added","output_index":2,"summary_index":0,"part":{"type":"summary_text","text":""}}),
        json!({"type":"response.reasoning_summary_text.delta","output_index":2,"summary_index":0,"delta":"\n"}),
        json!({"type":"response.reasoning_summary_part.added","output_index":2,"summary_index":1,"part":{"type":"summary_text","text":""}}),
        json!({"type":"response.output_item.done","output_index":2,"item":{"type":"reasoning","id":"rs_c","summary":[{"type":"summary_text","text":"\n"},{"type":"summary_text","text":""}]}}),
        json!({"type":"response.output_item.added","output_index":3,"item":{"type":"message","id":"msg","role":"assistant","content":[]}}),
        json!({"type":"response.content_part.added","output_index":3,"content_index":0,"part":{"type":"output_text","text":""}}),
        json!({"type":"response.output_text.delta","output_index":3,"content_index":0,"delta":"hi"}),
        json!({"type":"response.content_part.done","output_index":3,"content_index":0,"part":{"type":"output_text","text":"hi"}}),
        json!({"type":"response.output_item.done","output_index":3,"item":{"type":"message","id":"msg","role":"assistant","content":[{"type":"output_text","text":"hi"}]}}),
        json!({"type":"response.completed","response":{"id":"resp_r","object":"response","status":"completed","model":model,"output":[],"usage":{"input_tokens":5,"output_tokens":3,"total_tokens":8}}}),
    ];
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

/// What the client must read from [`answer`]: one thinking block joining the two parts,
/// the signed blank summary as a signature-only block, nothing for the unsigned one.
pub(crate) fn want() -> Vec<Value> {
    vec![
        json!({"type":"thinking","thinking":"one\n\ntwo","signature":"enc_a"}),
        json!({"type":"thinking","thinking":"","signature":"enc_b"}),
        json!({"type":"text","text":"hi"}),
    ]
}

/// The content blocks of a Claude answer: a buffered message's `content`, or a stream's
/// blocks with their deltas folded in.
pub(crate) fn blocks(answer: &str) -> Vec<Value> {
    if let Ok(message) = serde_json::from_str::<Value>(answer) {
        return message["content"].as_array().cloned().unwrap_or_default();
    }
    let mut blocks: Vec<Value> = vec![];
    for data in answer.lines().filter_map(|l| l.strip_prefix("data: ")) {
        let Ok(e) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        match e["type"].as_str() {
            Some("content_block_start") => blocks.push(e["content_block"].clone()),
            Some("content_block_delta") => {
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
