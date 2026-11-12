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

/// K1 (#68), retained model catalog additions: Claude Fable 5.2 after Fable 5.1, GPT-6.1
/// Terra after GPT-6.1 Sol in the Codex catalog, and Muse Ember 1.0 first in the Meta
/// catalog. Each expected definition is written out from its provider source (see
/// docs/DIFFERENCES-FROM-GO.md, "Retained model catalog additions"), in Go's `ModelInfo`
/// field order, and is not read from the embedded catalog.
pub mod k1_catalog {
    use super::Value;

    const FABLE_51: &str = r#"{"id":"claude-fable-5-1","object":"model","created":1788220800,"owned_by":"anthropic","type":"claude","display_name":"Claude Fable 5.1","description":"Claude Fable 5.1 extends Claude Fable 5 at the same input and output prices, with cache reads at a quarter of the cost, and brings stronger long-running agentic coding, multistep research, and document, spreadsheet, and slide work.","context_length":1000000,"max_completion_tokens":128000,"supportedInputModalities":["text","image"],"supportedOutputModalities":["text"],"thinking":{"dynamic_allowed":true,"levels":["low","medium","high","xhigh","max"]}}"#;
    /// platform.claude.com Fable 5.2 page and effort page, retrieved 2026-11-12.
    pub const FABLE_52: &str = r#"{"id":"claude-fable-5-2","object":"model","created":1794250800,"owned_by":"anthropic","type":"claude","display_name":"Claude Fable 5.2","description":"For demanding reasoning and long-horizon agentic work","context_length":1000000,"max_completion_tokens":128000,"supportedInputModalities":["text","image"],"supportedOutputModalities":["text"],"thinking":{"dynamic_allowed":true,"levels":["low","medium","high","xhigh","max"]}}"#;
    const SOL_61: &str = r#"{"id":"gpt-6.1-sol","object":"model","created":1790640000,"owned_by":"openai","type":"openai","display_name":"GPT 6.1 Sol","version":"gpt-6.0","description":"Latest workhorse model for coding and everyday work.","context_length":272000,"max_completion_tokens":128000,"supported_parameters":["tools"],"supportedInputModalities":["text","image"],"supportedOutputModalities":["text"],"thinking":{"levels":["low","medium","high","xhigh","max"]}}"#;
    /// First-party openai/codex models-manager/models.json at 44ccf394573c; no maximum
    /// output is stated there, so none is listed.
    pub const TERRA_61: &str = r#"{"id":"gpt-6.1-terra","object":"model","created":1792972800,"owned_by":"openai","type":"openai","display_name":"GPT 6.1 Terra","version":"gpt-6.0","description":"GPT-6.1 Terra Codex model.","context_length":500000,"supported_parameters":["tools"],"supportedInputModalities":["text","image"],"supportedOutputModalities":["text"],"thinking":{"levels":["low","medium","high","xhigh","max"]}}"#;
    /// dev.meta.ai/docs/muse-ember and /docs/reasoning, retrieved 2026-11-12.
    pub const EMBER_10: &str = r#"{"id":"muse-ember-1.0","object":"model","created":1794182400,"owned_by":"meta","type":"meta","display_name":"Muse Ember 1.0","name":"muse-ember-1.0","description":"Meta's smallest, most efficient model, for high-volume agent work.","context_length":1048576,"max_completion_tokens":524288,"supportedInputModalities":["text","image","video","audio"],"supportedOutputModalities":["text"],"thinking":{"levels":["minimal","low","medium","high","xhigh","max"]}}"#;

    /// `(anchor, addition)` for a channel's definitions: the addition follows the anchor,
    /// or comes first when the anchor is `None`. `None` for a channel K1 leaves alone.
    fn plan(channel: &str) -> Option<(Option<&'static str>, &'static str)> {
        match channel {
            "claude" => Some((Some(FABLE_51), FABLE_52)),
            "codex" => Some((Some(SOL_61), TERRA_61)),
            "meta" => Some((None, EMBER_10)),
            _ => None,
        }
    }

    fn id(entry: &str) -> String {
        serde_json::from_str::<Value>(entry).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// A channel's model definition list.
    pub fn definitions_list(channel: &str, go: &mut Value) {
        let Some((anchor, addition)) = plan(channel) else {
            return;
        };
        let list = go.as_array_mut().expect("a model list");
        let new_id = id(addition);
        assert!(
            list.iter().all(|m| m["id"] != new_id.as_str()),
            "Go already lists {new_id}"
        );
        let at = match anchor {
            None => 0,
            Some(anchor) => {
                let anchor: Value = serde_json::from_str(anchor).unwrap();
                1 + list
                    .iter()
                    .position(|m| *m == anchor)
                    .expect("Go's anchor definition changed")
            }
        };
        list.insert(at, serde_json::from_str(addition).unwrap());
    }

    /// A `{"channel":…,"models":[…]}` definitions response as raw JSON.
    pub fn definitions_raw(channel: &str, go: &str) -> String {
        let Some((anchor, addition)) = plan(channel) else {
            return go.to_owned();
        };
        assert!(
            !go.contains(&format!(r#""id":"{}""#, id(addition))),
            "Go already lists it"
        );
        match anchor {
            Some(anchor) => {
                assert_eq!(go.matches(anchor).count(), 1, "Go's anchor definition changed");
                go.replace(anchor, &format!("{anchor},{addition}"))
            }
            None => {
                let head = r#""models":["#;
                assert_eq!(go.matches(head).count(), 1);
                go.replace(head, &format!("{head}{addition},"))
            }
        }
    }

    /// An OpenAI `/v1/models` list served by a Codex credential: Terra is appended.
    pub fn codex_openai_models(go: &mut Value) {
        let data = go["data"].as_array_mut().expect("an OpenAI model list");
        assert!(data.iter().any(|m| m["id"] == "gpt-6.1-sol"), "Go lists gpt-6.1-sol");
        assert!(
            data.iter().all(|m| m["id"] != "gpt-6.1-terra"),
            "Go already lists Terra"
        );
        data.push(serde_json::json!({
            "created": 1792972800,
            "id": "gpt-6.1-terra",
            "object": "model",
            "owned_by": "openai",
        }));
    }
}
