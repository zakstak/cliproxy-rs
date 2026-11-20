//! `GET /v1/models?client_version=` through the real router. The catalog builder itself
//! replays Go vectors in protocol; this checks the route switch and that config model
//! fields reach it. Fake keys only; nothing is sent upstream.

use std::sync::Arc;

use cliproxy_internal::config::Config;
use cliproxy_internal::gateway::router;
use cliproxy_internal::providers::Executors;
use serde_json::Value;

async fn get(path: &str, cfg: &str) -> (u16, String, Value) {
    let dir = std::env::temp_dir().join(format!("cpa-catalog-contract-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = Config::parse(&format!("{cfg}\noauth: {{auth-dir: {}}}\n", dir.display())).unwrap();
    let credentials = cliproxy_internal::config::credentials::from_config(&cfg);
    let executors = Executors {
        claude: cliproxy_internal::providers::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cliproxy_internal::providers::codex::CodexExecutor::new().unwrap(),
        meta: Default::default(),
        xai: Default::default(),
    };
    let rt = Arc::new(cliproxy_internal::gateway::testing::runtime(
        cfg,
        credentials,
        executors,
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}{path}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router(rt)).await.unwrap() });
    let response = wreq::Client::new().get(url).send().await.unwrap();
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let body: Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    (status, content_type, body)
}

const CONFIG: &str = "codex-api-key:\n  - api-key: sk-FAKE-codex\n    base-url: http://127.0.0.1:1\n    models:\n      - name: gpt-5.5\nclaude-api-key:\n  - api-key: sk-FAKE-claude\n    models:\n      - name: claude-sonnet-4-6\n        alias: sonnet-long\n        max-context-length: 150000\nclient:\n  codex:\n    optimize-multi-agent-v2: true\n";

/// First-party metadata from openai/codex@6c0cee502d21,
/// codex-rs/models-manager/models.json, read 2026-11-17. Fake API-key defaults
/// must advertise both routing IDs and their own templates, not a synthesized GPT-5.5.
#[tokio::test]
async fn gpt_6_2_models_are_listed_with_first_party_client_templates() {
    let config = "codex-api-key:\n  - api-key: sk-FAKE-catalog\n    base-url: http://127.0.0.1:1\n";
    let (status, _, list) = get("/v1/models", config).await;
    assert_eq!(status, 200);
    for id in ["gpt-6.2-sol", "gpt-6.2-astra", "gpt-6.1-sol", "gpt-5.5"] {
        assert!(list["data"].as_array().unwrap().iter().any(|m| m["id"] == id), "{id}");
    }
    let (status, _, catalog) = get("/v1/models?client_version=0.166.0", config).await;
    assert_eq!(status, 200);
    for (id, context, priority) in [("gpt-6.2-sol", 500_000, 1), ("gpt-6.2-astra", 272_000, 2)] {
        let model = catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["slug"] == id)
            .unwrap();
        assert_eq!(model["context_window"], context);
        assert_eq!(model["max_context_window"], 872_000);
        assert_eq!(model["priority"], priority);
        assert_eq!(model["minimal_client_version"], "0.166.0");
        assert_eq!(model["input_modalities"], serde_json::json!(["text", "image"]));
        let efforts = model["supported_reasoning_levels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["effort"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(efforts, ["low", "medium", "high", "xhigh", "max", "ultra"]);
        assert_eq!(model["prefer_websockets"], true);
        assert_eq!(model["use_responses_lite"], true);
        assert_eq!(model["supports_reasoning_effort_updates"], true);
        assert_eq!(model["default_service_tier"], Value::Null);
        assert_eq!(
            model["service_tiers"],
            serde_json::json!([{"id":"priority","name":"Fast","description":"2x speed, increased usage"}])
        );
        assert!(model["base_instructions"].as_str().unwrap().len() > 1000);
        // First-party serialize_model_infos_with_legacy_base mirrors the canonical
        // literal instructions for clients that still read the deprecated field.
        assert_eq!(
            model["base_instructions"],
            model["model_messages"]["instructions_template"]
        );
    }
}

#[tokio::test]
async fn client_version_selects_the_codex_catalog() {
    let (status, content_type, body) = get("/v1/models?client_version=0.150.0", CONFIG).await;
    assert_eq!(status, 200);
    assert_eq!(content_type, "application/json; charset=utf-8");
    let models = body["models"].as_array().expect("Codex catalog shape");
    let slug = |s: &str| {
        models
            .iter()
            .find(|m| m["slug"] == s)
            .unwrap_or_else(|| panic!("no {s}"))
    };
    // gpt-5.5 keeps its catalog template (only Codex serves it); multi-agent v2 is on.
    let gpt = slug("gpt-5.5");
    assert_eq!(gpt["multi_agent_version"], "v2");
    assert!(gpt["base_instructions"].as_str().unwrap().len() > 1000);
    // M1-0033: the Claude key's max-context-length is the advertised window of the
    // synthesized entry, which carries compact instructions.
    let sonnet = slug("sonnet-long");
    assert_eq!(sonnet["context_window"], 150000);
    assert_eq!(sonnet["max_context_window"], 150000);
    assert_eq!(
        sonnet["base_instructions"],
        "You are Codex, a coding agent. You and the user share one workspace."
    );
    assert_eq!(sonnet["prefer_websockets"], false);
    assert!(sonnet["apply_patch_tool_type"].is_null());
    // Templates sort before synthesized entries.
    let position = |s: &str| models.iter().position(|m| m["slug"] == s).unwrap();
    assert!(position("gpt-5.5") < position("sonnet-long"));

    // An empty value still selects it; without the key the OpenAI list answers.
    let (_, _, body) = get("/v1/models?client_version", CONFIG).await;
    assert!(body["models"].is_array());
    let (_, _, body) = get("/v1/models", CONFIG).await;
    assert_eq!(body["object"], "list");
}

/// Configured aliases, capability bounds and first-party catalog templates are separate contracts.
#[tokio::test]
async fn configured_models_expose_aliases_reasoning_and_client_capabilities() {
    let config = format!("{CONFIG}    # model catalog contract\n");
    let config = config.replace("        max-context-length: 150000", "        max-context-length: 150000\n      - name: claude-opus-4-6\n        alias: opus-think\n        thinking:\n          levels: [low, high]");
    let templates: Value = serde_json::from_str(cliproxy_internal::protocol::codex_client::CLIENT_MODELS_JSON).unwrap();
    let template = templates["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["slug"] == "gpt-5.5")
        .unwrap();
    for version in ["0.150.0", "cpa", "0.143.0"] {
        let (status, _, body) = get(&format!("/v1/models?client_version={version}"), &config).await;
        assert_eq!(status, 200);
        let models = body["models"].as_array().unwrap();
        let mut slugs = models.iter().map(|m| m["slug"].as_str().unwrap()).collect::<Vec<_>>();
        slugs.sort_unstable();
        assert_eq!(slugs, ["gpt-5.5", "opus-think", "sonnet-long"]);
        let find = |slug: &str| models.iter().find(|m| m["slug"] == slug).unwrap();
        let codex = find("gpt-5.5");
        // This static template is the shipped first-party catalog, not recorded proxy output.
        for field in [
            "base_instructions",
            "model_messages",
            "input_modalities",
            "context_window",
            "supported_reasoning_levels",
        ] {
            assert_eq!(codex[field], template[field], "{version}: {field}");
        }
        assert_eq!(codex["multi_agent_version"], "v2");
        if version == "cpa" {
            assert_eq!(codex["cpa_capabilities"]["web_search"], true);
        } else {
            assert!(codex.get("cpa_capabilities").is_none());
        }
        let opus = find("opus-think");
        let efforts = opus["supported_reasoning_levels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["effort"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(efforts, ["low", "high"]);
        let sonnet = find("sonnet-long");
        assert_eq!(sonnet["context_window"], 150000);
        assert_eq!(sonnet["max_context_window"], 150000);
        for model in models {
            assert!(model["display_name"].is_string());
            assert!(model["priority"].is_i64());
            assert_eq!(model["multi_agent_version"], "v2");
            assert!(model["supported_in_api"].as_bool().unwrap());
        }
        assert_eq!(
            models[0]["slug"], "gpt-5.5",
            "first-party templates precede synthesized entries"
        );
    }
}

/// Native search requires every serving credential to support it, including mixed pools.
#[tokio::test]
async fn tool_search_catalog_requires_a_uniformly_capable_credential_pool() {
    let config = r#"
claude:
  tool-search: true
claude-api-key:
  - api-key: sk-FAKE-origin
    models:
      - {name: claude-sonnet-4-6, alias: sonnet-native}
      - {name: claude-sonnet-4-0, alias: sonnet-legacy}
  - api-key: sk-FAKE-trusted
    base-url: http://127.0.0.1:1
    native-tool-search: true
    models:
      - {name: claude-opus-4-6, alias: opus-trusted}
      - {name: claude-opus-4-6, alias: opus-mixed}
  - api-key: sk-FAKE-gateway
    base-url: http://127.0.0.1:1
    models:
      - {name: claude-haiku-4-5, alias: haiku-gateway}
      - {name: claude-opus-4-6, alias: opus-mixed}
"#;
    for enabled in [true, false] {
        let config = config.replace("tool-search: true", &format!("tool-search: {enabled}"));
        let (status, _, body) = get("/v1/models?client_version=0.150.0", &config).await;
        assert_eq!(status, 200);
        let models = body["models"].as_array().unwrap();
        assert_eq!(models.len(), 5);
        for (slug, capable) in [
            ("sonnet-native", true),
            ("sonnet-legacy", false),
            ("opus-trusted", true),
            ("opus-mixed", false),
            ("haiku-gateway", false),
        ] {
            let model = models.iter().find(|m| m["slug"] == slug).unwrap();
            assert_eq!(
                model["supports_search_tool"],
                enabled && capable,
                "{slug}, enabled={enabled}"
            );
        }
    }
}

/// Unconfigured Codex credentials advertise the supported catalog and image built-ins.
#[tokio::test]
async fn codex_builtin_catalog_has_unique_client_model_records() {
    let (status, _, body) = get(
        "/v1/models",
        "codex-api-key:\n  - api-key: sk-FAKE-builtins\n    base-url: http://127.0.0.1:1\n",
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["object"], "list");
    let entries = body["data"].as_array().unwrap();
    let ids = entries
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(ids.len(), entries.len(), "one record per model");
    for id in [
        "gpt-5.5",
        "gpt-6-sol",
        "gpt-6-luna",
        "gpt-6-astra",
        "gpt-6.1-sol",
        "gpt-6.1-terra",
        "gpt-image-1.5",
        "gpt-image-2",
        "gpt-image-2.6-flare",
        "gpt-image-2.6-sunburst",
    ] {
        assert!(ids.contains(id), "missing supported built-in {id}");
    }
    for entry in entries {
        assert_eq!(entry["object"], "model");
        assert_eq!(entry["owned_by"], "openai");
        assert!(entry["created"].as_i64().is_some_and(|t| t > 0));
    }
}

/// M1-0128 `GET /v1/models/*model`: the entry from the catalog this client would list,
/// and Go's JSON 404 for an unknown or empty ID.
#[tokio::test]
async fn model_detail_comes_from_the_clients_catalog() {
    let (_, _, codex) = get("/v1/models?client_version=0.150.0", CONFIG).await;
    let (status, content_type, body) = get("/v1/models/sonnet-long?client_version=0.150.0", CONFIG).await;
    assert_eq!(
        (status, content_type.as_str()),
        (200, "application/json; charset=utf-8")
    );
    let listed = codex["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["slug"] == "sonnet-long");
    assert_eq!(Some(&body), listed);
    let (_, _, openai) = get("/v1/models", CONFIG).await;
    let (status, _, mut body) = get("/v1/models/sonnet-long", CONFIG).await;
    assert_eq!(status, 200);
    let mut listed = openai["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "sonnet-long")
        .unwrap()
        .clone();
    // A config model is `created` at the request's second; the two requests can
    // straddle a second boundary.
    let (got, want) = (body["created"].take(), listed["created"].take());
    assert!(got.as_i64().unwrap() - want.as_i64().unwrap() <= 1, "{got} {want}");
    assert!(got.as_i64() >= want.as_i64(), "{got} {want}");
    assert_eq!(body, listed);
    assert!(body.get("slug").is_none(), "the OpenAI entry, not the Codex one");
    let missing = serde_json::json!({"error": {"code": "model_not_found", "message": "Model not found", "type": "invalid_request_error"}});
    for path in ["/v1/models/nope", "/v1/models/", "/v1/models/sonnet-long/extra"] {
        let (status, content_type, body) = get(path, CONFIG).await;
        assert_eq!(
            (status, content_type.as_str(), &body),
            (404, "application/json; charset=utf-8", &missing),
            "{path}"
        );
    }
}
