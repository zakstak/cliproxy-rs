//! #183: with `RequestCtx::system_turns`, the Responses -> Claude translation keeps a
//! text-only system or developer item that follows the start of the conversation in
//! place, as a `role: system` turn after the next user message. Leading items, items
//! with other content, and items no user message follows stay in the top-level `system`,
//! in source order, with the structured-output instruction last. Without the flag every
//! item is top-level, as before.

use cpa_core::format::Format;
use cpa_translate::{RequestCtx, translate_request};
use serde_json::{Value, json};

fn translate(system_turns: bool, body: &Value) -> Value {
    let ctx = RequestCtx {
        model: "claude-opus-4-8",
        stream: false,
        tool_search: false,
        system_turns,
    };
    let raw = serde_json::to_vec(body).unwrap();
    serde_json::from_slice(&translate_request(Format::OpenAIResponse, Format::Claude, &ctx, &raw).unwrap()).unwrap()
}

fn text(t: &str) -> Value {
    json!({"type": "text", "text": t})
}

fn body() -> Value {
    json!({"model": "claude-opus-4-8", "instructions": "base",
    "text": {"format": {"type": "json_schema", "name": "out", "schema": {"type": "object"}}},
    "input": [
        {"type": "message", "role": "developer", "content": "leading"},
        {"type": "message", "role": "user", "content": "one"},
        {"type": "message", "role": "system", "content": [
            {"type": "input_text", "text": "after one"},
            {"type": "input_text", "text": "marked", "cache_control": {"type": "ephemeral", "ttl": "1h"}},
        ]},
        {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "two"}]},
        {"type": "message", "role": "developer", "content": [{"type": "input_image", "image_url": "data:image/png;base64,AA=="}]},
        {"role": "developer", "content": "first"},
        {"role": "developer", "content": [{"type": "input_text", "text": "second"}]},
        {"type": "message", "role": "user", "content": "three"},
        {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "four"}]},
        {"type": "message", "role": "developer", "content": "trailing"},
    ]})
}

fn system_texts(out: &Value) -> Vec<Value> {
    out["system"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b.get("text").cloned().unwrap_or_else(|| b["type"].clone()))
        .collect()
}

#[test]
fn mid_session_text_items_follow_the_next_user_message() {
    let out = translate(true, &body());
    assert_eq!(
        out["messages"],
        json!([
            {"role": "user", "content": "one"},
            {"role": "system", "content": [
                text("after one"),
                {"type": "text", "text": "marked", "cache_control": {"type": "ephemeral", "ttl": "1h"}},
            ]},
            {"role": "assistant", "content": "two"},
            {"role": "user", "content": "three"},
            {"role": "system", "content": [text("first")]},
            {"role": "system", "content": [text("second")]},
            {"role": "assistant", "content": "four"},
        ]),
        "{out}"
    );
    // The leading item, the image item and the item no user message follows stay
    // top-level in source order; the structured-output instruction stays last.
    let system = system_texts(&out);
    assert_eq!(
        system[..4],
        [json!("base"), json!("leading"), json!("input_image"), json!("trailing")],
        "{out}"
    );
    assert_eq!(system.len(), 5, "{out}");
}

#[test]
fn without_system_turns_every_item_stays_top_level() {
    let out = translate(false, &body());
    assert_eq!(
        out["messages"],
        json!([
            {"role": "user", "content": "one"},
            {"role": "assistant", "content": "two"},
            {"role": "user", "content": "three"},
            {"role": "assistant", "content": "four"},
        ]),
        "{out}"
    );
    let system = system_texts(&out);
    assert_eq!(
        system[..8],
        [
            json!("base"),
            json!("leading"),
            json!("after one"),
            json!("marked"),
            json!("input_image"),
            json!("first"),
            json!("second"),
            json!("trailing"),
        ],
        "{out}"
    );
    assert_eq!(system.len(), 9, "{out}");
    assert_eq!(
        out["system"][3]["cache_control"],
        json!({"type": "ephemeral", "ttl": "1h"})
    );
}

/// An item that comes before any conversation item is a leading one, even when it is
/// not first in `input`; an item while the user message is still being built goes after
/// that message.
#[test]
fn only_items_after_the_conversation_starts_move() {
    let out = translate(
        true,
        &json!({"model": "claude-opus-4-8", "input": [
            {"role": "system", "content": "a"},
            {"role": "developer", "content": "b"},
            {"role": "user", "content": "one"},
            {"role": "developer", "content": "c"},
            {"role": "user", "content": "two"},
        ]}),
    );
    assert_eq!(out["system"], json!([text("a"), text("b")]), "{out}");
    assert_eq!(
        out["messages"],
        json!([
            {"role": "user", "content": [text("one"), text("two")]},
            {"role": "system", "content": [text("c")]},
        ]),
        "{out}"
    );
}
