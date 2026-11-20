//! Claude translation prefers this request's registration and falls back to the pinned catalog.
use cliproxy_internal::translation::{RequestCtx, translate_request};
use cliproxy_internal::{
    catalog::dynamic::{Registry, Spec},
    format::Format,
};
use serde_json::{Value, json};

fn thinking(registry: Option<&Registry>, model: &str, effort: &str) -> (Value, Value) {
    let body = format!(r#"{{"reasoning_effort":"{effort}","messages":[{{"role":"user","content":"hi"}}]}}"#);
    let out = translate_request(
        Format::OpenAI,
        Format::Claude,
        &RequestCtx {
            registry,
            model,
            stream: false,
            tool_search: false,
            system_turns: false,
        },
        body.as_bytes(),
    )
    .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    (v["thinking"].clone(), v["output_config"].clone())
}

#[test]
fn dynamic_registry_overrides_static_capabilities() {
    let mut registry = Registry::default();
    let custom = "custom-adaptive";
    let budget = "claude-sonnet-4-5-20250929";
    for (id, levels) in [
        (custom, vec!["low", "high", "max"]),
        (budget, vec!["low", "medium", "high"]),
    ] {
        let spec: Spec = serde_json::from_value(json!({"id": id, "thinking": {"levels": levels}})).unwrap();
        registry.register(id, "claude", vec![spec]);
    }
    // A later registration for a different provider cannot override the Claude one.
    registry.register(
        "other",
        "codex",
        vec![Spec {
            id: custom.into(),
            ..Default::default()
        }],
    );
    for model in [custom, budget] {
        assert_eq!(
            thinking(None, model, "xhigh"),
            (json!({"type":"enabled","budget_tokens":32768}), Value::Null)
        );
    }
    for (model, effort) in [(custom, "max"), (budget, "high"), ("gpt-5.5", "xhigh")] {
        assert_eq!(
            thinking(Some(&registry), model, "xhigh"),
            (json!({"type":"adaptive"}), json!({"effort":effort}))
        );
    }
}
