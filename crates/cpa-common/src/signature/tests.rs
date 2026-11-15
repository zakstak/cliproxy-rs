//! Signature replay safety: local structural policy and byte preservation.
//! Synthetic envelopes exercise parsers. They do not establish provider acceptance
//! and these tests never attempt cryptographic verification.
use super::*;
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use serde_json::{Value, json};

fn claude_signature(channel: u8) -> String {
    // Protobuf: field 2 -> field 1 -> channel_id varint and model_text bytes.
    // The envelope is deliberately built without calling a production encoder.
    let model = b"synthetic-claude";
    let mut inner = vec![0x08, channel, 0x32, model.len() as u8];
    inner.extend_from_slice(model);
    let mut payload = vec![0x12, (inner.len() + 2) as u8, 0x0a, inner.len() as u8];
    payload.extend(inner);
    STANDARD.encode(payload)
}
fn gpt_signature(blocks: usize) -> String {
    // Fernet layout: version, timestamp, IV, N AES blocks, HMAC.
    let mut payload = vec![0; 1 + 8 + 16 + 16 * blocks + 32];
    payload[0] = 0x80;
    URL_SAFE_NO_PAD.encode(payload)
}

#[test]
fn claude_envelope_layers_round_trip_and_expose_structural_fields() {
    for channel in [11, 12, 30] {
        let single = claude_signature(channel);
        let double = STANDARD.encode(&single);
        for (sig, layers) in [(&single, 1), (&double, 2)] {
            assert!(is_valid_claude_thinking_signature(sig, ClaudeValidation::STRICT));
            let tree = if layers == 1 {
                inspect_claude_single_layer_signature(sig)
            } else {
                inspect_claude_double_layer_signature(sig)
            }
            .unwrap();
            assert_eq!(tree.encoding_layers, layers);
            assert_eq!(tree.channel_id, u64::from(channel));
            assert_eq!(tree.model_text, "synthetic-claude");
            assert_eq!(
                normalize_claude_provider_native_thinking_signature(sig, ClaudeValidation::STRICT).unwrap(),
                single
            );
            assert_eq!(
                normalize_claude_thinking_signature(sig, ClaudeValidation::STRICT).unwrap(),
                double
            );
        }
    }
    for malformed in ["", "not base64", "Eg==", "EgE=", "E===", "claude#claude#bad"] {
        assert!(
            !is_valid_claude_thinking_signature(malformed, ClaudeValidation::STRICT),
            "{malformed}"
        );
    }
}

#[test]
fn fernet_shape_requires_positive_aes_block_ciphertext() {
    for blocks in [1, 2, 8] {
        let token = gpt_signature(blocks);
        let info = inspect_gpt_reasoning_signature(&token).unwrap();
        assert_eq!(info.decoded_len, 57 + 16 * blocks);
        assert_eq!(info.ciphertext_len, 16 * blocks);
        assert_eq!(detect_provider(&token), Provider::Gpt);
    }
    for length in [0, 1, 25, 57, 58, 72, 74] {
        let mut raw = vec![0; length];
        if let Some(first) = raw.first_mut() {
            *first = 0x80;
        }
        assert!(
            inspect_gpt_reasoning_signature(URL_SAFE_NO_PAD.encode(raw)).is_err(),
            "{length}"
        );
    }
    assert!(inspect_gpt_reasoning_signature("gAAAA!invalid").is_err());
}

#[test]
fn kept_provider_compatibility_preserves_own_envelopes_and_drops_foreign_blocks() {
    let cases = [
        (Provider::Claude, BlockKind::ClaudeThinking, claude_signature(11)),
        (Provider::Gpt, BlockKind::GptReasoning, gpt_signature(1)),
    ];
    for (origin, kind, signature) in cases {
        assert_eq!(detect_provider_for_block(&signature, kind), origin);
        for target in [Provider::Claude, Provider::Gpt, Provider::Grok] {
            let decision = decide_compatibility(target, &signature, kind);
            assert_eq!(decision.compatible, target == origin);
            assert_eq!(
                decision.action,
                if target == origin {
                    Action::Preserve
                } else {
                    Action::DropBlock
                }
            );
            if target == origin {
                assert_eq!(decision.normalized_signature, signature);
            } else {
                assert!(decision.replacement_signature.is_empty());
            }
        }
        let prefixed = format!("{}#{signature}", origin.as_str());
        assert_eq!(compatible_signature_for_provider(origin, &prefixed), Some(signature));
        assert_eq!(
            detect_provider(format!("{}#{prefixed}", origin.as_str())),
            Provider::Unknown
        );
    }
    for opaque in [
        "",
        "opaque-provider-data",
        "claude#unknown",
        "00000000-0000-0000-0000-000000000000",
    ] {
        assert_eq!(detect_provider(opaque), Provider::Unknown);
        for target in [Provider::Claude, Provider::Gpt, Provider::Grok] {
            assert!(!decide_compatibility(target, opaque, BlockKind::Unknown).compatible);
        }
    }
}

#[test]
fn xai_ciphertext_policy_rejects_foreign_envelopes_and_low_information_data() {
    let bytes: Vec<u8> = (0..128).collect();
    let raw = base64::engine::general_purpose::STANDARD_NO_PAD.encode(&bytes);
    assert_eq!(inspect_grok_encrypted_content(&raw).unwrap().decoded_len, 128);
    for invalid in [
        "short".to_owned(),
        format!(" {raw}"),
        format!("{raw}="),
        gpt_signature(1),
        claude_signature(11),
        base64::engine::general_purpose::STANDARD_NO_PAD.encode([0u8; 128]),
    ] {
        assert!(inspect_grok_encrypted_content(invalid).is_err());
    }
}

#[test]
fn claude_sanitizer_preserves_valid_history_and_removes_foreign_thinking() {
    let own = claude_signature(11);
    let foreign = gpt_signature(1);
    let input = json!({"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"kept","signature":own},{"type":"thinking","thinking":"drop","signature":foreign},{"type":"text","text":"answer"}]}],"other":{"n":9007199254740993u64}});
    let options = ClaudeMessagesSanitizeOptions {
        target_provider: Provider::Claude,
        drop_empty_messages: true,
        ..Default::default()
    };
    let (output, report) =
        sanitize_claude_messages_signatures_for_target(&serde_json::to_vec(&input).unwrap(), &options);
    let parsed: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(
        parsed["messages"][0]["content"],
        json!([{"type":"thinking","thinking":"kept","signature":own},{"type":"text","text":"answer"}])
    );
    assert_eq!(parsed["other"], input["other"]);
    assert_eq!(report.preserved, 1);
    assert_eq!(report.dropped_blocks, 1);
    let (again, report) = sanitize_claude_messages_signatures_for_target(&output, &options);
    assert_eq!(again, output);
    assert_eq!(report.dropped_blocks, 0);
    let plain = br#"{ "messages":[{"role":"user","content":"hi"}],"n":1.2300e+04 }"#;
    assert_eq!(sanitize_claude_messages_signatures_for_target(plain, &options).0, plain);
}

#[test]
fn signature_validation_options_are_explicit_and_empty_placeholders_are_controlled() {
    assert!(has_claude_thinking_signature_prefix("E-not-a-signature"));
    assert!(!has_decodable_claude_thinking_signature("E-not-a-signature"));
    let placeholder =
        br#"{"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"","signature":""}]}]}"#;
    assert!(validate_claude_thinking_signatures(placeholder, ClaudeValidation::STRICT).is_err());
    // Empty placeholders may survive sanitization, but do not become valid signatures.
    let allow_placeholder = ClaudeValidation {
        allow_empty_signature_with_empty_text: true,
        ..ClaudeValidation::STRICT
    };
    assert!(validate_claude_thinking_signatures(placeholder, allow_placeholder).is_err());
    assert_eq!(
        strip_invalid_claude_thinking_blocks(placeholder, allow_placeholder),
        placeholder
    );
    assert_ne!(
        strip_invalid_claude_thinking_blocks(placeholder, ClaudeValidation::STRICT),
        placeholder
    );

    let bad=br#"{"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"text","signature":"invalid"},{"type":"text","text":"answer"}]}]}"#;
    let stripped = strip_invalid_claude_thinking_blocks(bad, ClaudeValidation::STRICT);
    assert_eq!(
        serde_json::from_slice::<Value>(&stripped).unwrap()["messages"][0]["content"],
        json!([{"type":"text","text":"answer"}])
    );
}

#[test]
fn length_caps_reject_before_decoding() {
    // The recorded Go cases for these caps exceed the fixture's 64 KiB line limit.
    let huge = format!("E{}", "A".repeat(MAX_CLAUDE_THINKING_SIGNATURE_LEN));
    assert_eq!(
        normalize_claude_thinking_signature(&huge, ClaudeValidation::default())
            .unwrap_err()
            .0,
        "signature exceeds maximum length (33554432 bytes)"
    );
    assert_eq!(
        inspect_gemini_thought_signature(&huge, GeminiValidation::default())
            .unwrap_err()
            .0,
        "Gemini thought signature exceeds maximum length (33554432 bytes)"
    );
    let exact = "A".repeat(MAX_CLAUDE_THINKING_SIGNATURE_LEN - 4) + "AAA=";
    assert!(inspect_gemini_thought_signature(&exact, GeminiValidation::default()).is_ok());
}

/// The two recorded sanitizer calls over 64 KiB (translator tests at 6fecc6e), rebuilt
/// byte for byte: large inline media must pass through unchanged and without copies of
/// the body being edited.
#[test]
fn large_inline_data_passes_through_like_go() {
    let cases = [
        (
            r#"{"project":"","request":{"contents":[{"role":"user","parts":[{"inlineData":{"mimeType":"image/png","data":""#,
            r#""}},{"text":"describe"}]}]},"model":"gemini-3-flash"}"#,
            4_194_464,
            "request.contents",
        ),
        (
            r#"{"contents":[{"role":"user","parts":[{"inlineData":{"mimeType":"video/mp4","data":""#,
            r#""}}]}],"safetySettings":[]}"#,
            20_971_630,
            "contents",
        ),
    ];
    for (prefix, suffix, total, path) in cases {
        let body = format!("{prefix}{}{suffix}", "A".repeat(total - prefix.len() - suffix.len()));
        assert_eq!(body.len(), total);
        assert_eq!(
            sanitize_gemini_request_thought_signatures(body.as_bytes(), path),
            body.as_bytes()
        );
    }
}

#[test]
fn cais_envelopes_keep_signature_bytes_and_reject_missing_or_wrong_typed_fields() {
    fn bytes_field(tag: u8, bytes: &[u8]) -> Vec<u8> {
        let mut out = vec![tag, bytes.len() as u8];
        out.extend_from_slice(bytes);
        out
    }
    for version in [3, 4] {
        let mut channel = vec![0x08, 11];
        channel.extend(bytes_field(0x2a, b"opaque signature bytes"));
        channel.extend(bytes_field(0x32, b"claude-synthetic"));
        if version == 4 {
            channel.extend(bytes_field(0x42, b"thinking"));
        }
        let container = bytes_field(0x0a, &channel);
        let mut envelope = vec![0x08, version];
        envelope.extend(bytes_field(0x12, &container));
        let sig = STANDARD.encode(&envelope);
        let info = inspect_claude_cais_signature(&sig).unwrap();
        assert_eq!(info.envelope_version, u64::from(version));
        assert_eq!(info.channel_id, 11);
        assert_eq!(info.model_text, "claude-synthetic");
        assert_eq!(info.signature_len, b"opaque signature bytes".len());
        assert_eq!(detect_provider(&sig), Provider::Claude);
        assert_eq!(compatible_signature_for_provider(Provider::Claude, &sig), Some(sig));
        for cut in [0, 1, 2, envelope.len() - 1] {
            assert!(inspect_claude_cais_signature(STANDARD.encode(&envelope[..cut])).is_err());
        }
        // A length-delimited envelope version is not a varint version.
        envelope[0] = 0x0a;
        assert!(inspect_claude_cais_signature(STANDARD.encode(envelope)).is_err());
    }
}

#[test]
fn claude_upstream_sanitization_strips_tool_signatures_and_controls_empty_history() {
    let input=br#"{"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"t","name":"tool","input":{"keep":"value"},"signature":"foreign","thought_signature":"foreign"}]},{"role":"assistant","content":[{"type":"thinking","thinking":"","signature":""}]}]}"#;
    let (output, report) = sanitize_claude_messages_for_claude_upstream(input, "claude-synthetic", false);
    let output = output.expect("tool signatures and empty blocks are removed");
    let parsed: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(parsed["messages"].as_array().unwrap().len(), 1);
    assert_eq!(
        parsed["messages"][0]["content"][0],
        json!({"type":"tool_use","id":"t","name":"tool","input":{"keep":"value"}})
    );
    assert!(report.dropped_signatures > 0);
}

#[test]
fn claude_validation_options_select_prefix_decoding_or_full_structure_explicitly() {
    let valid = claude_signature(11);
    let cases = [
        (valid.as_str(), true, true, true),
        ("Eg==", true, true, false), // Decodable, but no complete protobuf tree.
        ("E!invalid", true, false, false),
        ("R!invalid", true, false, false),
        ("YQ==", false, false, false), // Decodable bytes without a Claude marker.
        ("", false, false, false),
    ];
    for (signature, prefix, decodable, strict) in cases {
        for (options, expected) in [
            (ClaudeValidation::STRICT, strict),
            (
                ClaudeValidation {
                    base64_only: true,
                    ..ClaudeValidation::STRICT
                },
                decodable,
            ),
            (
                ClaudeValidation {
                    prefix_only: true,
                    ..ClaudeValidation::STRICT
                },
                prefix,
            ),
            // Prefix-only deliberately takes precedence when both options are set.
            (
                ClaudeValidation {
                    prefix_only: true,
                    base64_only: true,
                    ..ClaudeValidation::STRICT
                },
                prefix,
            ),
        ] {
            assert_eq!(
                is_valid_claude_thinking_signature(signature, options),
                expected,
                "{signature:?} {options:?}"
            );
            let body = serde_json::to_vec(&json!({"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"history","signature":signature}]}]})).unwrap();
            // Request validation normalizes the full carrier even when a caller
            // uses the convenience predicate's permissive inspection options.
            assert_eq!(validate_claude_thinking_signatures(&body, options).is_ok(), strict);
        }
    }
}
