//! Go goldens adjusted for forward ports past the frozen v8.0.35 reference
//! (docs/DIFFERENCES-FROM-GO.md). Each adjustment asserts that the golden still has the
//! shape Go recorded, so a re-recorded golden fails here instead of being edited twice.
#![allow(dead_code)] // Each test target uses only some adjustments.

use serde_json::Value;

/// GPT Image 2.6 Flare and Sunburst (CLIProxyAPI PR #7261, open upstream at 4d5658e):
/// Codex built-ins listed after `gpt-image-2.5`.
pub mod gpt_image_26 {
    use super::Value;

    pub const IDS: [&str; 2] = ["gpt-image-2.6-flare", "gpt-image-2.6-sunburst"];
    const DISPLAY_NAMES: [&str; 2] = ["GPT Image 2.6 Flare", "GPT Image 2.6 Sunburst"];

    /// Go's last GPT Image entry in a model definition list.
    const GO_LAST_DEFINITION: &str = r#"{"id":"gpt-image-2.5","object":"model","created":1704067200,"owned_by":"openai","type":"openai","display_name":"GPT Image 2.5","version":"gpt-image-2.5"}"#;

    /// The model definitions (`ModelInfo` JSON, Go's field order) of the two built-ins.
    fn definitions() -> Vec<String> {
        IDS.iter()
            .zip(DISPLAY_NAMES)
            .map(|(id, name)| {
                format!(
                    r#"{{"id":"{id}","object":"model","created":1704067200,"owned_by":"openai","type":"openai","display_name":"{name}","version":"{id}"}}"#
                )
            })
            .collect()
    }

    /// The Images API's supported-model error: the 2.6 models follow `gpt-image-2.5`.
    /// Text without the error passes unchanged.
    pub fn error_message(go: &str) -> String {
        if !go.contains("is not supported on /v1/images/generations or /v1/images/edits") {
            return go.to_owned();
        }
        let from = "gpt-image-2.5, grok-imagine-image,";
        assert!(go.contains(from), "Go's supported-model list changed: {go}");
        go.replace(from, &format!("gpt-image-2.5, {}, grok-imagine-image,", IDS.join(", ")))
    }

    /// A model definition list as raw JSON: the 2.6 entries follow `gpt-image-2.5`.
    pub fn definitions_raw(go: &str) -> String {
        assert_eq!(
            go.matches(GO_LAST_DEFINITION).count(),
            1,
            "Go's Codex built-ins changed"
        );
        let entries = definitions().join(",");
        go.replace(GO_LAST_DEFINITION, &format!("{GO_LAST_DEFINITION},{entries}"))
    }

    /// A model definition list: the 2.6 entries follow `gpt-image-2.5`.
    pub fn definitions_list(go: &mut Value) {
        let list = go.as_array_mut().expect("a model list");
        let last: Value = serde_json::from_str(GO_LAST_DEFINITION).unwrap();
        let at = list.iter().position(|m| *m == last).expect("Go lists gpt-image-2.5");
        for (offset, entry) in definitions().iter().enumerate() {
            list.insert(at + 1 + offset, serde_json::from_str(entry).unwrap());
        }
    }

    /// An OpenAI `/v1/models` list (`data` entries): the 2.6 entries are appended.
    pub fn openai_models(go: &mut Value) {
        let data = go["data"].as_array_mut().expect("an OpenAI model list");
        assert!(
            data.iter().any(|m| m["id"] == "gpt-image-2.5"),
            "Go lists gpt-image-2.5"
        );
        for id in IDS {
            assert!(data.iter().all(|m| m["id"] != id), "Go already lists {id}");
            data.push(serde_json::json!({
                "created": 1704067200,
                "id": id,
                "object": "model",
                "owned_by": "openai",
            }));
        }
    }
}

/// Grok Imagine Video 1.5 Lite (CLIProxyAPI v8.0.67 f0ddc38f and c69cdd93, #87): an
/// xAI built-in listed after the 1.5 preview alias.
pub mod grok_video_lite {
    use super::Value;

    pub const ID: &str = "grok-imagine-video-1.5-lite";

    /// The preview alias as Go's Grok Shell catalog lists it.
    const GO_PREVIEW_ENTRY: &str = r#"{"id":"grok-imagine-video-1.5-preview","model":"grok-imagine-video-1.5-preview","name":"Grok Imagine Video 1.5 Preview","api_backend":"responses","supported_in_api":true}"#;
    const ENTRY: &str = r#"{"id":"grok-imagine-video-1.5-lite","model":"grok-imagine-video-1.5-lite","name":"Grok Imagine Video 1.5 Lite","api_backend":"responses","supported_in_api":true}"#;

    /// The preview alias's model definition (`ModelInfo` JSON, Go's field order).
    const GO_PREVIEW_DEFINITION: &str = r#"{"id":"grok-imagine-video-1.5-preview","object":"model","created":1735689600,"owned_by":"xai","type":"xai","display_name":"Grok Imagine Video 1.5 Preview","name":"grok-imagine-video-1.5-preview","description":"Compatibility alias for the xAI Grok video generation model."}"#;
    const DEFINITION: &str = r#"{"id":"grok-imagine-video-1.5-lite","object":"model","created":1787702400,"owned_by":"xai","type":"xai","display_name":"Grok Imagine Video 1.5 Lite","name":"grok-imagine-video-1.5-lite","description":"xAI Grok video generation model."}"#;

    /// A Grok Shell catalog (`GET /v1/models` for the Grok CLI), which lists models by
    /// ID: Lite comes before the preview alias. Text without the catalog passes
    /// unchanged.
    pub fn grok_shell_catalog(go: &str) -> String {
        if !go.contains(GO_PREVIEW_ENTRY) {
            return go.to_owned();
        }
        assert_eq!(
            go.matches(GO_PREVIEW_ENTRY).count(),
            1,
            "Go's Grok Shell catalog changed"
        );
        assert!(!go.contains(ID), "Go already lists {ID}");
        go.replace(GO_PREVIEW_ENTRY, &format!("{ENTRY},{GO_PREVIEW_ENTRY}"))
    }

    /// `GET /openai/v1/videos/{id}` for a job created with the 1.5 preview alias whose
    /// poll answer omits the model: the created model (v8.0.67 f0ddc38f) where v8.0.35
    /// names the classic model.
    pub fn retrieve_fallback_model(go: &str) -> String {
        const GO_MODEL: &str = r#""model":"grok-imagine-video","#;
        assert_eq!(go.matches(GO_MODEL).count(), 1, "Go's retrieve answer changed: {go}");
        go.replace(GO_MODEL, r#""model":"grok-imagine-video-1.5","#)
    }

    /// The xAI channel's model definitions: Lite follows the preview alias.
    pub fn definitions_list(go: &mut Value) {
        let list = go.as_array_mut().expect("a model list");
        let preview: Value = serde_json::from_str(GO_PREVIEW_DEFINITION).unwrap();
        let at = list
            .iter()
            .position(|m| *m == preview)
            .expect("Go lists the 1.5 preview alias");
        assert!(list.iter().all(|m| m["id"] != ID), "Go already lists {ID}");
        list.insert(at + 1, serde_json::from_str(DEFINITION).unwrap());
    }
}
