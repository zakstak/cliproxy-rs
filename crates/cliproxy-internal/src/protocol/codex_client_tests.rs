//! Behavior of optional rewrites for translated Codex clients, using explicit requests.
//! Native Codex payloads remain governed by docs/codex/CONTRACT.md.
use super::*;
use serde_json::{Value, json};

fn official_headers() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("user-agent", "codex_cli_rs/0.150.0".parse().unwrap());
    h
}

#[test]
fn orphan_outputs_rewrite_only_when_the_option_and_subagent_identity_apply() {
    let body = br#"{ "input":[{"type":"function_call","call_id":"paired"},{"type":"function_call_output","call_id":"paired","namespace":"codex_app","name":"create_thread","output":"keep"},{"type":"function_call_output","call_id":"orphan","namespace":"codex_app","name":"create_thread","output":"answer"}], "raw":9007199254740993 }"#;
    let mut h = official_headers();
    assert_eq!(rewrite_orphan_delegation_input(&h, body, true), body);
    h.insert("x-openai-subagent", "collab_spawn".parse().unwrap());
    assert_eq!(rewrite_orphan_delegation_input(&h, body, false), body);
    let output = rewrite_orphan_delegation_input(&h, body, true);
    let parsed: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(parsed["input"][1]["type"], "function_call_output");
    assert_eq!(
        parsed["input"][2],
        json!({"type":"message","role":"user","content":[{"type":"input_text","text":"Tool output from codex_app__create_thread:\nanswer"}]})
    );
    assert!(
        output
            .windows(b"9007199254740993".len())
            .any(|span| span == b"9007199254740993")
    );
}

#[test]
fn subagent_header_accepts_the_protocol_identity_and_refuses_other_tokens() {
    let body=br#"{"input":[{"type":"function_call_output","call_id":"x","name":"create_thread","namespace":"codex_app","output":"o"}]}"#;
    for (token, rewrite) in [
        ("collab_spawn", true),
        ("COLLAB_SPAWN", true),
        ("collab_spawnx", false),
        ("", false),
    ] {
        let mut headers = HeaderMap::new();
        headers.insert("x-openai-subagent", token.parse().unwrap());
        assert_eq!(
            rewrite_orphan_delegation_input(&headers, body, true) != body,
            rewrite,
            "{token}"
        );
    }
}

#[test]
fn agent_messages_convert_content_and_compatibility_removes_only_extension_fields() {
    let body=br#"{"input":[{"type":"agent_message","author":"a","recipient":"b","content":[{"type":"encrypted_content","encrypted_content":"hello"}]}],"other":{"keep":true}}"#;
    let settings = Settings {
        optimize_multi_agent_v2: true,
        orphan_delegation: false,
    };
    let output = rewrite_multi_agent_v2_input(&official_headers(), body, &settings, true);
    let parsed: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(
        parsed["input"][0],
        json!({"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]})
    );
    assert_eq!(parsed["other"], json!({"keep":true}));
    assert_eq!(
        rewrite_multi_agent_v2_input(&HeaderMap::new(), body, &settings, false),
        body
    );
}

#[test]
fn tools_prepare_optimize_and_restore_without_touching_tool_arguments() {
    let body=br#"{"tools":[{"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"spawn_agent","description":"Spawns an agent.","parameters":{"properties":{"message":{"encrypted":true}}}}]}]}"#;
    let settings = Settings {
        optimize_multi_agent_v2: true,
        orphan_delegation: false,
    };
    let (prepared, eligible) = prepare_tools(&official_headers(), body, true, || "- custom: model".into());
    assert!(eligible);
    let parsed: Value = serde_json::from_slice(&prepared).unwrap();
    assert!(
        parsed["tools"][0]["tools"][0]["parameters"]["properties"]["message"]
            .get("encrypted")
            .is_none()
    );
    let (optimized, renamed) = optimize_request(&official_headers(), &prepared, &settings, true, || {
        panic!("already prepared")
    });
    assert!(renamed);
    assert_eq!(
        serde_json::from_slice::<Value>(&optimized).unwrap()["tools"][0]["name"],
        "collaboration-optimize"
    );
    let response=br#"{"type":"function_call","namespace":"collaboration-optimize","name":"spawn_agent","arguments":{"namespace":"collaboration-optimize"},"n":9007199254740993}"#;
    let restored = restore_response(response, true);
    let parsed: Value = serde_json::from_slice(&restored).unwrap();
    assert_eq!(parsed["namespace"], "collaboration");
    assert_eq!(parsed["arguments"]["namespace"], "collaboration-optimize");
    assert_eq!(parsed["n"].as_u64(), Some(9007199254740993));
    assert_eq!(restore_response(response, false), response);
}

#[test]
fn namespace_conflicts_are_detected_in_both_tool_locations() {
    for body in [
        r#"{"tools":[{"type":"namespace","name":"collaboration-optimize","tools":[]}]}"#,
        r#"{"input":[{"type":"additional_tools","tools":[{"type":"function","name":"collaboration-optimize__spawn_agent"}]}]}"#,
    ] {
        assert!(has_namespace_conflict(body.as_bytes()));
    }
    assert!(!has_namespace_conflict(
        br#"{"tools":[{"type":"function","name":"normal"}]}"#
    ));
}

#[test]
fn official_user_agents_and_model_descriptions_remain_discoverable() {
    for (ua, expected) in [
        ("codex_cli_rs/0.150.0", true),
        ("Codex Desktop/1", true),
        ("codex_exec/1", true),
        ("codex-tui/1", true),
        ("curl/1", false),
        ("fake-codex_cli_rs/1", false),
    ] {
        assert_eq!(is_codex_client_user_agent(ua), expected, "{ua}");
    }
    let available = vec![AvailableModel {
        id: "custom-model".into(),
        display_name: "Custom".into(),
        description: "Operator model".into(),
    }];
    let markdown = spawn_agent_models(&available, &|_| None);
    assert!(markdown.contains("custom-model"));
    assert!(markdown.contains("Operator model"));
    let description =
        "Spawn.\n\nAvailable model overrides (optional; inherited parent model is preferred):\n- old\n\nNext section.";
    let updated = replace_spawn_agent_models(description, "- custom-model");
    assert!(updated.contains("custom-model"));
    assert!(!updated.contains("- old"));
}

/// Tools already prepared at the Responses boundary only lose `message.encrypted`; the
/// description keeps whatever the boundary wrote and the model list is not rebuilt.
#[test]
fn prepared_tools_skip_the_model_list() {
    let mut h = HeaderMap::new();
    h.insert("user-agent", "codex_cli_rs/0.150.0".parse().unwrap());
    let input = br#"{"tools":[{"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"spawn_agent","description":"Spawns an agent.","parameters":{"properties":{"message":{"encrypted":true}}}}]}]}"#;
    let settings = Settings {
        optimize_multi_agent_v2: true,
        orphan_delegation: false,
    };
    let (got, renamed) = optimize_request(&h, input, &settings, true, || panic!("model list rebuilt"));
    assert!(renamed);
    assert_eq!(
        String::from_utf8(got).unwrap(),
        r#"{"tools":[{"type":"namespace","name":"collaboration-optimize","tools":[{"type":"function","name":"spawn_agent","description":"Spawns an agent.","parameters":{"properties":{"message":{}}}}]}]}"#
    );
}

#[test]
fn settings_read_go_config_paths() {
    let cfg = Config::parse(
        "client:\n  codex:\n    optimize-multi-agent-v2: true\noauth:\n  providers:\n    codex:\n      orphan-delegation-compatibility: true\n",
    )
    .unwrap();
    assert_eq!(
        Settings::from_config(&cfg),
        Settings {
            optimize_multi_agent_v2: true,
            orphan_delegation: true
        }
    );
    assert_eq!(Settings::from_config(&Config::default()), Settings::default());
}

#[test]
fn normalizes_missing_or_null_instructions() {
    for (input, want) in [
        (r#"{"model":"m"}"#, r#"{"model":"m","instructions":""}"#),
        (r#"{"instructions":null}"#, r#"{"instructions":""}"#),
        (r#"{"instructions":"keep"}"#, r#"{"instructions":"keep"}"#),
    ] {
        let mut body = input.as_bytes().to_vec();
        normalize_codex_instructions(&mut body);
        assert_eq!(String::from_utf8(body).unwrap(), want);
    }
}

/// The ChatGPT Codex backend's rejection of an input larger than the window, recorded
/// from a live subscription request (no `response.created` before it).
const LIVE_CONTEXT_ERROR: &str = include_str!("../../tests/protocol/fixtures/codex_context_window_error_live.json");

#[test]
fn context_window_error_becomes_the_failure_codex_compacts_on() {
    let event = LIVE_CONTEXT_ERROR.trim();
    assert!(is_context_window_rejection(event.as_bytes()));
    assert!(!is_context_window_failure(event.as_bytes()));
    let out = context_window_failure_event(event, event).expect("normalized");
    assert_eq!(
        out,
        concat!(
            r#"{"type":"response.failed","sequence_number":3,"response":{"id":"resp_0565bda653e9b5da016aee80a2d70487d18b1c1858bc049dcf","object":"response","status":"failed","#,
            r#""error":{"message":"Your input exceeds the context window of this model. Please adjust your input and try again.","type":"invalid_request_error","param":"input","code":"context_length_exceeded"}}}"#
        )
    );
    assert!(is_context_window_failure(out.as_bytes()));
}

#[test]
fn context_window_failure_event_keeps_failures_and_ignores_others() {
    let failed = r#"{"type":"response.failed","response":{"id":"r1","error":{"code":"context_length_exceeded"}}}"#;
    assert!(matches!(context_window_failure_event(failed, failed), Some(std::borrow::Cow::Borrowed(f)) if f == failed));
    let error = r#"{"type":"error","error":{"code":"context_length_exceeded"}}"#;
    assert_eq!(
        context_window_failure_event(error, error).as_deref(),
        Some(
            r#"{"type":"response.failed","response":{"object":"response","status":"failed","error":{"code":"context_length_exceeded"}}}"#
        )
    );
    for other in [
        r#"{"type":"error","error":{"code":"rate_limit_exceeded","message":"slow down"}}"#,
        r#"{"type":"error","error":{"code":"context_too_large"}}"#,
        r#"{"type":"error","code":"context_length_exceeded"}"#,
        r#"{"type":"error","status":400,"error":{"code":"context_length_exceeded"}}"#,
        r#"{"type":"error","status_code":400,"error":{"code":"context_length_exceeded"}}"#,
        r#"{"type":"response.failed","response":{"error":{"code":"server_error"}}}"#,
        r#"{"type":"response.completed","response":{"error":{"code":"context_length_exceeded"}}}"#,
    ] {
        assert!(context_window_failure_event(other, other).is_none(), "{other}");
        assert!(!is_context_window_rejection(other.as_bytes()), "{other}");
    }
}

/// The copy-free variant agrees with `prepare_responses_request`: `None` exactly when the
/// body would come back unchanged, and the same bytes otherwise.
#[test]
fn prepare_if_changed_matches_the_full_preparation() {
    let codex = |subagent: bool| {
        let mut h = HeaderMap::new();
        h.insert("user-agent", "codex_cli_rs/0.160.0 (Linux; x86_64)".parse().unwrap());
        if subagent {
            h.insert("x-openai-subagent", "collab_spawn".parse().unwrap());
        }
        h
    };
    let orphan = br#"{"input":[{"type":"function_call_output","call_id":"orphan","name":"create_thread","namespace":"codex_app","output":"ok"}]}"#;
    let plain = br#"{"input":[{"type":"message","role":"user","content":"hi"}]}"#;
    for (headers, orphan_on, body) in [
        (HeaderMap::new(), true, &orphan[..]),
        (codex(false), true, &orphan[..]),
        (codex(true), false, &orphan[..]),
        (codex(true), true, &orphan[..]),
        (codex(true), true, &plain[..]),
    ] {
        let settings = Settings {
            optimize_multi_agent_v2: true,
            orphan_delegation: orphan_on,
        };
        let full = prepare_responses_request(&headers, body, &settings);
        let changed = prepare_responses_request_if_changed(&headers, body, &settings);
        assert_eq!(changed.as_deref().unwrap_or(body), full.as_slice());
        assert_eq!(changed.is_some(), full != body);
    }
}
