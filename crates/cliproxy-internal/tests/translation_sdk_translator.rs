//! The supported format matrix and fallback contract are owned by this SDK.
//! These tests deliberately do not read a registry recorded from another implementation.
use cliproxy_internal::format::Format;
use cliproxy_internal::translation::{RequestCtx, pair, token_count, translate_request, translate_token_count};

#[test]
fn supported_format_pairs_and_token_count_adapters() {
    use Format::{Claude, Codex, OpenAI, OpenAIResponse};
    let supported = [
        (OpenAI, OpenAI),
        (OpenAI, Claude),
        (OpenAI, Codex),
        (OpenAIResponse, Claude),
        (OpenAIResponse, Codex),
        (OpenAIResponse, OpenAI),
        (Claude, OpenAI),
        (Claude, Codex),
    ];
    for client in Format::ALL {
        for upstream in Format::ALL {
            assert_eq!(
                pair(client, upstream).is_some(),
                supported.contains(&(client, upstream)),
                "{client:?} -> {upstream:?}"
            );
            assert_eq!(
                token_count(client, upstream).is_some(),
                client == Claude && matches!(upstream, OpenAI | Codex),
                "{client:?} -> {upstream:?} token count"
            );
        }
    }
}

#[test]
fn unregistered_pairs_only_rewrite_the_model_and_keep_unknown_bytes() {
    let ctx = RequestCtx {
        model: "chosen",
        stream: false,
        tool_search: false,
        system_turns: false,
    };
    for client in Format::ALL {
        for upstream in Format::ALL {
            if pair(client, upstream).is_some() {
                continue;
            }
            for (body, expected) in [
                (
                    &br#"{ "model" : "old", "unknown" : 1.500, "text":"<>&" }"#[..],
                    &br#"{ "model" : "chosen", "unknown" : 1.500, "text":"<>&" }"#[..],
                ),
                (
                    &br#"{ "model" : "chosen", "unknown": [1,  2] }"#[..],
                    &br#"{ "model" : "chosen", "unknown": [1,  2] }"#[..],
                ),
            ] {
                assert_eq!(
                    translate_request(client, upstream, &ctx, body).unwrap(),
                    expected,
                    "{client:?} -> {upstream:?}"
                );
            }
            assert_eq!(
                translate_token_count(client, upstream, 17, b"opaque upstream body"),
                b"opaque upstream body"
            );
        }
    }
}

#[test]
fn messages_clients_receive_the_explicit_input_token_count() {
    for upstream in [Format::OpenAI, Format::Codex] {
        for count in [0, 1, 17, i32::MAX as i64] {
            let out = translate_token_count(Format::Claude, upstream, count, b"ignored");
            let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
            assert_eq!(value, serde_json::json!({"input_tokens": count}));
        }
    }
}
