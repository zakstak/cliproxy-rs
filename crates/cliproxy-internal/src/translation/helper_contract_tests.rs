//! Translator helper behavior, specified with small inputs rather than recorded calls.
use crate::protocol::json as gj;
use serde_json::{Value, json};

fn value(raw: &[u8]) -> Value {
    serde_json::from_slice(raw).unwrap()
}

#[test]
fn patch_tool_declarations_and_payloads_are_explicit() {
    use crate::translation::apply_patch::{PARAMETERS, description, is_custom_tool, unwrap_input, wrap_input};
    for (tool, custom) in [
        (json!({"type":"custom", "name":"apply_patch"}), true),
        (json!({"type":"function", "name":"apply_patch"}), false),
        (json!({"type":"custom", "name":"other"}), false),
    ] {
        assert_eq!(is_custom_tool(&gj::parse(&serde_json::to_vec(&tool).unwrap())), custom);
    }
    let parameters = value(PARAMETERS.as_bytes());
    assert_eq!(parameters["type"], "object");
    assert_eq!(parameters["required"], json!(["input"]));
    assert_eq!(parameters["properties"]["input"]["type"], "string");
    assert_eq!(parameters["additionalProperties"], false);
    let described = description(&gj::parse(br#"{"description":"keep this explanation"}"#));
    assert!(String::from_utf8(described).unwrap().contains("keep this explanation"));
    for patch in ["", "*** Begin Patch\n+é🙂\n*** End Patch", "<>&\"\\\t\n"] {
        assert_eq!(value(&wrap_input(patch.as_bytes())), json!({"input":patch}));
        assert_eq!(unwrap_input(&wrap_input(patch.as_bytes())).as_deref(), Some(patch));
    }
    for invalid in [
        b"null".as_slice(),
        b"{}",
        br#"{"input":null}"#,
        br#"{"input":"a","input":"b"}"#,
    ] {
        assert!(unwrap_input(invalid).is_none());
    }
}

#[test]
fn explicit_user_identity_wins_and_conversation_identity_is_stable() {
    use crate::translation::common::derive_claude_user_id as identity;
    for (request, expected) in [
        (json!({"metadata":{"user_id":"operator"},"user":"other"}), "operator"),
        (json!({"metadata":{"user_id":" "},"user":"client"}), "client"),
        (json!({}), "unknown"),
    ] {
        assert_eq!(identity(&serde_json::to_vec(&request).unwrap()), expected.as_bytes());
    }
    let stable = |request: Value| identity(&serde_json::to_vec(&request).unwrap());
    for key in ["prompt_cache_key", "session_id", "sessionId", "conversation_id"] {
        let first = stable(json!({key:"session-a","messages":[{"role":"user","content":"first"}]}));
        let later = stable(json!({key:"session-a","messages":[{"role":"user","content":"later"}]}));
        let other = stable(json!({key:"session-b","messages":[{"role":"user","content":"first"}]}));
        assert_eq!(first, later, "{key} must survive subsequent turns");
        assert_ne!(first, other, "{key} must isolate conversations");
    }
    assert_eq!(
        stable(json!({"conversation":"same"})),
        stable(json!({"conversation":{"id":"same"}}))
    );
    let first = json!({"model":"one","messages":[{"role":"user","content":"first"}]});
    let later = json!({"model":"two","messages":[{"role":"user","content":"first"},{"role":"assistant","content":"reply"},{"role":"user","content":"next"}]});
    assert_eq!(
        stable(first),
        stable(later),
        "the first user content anchors unlabelled continuations"
    );
}

#[test]
fn cache_hints_attach_to_the_intended_block_without_changing_content() {
    use crate::translation::common::{
        attach_cache_control, attach_message_cache_control, attach_tool_message_cache_control,
    };
    let source = gj::parse(br#"{"cache_control":{"type":"ephemeral","ttl":"1h"}}"#);
    assert_eq!(
        value(&attach_cache_control(br#"{"text":"x"}"#.to_vec(), &source)),
        json!({"text":"x","cache_control":{"type":"ephemeral","ttl":"1h"}})
    );
    for invalid in [
        b"{}".as_slice(),
        br#"{"cache_control":null}"#,
        br#"{"cache_control":{"type":"invalid"}}"#,
    ] {
        let raw = br#"{ "text":"unchanged", "number":1.50 }"#.to_vec();
        assert_eq!(attach_cache_control(raw.clone(), &gj::parse(invalid)), raw);
    }
    let msg = br#"{"role":"user","content":[{"type":"text","text":"first"},{"type":"text","text":"last"}]}"#.to_vec();
    let output = value(&attach_message_cache_control(msg, &source));
    assert!(output["content"][0].get("cache_control").is_none());
    assert_eq!(output["content"][1]["text"], "last");
    assert_eq!(
        output["content"][1]["cache_control"],
        json!({"type":"ephemeral","ttl":"1h"})
    );
    let existing =
        br#"{"content":[{"type":"text","text":"x","cache_control":{"type":"ephemeral","ttl":"5m"}}]}"#.to_vec();
    assert_eq!(attach_message_cache_control(existing.clone(), &source), existing);
    let tool =
        br#"{"content":[{"type":"text","text":"before"},{"type":"tool_result","tool_use_id":"c","content":"result"}]}"#
            .to_vec();
    let output = value(&attach_tool_message_cache_control(tool, &source));
    assert!(output["content"][0].get("cache_control").is_none());
    assert_eq!(output["content"][1]["tool_use_id"], "c");
    assert_eq!(output["content"][1]["cache_control"]["ttl"], "1h");
}

#[test]
fn schema_normalization_preserves_constraints_and_merges_object_branches() {
    use crate::translation::common::normalize_claude_tool_input_schema as normalize;
    assert_eq!(value(&normalize(None)), json!({"type":"object","properties":{}}));
    let schema = br#"{"title":"input","properties":{"fixed":{"type":"string"}},"allOf":[{"type":"object","properties":{"added":{"type":"integer"}},"required":["added"]}],"required":["fixed"],"additionalProperties":false}"#;
    assert_eq!(
        value(&normalize(Some(schema))),
        json!({
            "title":"input", "type":"object", "properties":{"fixed":{"type":"string"},"added":{"type":"integer"}},
            "required":["fixed","added"], "additionalProperties":false
        })
    );
    let raw = br#"{"type":"object","properties":{"n":{"type":"integer","minimum":1}},"required":["n"],"additionalProperties":false}"#;
    assert_eq!(value(&normalize(Some(raw))), value(raw));
    let raw = br#"{"type":"object","properties":{"x":{"type":"string"}},"$schema":"https://json-schema.org/draft/2020-12/schema"}"#;
    let normalized = value(&crate::translation::codex_claude::normalize_tool_parameters(raw));
    assert_eq!(normalized["type"], "object");
    assert_eq!(normalized["properties"]["x"], json!({"type":"string"}));
    assert!(normalized.get("$schema").is_none());
}

#[test]
fn structured_output_instructions_keep_the_schema_and_labels() {
    use crate::translation::common::claude_structured_output_instruction as instruction;
    assert!(instruction(&gj::parse(b"{}")).is_empty());
    let object = String::from_utf8(instruction(&gj::parse(br#"{"type":"json_object"}"#))).unwrap();
    assert!(object.contains("JSON object"));
    let schema = br#"{"type":"json_schema","json_schema":{"name":"answer","description":"one result","schema":{"type":"object","required":["x"]}}}"#;
    let output = String::from_utf8(instruction(&gj::parse(schema))).unwrap();
    assert!(output.contains("answer") && output.contains("one result"));
    assert!(output.contains(r#"{"type":"object","required":["x"]}"#));
}

#[test]
fn tool_results_follow_their_calls_without_inventing_references() {
    use crate::translation::common::align_openai_tool_call_messages_with as align;
    let raw = |v: Value| serde_json::to_vec(&v).unwrap();
    let call = raw(json!({"role":"assistant","tool_calls":[
        {"id":"a","type":"function","function":{"name":"first","arguments":"{}"}},
        {"id":"b","type":"function","function":{"name":"second","arguments":"{}"}}
    ]}));
    let a = raw(json!({"role":"tool","tool_call_id":"a","content":"one"}));
    let b = raw(json!({"role":"tool","tool_call_id":"b","content":"two"}));
    assert_eq!(
        align(vec![call.clone(), b.clone(), a.clone()], &[]),
        vec![call.clone(), b.clone(), a.clone()]
    );
    let user = raw(json!({"role":"user","content":"next"}));
    assert_eq!(
        align(vec![call.clone(), user.clone(), b.clone(), a.clone()], &[]),
        vec![call.clone(), b.clone(), a.clone(), user.clone()]
    );
    let ambiguous = vec![call, user, b, a];
    assert_eq!(align(ambiguous.clone(), &[b"a".to_vec()]), ambiguous);
}

#[test]
fn model_and_reminder_helpers_preserve_user_text() {
    use crate::translation::common::{request_model_name, system_reminder_text};
    assert_eq!(
        request_model_name(br#"{"model":"client"}"#, br#"{"model":"upstream"}"#),
        b"client"
    );
    assert_eq!(request_model_name(b"{}", br#"{"model":"upstream"}"#), b"upstream");
    assert_eq!(
        system_reminder_text(b"keep <>&\ntext"),
        b"<system-reminder>\nkeep <>&\ntext\n</system-reminder>"
    );
    for (pattern, unsupported) in [
        (r"\p{L}", true),
        (r"\P{Greek}", true),
        (r"[a-z]+", false),
        (r"\\p{L}", false),
    ] {
        assert_eq!(
            crate::translation::common::has_unsupported_unicode_property_escape(pattern.as_bytes()),
            unsupported,
            "{pattern}"
        );
    }
    assert!(crate::translation::common::is_claude_code_attribution_text(
        b"  x-anthropic-billing-header: cc_version=1"
    ));
    assert!(!crate::translation::common::is_claude_code_attribution_text(
        b"User text mentioning Claude Code"
    ));
}
