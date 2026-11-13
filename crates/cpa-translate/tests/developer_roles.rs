//! #80: a Responses `developer` message keeps its role, position and content when it is
//! converted to Chat Completions. Chat Completions takes `developer` messages as
//! instructions, and the Responses reference says developer instructions outrank user
//! messages. Go v8.0.35 sends them as `user` text (docs/DIFFERENCES-FROM-GO.md). Only a
//! real user turn can still be refused as emptied.
//!
//! `/v1/chat/completions` runs a Responses-shaped body through
//! `responses_to_chat_completions_body`, then through the upstream's Chat converter. The
//! tests below follow that chain into each converter the route can reach: the registered
//! Chat -> Codex pair (Grok, Muse), the Codex translated route and the Chat -> Claude pair.
//! cpa-server's `developer_role_e2e` proves the final outbound bodies.

use cpa_core::format::Format;
use cpa_translate::{RequestCtx, codex_request, responses_to_chat_completions_body, translate_request};
use serde_json::{Value, json};

const CTX: RequestCtx<'static> = RequestCtx {
    model: "grok-4.7",
    stream: false,
    tool_search: false,
};

/// A leading developer message with string content and a later one with a text-part
/// array, around user and assistant turns.
fn responses_body(instructions: bool) -> Value {
    let mut body = json!({"model": "grok-4.7", "input": [
        {"type": "message", "role": "developer", "content": "leading instruction"},
        {"type": "message", "role": "user", "content": "question"},
        {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "prior answer"}]},
        {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "later instruction"}]},
        {"role": "user", "content": [{"type": "input_text", "text": "continue"}]},
    ]});
    if instructions {
        body["instructions"] = json!("base instructions");
    }
    body
}

fn parse(out: &[u8]) -> Value {
    serde_json::from_slice(out).unwrap()
}

fn chat_messages(instructions: bool) -> Vec<Value> {
    let mut messages = vec![];
    if instructions {
        messages.push(json!({"role": "system", "content": "base instructions"}));
    }
    messages.extend([
        json!({"role": "developer", "content": "leading instruction"}),
        json!({"role": "user", "content": "question"}),
        json!({"role": "assistant", "content": [{"type": "text", "text": "prior answer"}]}),
        json!({"role": "developer", "content": [{"type": "text", "text": "later instruction"}]}),
        json!({"role": "user", "content": [{"type": "text", "text": "continue"}]}),
    ]);
    messages
}

#[test]
fn public_and_registered_converters_keep_developer_roles_in_place() {
    for instructions in [false, true] {
        let raw = serde_json::to_vec(&responses_body(instructions)).unwrap();
        let public = parse(&responses_to_chat_completions_body("grok-4.7", &raw, false));
        let registered = parse(&translate_request(Format::OpenAIResponse, Format::OpenAI, &CTX, &raw).unwrap());
        for (name, chat) in [("public", public), ("registered", registered)] {
            assert_eq!(
                chat["messages"],
                Value::Array(chat_messages(instructions)),
                "{name} instructions={instructions}"
            );
        }
    }
}

#[test]
fn a_developer_turn_is_never_refused_as_an_emptied_user_turn() {
    // A developer message whose only part Chat cannot carry is not the user's turn.
    let developer_only = json!({"model": "grok-4.7", "input": [
        {"type": "message", "role": "developer", "content": [{"type": "input_file", "file_url": "https://example.test/a.pdf"}]},
        {"type": "message", "role": "user", "content": "question"},
    ]});
    let raw = serde_json::to_vec(&developer_only).unwrap();
    let chat = parse(&translate_request(Format::OpenAIResponse, Format::OpenAI, &CTX, &raw).unwrap());
    assert_eq!(
        chat["messages"],
        json!([{"role": "developer", "content": []}, {"role": "user", "content": "question"}])
    );

    // An emptied user turn is still refused, with or without a developer message before it.
    let emptied_user = json!({"model": "grok-4.7", "input": [
        {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "dev"}]},
        {"type": "message", "role": "user", "content": [{"type": "input_file", "file_url": "https://example.test/a.pdf"}]},
    ]});
    let raw = serde_json::to_vec(&emptied_user).unwrap();
    let refused = translate_request(Format::OpenAIResponse, Format::OpenAI, &CTX, &raw).unwrap_err();
    assert!(refused.0.contains("input_file"), "{}", refused.0);
    // The Chat Completions route ignores the refusal like Go and still sends the body,
    // now with the developer role.
    let chat = parse(&responses_to_chat_completions_body("grok-4.7", &raw, false));
    assert_eq!(
        chat["messages"],
        json!([
            {"role": "developer", "content": [{"type": "text", "text": "dev"}]},
            {"role": "user", "content": []}
        ])
    );
}

/// `(role, text)` for each Responses `message` input item.
fn input_messages(body: &Value) -> Vec<(String, String)> {
    body["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == "message")
        .map(|item| {
            let text = item["content"]
                .as_array()
                .unwrap()
                .iter()
                .map(|part| part["text"].as_str().unwrap())
                .collect::<String>();
            (item["role"].as_str().unwrap().to_owned(), text)
        })
        .collect()
}

fn expected_input(instructions: bool) -> Vec<(String, String)> {
    let mut want = vec![];
    if instructions {
        // A Chat `system` message becomes a developer item on both Codex converters.
        want.push(("developer", "base instructions"));
    }
    want.extend([
        ("developer", "leading instruction"),
        ("user", "question"),
        ("assistant", "prior answer"),
        ("developer", "later instruction"),
        ("user", "continue"),
    ]);
    want.into_iter().map(|(r, t)| (r.to_owned(), t.to_owned())).collect()
}

#[test]
fn grok_muse_and_codex_conversions_keep_the_developer_roles() {
    for instructions in [false, true] {
        let raw = serde_json::to_vec(&responses_body(instructions)).unwrap();
        let chat = responses_to_chat_completions_body("grok-4.7", &raw, false);
        // The registered pair the Grok and Muse executors share.
        let registered = parse(&translate_request(Format::OpenAI, Format::Codex, &CTX, &chat).unwrap());
        // The Codex translated route's strict conversion.
        let strict = parse(&codex_request(Format::OpenAI, &CTX, &chat).unwrap().unwrap());
        for (name, body) in [("registered", registered), ("codex strict", strict)] {
            assert_eq!(
                input_messages(&body),
                expected_input(instructions),
                "{name} instructions={instructions}: {body}"
            );
        }
    }
}

/// Anthropic Messages has no developer role: the Chat -> Claude converter puts developer
/// text in `system`, as it does for a Chat client's own developer message and as the
/// Responses -> Claude converter does on `/v1/responses`.
#[test]
fn claude_receives_developer_text_as_system_instructions() {
    let raw = serde_json::to_vec(&responses_body(true)).unwrap();
    let chat = responses_to_chat_completions_body("claude-sonnet-4-5", &raw, false);
    let ctx = RequestCtx {
        model: "claude-sonnet-4-5",
        ..CTX
    };
    let claude = parse(&translate_request(Format::OpenAI, Format::Claude, &ctx, &chat).unwrap());
    let system: Vec<&str> = claude["system"]
        .as_array()
        .unwrap()
        .iter()
        .map(|block| block["text"].as_str().unwrap())
        .collect();
    assert_eq!(
        system,
        ["base instructions", "leading instruction", "later instruction"],
        "{claude}"
    );
    let roles: Vec<&str> = claude["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "assistant", "user"], "{claude}");
    assert!(!claude.to_string().contains("\"developer\""), "{claude}");
}
