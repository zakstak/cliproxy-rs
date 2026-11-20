//! Catalog schema and capability behavior. Payload contracts remain in docs/codex/CONTRACT.md.
use super::*;
use serde_json::{Value, json};

struct CatalogRegistry;
impl Registry for CatalogRegistry {
    fn lookup(&self, _: &str, _: Option<&str>) -> Option<ModelFacts> {
        None
    }
    fn providers(&self, _: &str) -> Vec<String> {
        vec!["claude".into()]
    }
    fn web_search(&self, _: &str) -> Option<bool> {
        Some(false)
    }
}

fn model() -> Value {
    json!({"slug":"custom","display_name":"Custom","description":"Description","base_instructions":"Instructions","minimal_client_version":"0","visibility":"list","default_reasoning_level":"low","context_window":1024,"max_context_window":2048,"priority":0,"supported_reasoning_levels":[{"effort":"low"}]})
}

#[test]
fn catalog_reports_available_models_and_version_supported_capabilities() {
    let available = [json!({"id":"custom","display_name":"Operator model","description":"Operator description","max_context_length":16384,"thinking":{"levels":["low","medium","max","ultra"]}}), json!({"id":"gpt-image-2.5"}), json!({"id":""})]
        .map(|v| GoValue::from_json(&v).into_object().unwrap());
    for version in ["0.143.99", "0.144.0"] {
        let output = build_response(&available, &CatalogRegistry, None, None, true, version);
        let output: Value = serde_json::from_slice(&marshal_compact(&output)).unwrap();
        let models = output["models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        let custom = models.iter().find(|m| m["slug"] == "custom").unwrap();
        assert_eq!(custom["display_name"], "Operator model");
        assert_eq!(custom["description"], "Operator description");
        assert_eq!(custom["context_window"], 16384);
        assert_eq!(custom["multi_agent_version"], "v2");
        let efforts = custom["supported_reasoning_levels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["effort"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(efforts.contains(&"low") && efforts.contains(&"medium"));
        assert_eq!(efforts.contains(&"max"), version == "0.144.0");
        assert_eq!(efforts.contains(&"ultra"), version == "0.144.0");
        assert_eq!(
            models.iter().find(|m| m["slug"] == "gpt-image-2.5").unwrap()["visibility"],
            "hide"
        );
    }
}

#[test]
fn catalog_validation_checks_schema_and_relations_without_pinning_error_prose() {
    assert!(validate(&serde_json::to_vec(&json!({"models":[model()]})).unwrap()).is_ok());
    assert!(validate(crate::protocol::codex_client::CLIENT_MODELS_JSON.as_bytes()).is_ok());
    for bad in [
        json!({}),
        json!({"models":[]}),
        json!({"models":{}}),
        json!({"models":[model(),model()]}),
    ] {
        assert!(validate(&serde_json::to_vec(&bad).unwrap()).is_err());
    }
    for key in [
        "slug",
        "display_name",
        "description",
        "base_instructions",
        "minimal_client_version",
        "visibility",
        "default_reasoning_level",
        "context_window",
        "max_context_window",
        "priority",
        "supported_reasoning_levels",
    ] {
        let mut missing = model();
        missing.as_object_mut().unwrap().remove(key);
        assert!(
            validate(&serde_json::to_vec(&json!({"models":[missing]})).unwrap()).is_err(),
            "missing {key}"
        );
    }
    for (key, value) in [
        ("context_window", json!(0)),
        ("context_window", json!(4096)),
        ("priority", json!(-1)),
        ("priority", json!(0.5)),
        ("default_reasoning_level", json!("unknown")),
        ("supported_reasoning_levels", json!([{"effort":"low"},{"effort":"low"}])),
    ] {
        let mut invalid = model();
        invalid[key] = value;
        assert!(
            validate(&serde_json::to_vec(&json!({"models":[invalid]})).unwrap()).is_err(),
            "invalid {key}"
        );
    }
    assert!(validate(b"{").is_err());
}

#[test]
fn extended_reasoning_levels_start_at_the_client_capability_version() {
    for (version, enabled) in [
        ("0.143.99", false),
        ("0.143", false),
        ("0.144", true),
        ("0.144.0", true),
        ("v0.144.0-rc1", true),
        ("0.145.0", true),
    ] {
        assert_eq!(extended_levels(version), enabled, "{version}");
    }
}

/// Go `isCodexClientImageOrVideoModel`, with the GPT Image 2.6 IDs of CLIProxyAPI
/// PR #7261 (docs/DIFFERENCES-FROM-GO.md): hidden from the client catalog, prefixed or
/// not; the bare `gpt-image-2.6` is no known image model.
#[test]
fn gpt_image_built_ins_are_hidden_image_models() {
    for id in [
        "gpt-image-2.5-flare",
        "gpt-image-2.5-sunburst",
        "gpt-image-2.5",
        "gpt-image-2.6-flare",
        "gpt-image-2.6-sunburst",
    ] {
        assert!(image_or_video(id), "{id}");
        assert!(image_or_video(&format!("codex/{id}")), "codex/{id}");
        let mut entry = Map::new();
        apply_visibility(&mut entry, id);
        assert!(
            matches!(entry.get("visibility"), Some(GoValue::String(s)) if s == "hide"),
            "{id}"
        );
    }
    assert!(!image_or_video("gpt-image-2.6"));
}

/// Grok Imagine Video 1.5 Lite (#87) is hidden from Codex clients like the other xAI
/// video models. Go v8.0.67 added the model without hiding it (docs/DIFFERENCES-FROM-GO.md).
#[test]
fn grok_video_lite_is_a_hidden_video_model() {
    for id in [
        "grok-imagine-video",
        "grok-imagine-video-1.5",
        "grok-imagine-video-1.5-preview",
        "grok-imagine-video-1.5-lite",
    ] {
        assert!(image_or_video(id), "{id}");
        assert!(image_or_video(&format!("xai/{id}")), "xai/{id}");
    }
    assert!(!image_or_video("grok-imagine-video-2-lite"));
}
