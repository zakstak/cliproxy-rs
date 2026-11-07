//! The Codex GPT Image built-ins through the real router: GPT Image 2.5 (Go v8.0.35) and
//! the GPT Image 2.6 Flare and Sunburst forward port (CLIProxyAPI PR #7261, open upstream
//! at 4d5658e; docs/DIFFERENCES-FROM-GO.md). The model list, the Codex client catalog,
//! the direct Images path for generations and edits, the supported-model error and the
//! image-only rejection on text routes. Fake credentials, a private `auth-dir` and a
//! loopback upstream only.

use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::http::{HeaderMap, Uri};
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_exec::Executors;
use serde_json::Value;

/// The GPT Image 2.5 and 2.6 built-ins with their display names.
const BUILTINS: [(&str, &str); 5] = [
    ("gpt-image-2.5-flare", "GPT Image 2.5 Flare"),
    ("gpt-image-2.5-sunburst", "GPT Image 2.5 Sunburst"),
    ("gpt-image-2.5", "GPT Image 2.5"),
    ("gpt-image-2.6-flare", "GPT Image 2.6 Flare"),
    ("gpt-image-2.6-sunburst", "GPT Image 2.6 Sunburst"),
];

/// Client model names and the model the direct Images endpoint receives: the routing
/// prefix and case go. The Images handler, like Go's `imagesModelBase`, refuses a
/// thinking suffix before the executor could strip it.
const ROUTED: [(&str, &str); 9] = [
    ("gpt-image-2.5", "gpt-image-2.5"),
    ("gpt-image-2.5-flare", "gpt-image-2.5-flare"),
    ("codex/gpt-image-2.5-sunburst", "gpt-image-2.5-sunburst"),
    ("gpt-image-2.6-flare", "gpt-image-2.6-flare"),
    ("gpt-image-2.6-sunburst", "gpt-image-2.6-sunburst"),
    ("codex/gpt-image-2.6-flare", "gpt-image-2.6-flare"),
    ("codex/gpt-image-2.6-sunburst", "gpt-image-2.6-sunburst"),
    ("GPT-Image-2.6-Flare", "gpt-image-2.6-flare"),
    ("codex/GPT-Image-2.6-Sunburst", "gpt-image-2.6-sunburst"),
];

/// The upstream PR deliberately adds no bare `gpt-image-2.6` alias.
const BARE_26: &str = "gpt-image-2.6";

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

/// A private, empty `auth-dir`, removed when dropped: credential loading never falls back
/// to `~/.cli-proxy-api`.
struct AuthDir(std::path::PathBuf);

impl AuthDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("cpa-it-auth-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn config(&self, extra: &str) -> Config {
        Config::parse(&format!(
            "oauth:\n  auth-dir: {}\naccess:\n  api-keys: [client-key]\n{extra}",
            self.0.display()
        ))
        .unwrap()
    }
}

impl Drop for AuthDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn executors() -> Executors {
    Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    }
}

/// A fake Codex OAuth credential on a paid plan whose backend is `base_url`. Its
/// `prefix` makes `codex/<model>` resolve to it, as well as the bare model.
fn codex_oauth(base_url: &str) -> Credential {
    let metadata = serde_json::json!({
        "type": "codex",
        "prefix": "codex",
        "access_token": "at-FAKE-codex",
        "account_id": "acct",
        "expired": "2099-01-01T00:00:00Z",
    });
    let mut credential = Credential::from_file(
        Path::new("/fake"),
        &Path::new("/fake").join("codex-plus.json"),
        metadata.as_object().unwrap().clone(),
    )
    .unwrap();
    for (key, value) in [("base_url", base_url), ("plan_type", "plus")] {
        credential.attributes.insert(key.into(), value.into());
    }
    credential
}

/// One request the mock backend saw: path, content type, body.
type Seen = Arc<Mutex<Vec<(String, String, String)>>>;

/// A proxy whose one Codex OAuth credential talks to a recording mock backend.
async fn proxy(auth_dir: &AuthDir) -> (String, Seen) {
    let seen: Seen = Arc::default();
    let record = seen.clone();
    let upstream = serve(
        axum::Router::new().fallback(move |uri: Uri, headers: HeaderMap, body: axum::body::Bytes| {
            let record = record.clone();
            async move {
                let content_type = headers
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                record.lock().unwrap().push((
                    uri.path().to_owned(),
                    content_type,
                    String::from_utf8_lossy(&body).into_owned(),
                ));
                (
                    [("content-type", "application/json")],
                    r#"{"created":1,"data":[{"b64_json":"aGk="}]}"#,
                )
            }
        }),
    )
    .await;
    let cfg = auth_dir.config("");
    let rt = Arc::new(cpa_server::testing::runtime(
        cfg,
        vec![codex_oauth(&upstream)],
        executors(),
    ));
    (serve(cpa_server::router(rt)).await, seen)
}

async fn post(url: String, content_type: &str, body: Vec<u8>) -> (u16, String) {
    let response = wreq::Client::new()
        .post(url)
        .header("authorization", "Bearer client-key")
        .header("content-type", content_type)
        .body(body)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.text().await.unwrap())
}

async fn get(url: String) -> Value {
    let response = wreq::Client::new()
        .get(url)
        .header("authorization", "Bearer client-key")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    serde_json::from_slice(&response.bytes().await.unwrap()).unwrap()
}

fn edit_form(model: &str) -> (String, Vec<u8>) {
    let boundary = "cpa-image-boundary";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n{model}\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nadd a moon\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"in.png\"\r\n\
         Content-Type: image/png\r\n\r\nPNGDATA\r\n--{boundary}--\r\n"
    );
    (format!("multipart/form-data; boundary={boundary}"), body.into_bytes())
}

/// Generations and edits for each GPT Image 2.5 and 2.6 name reach the backend's own
/// Images endpoints with the plain model, never the Responses image tool.
#[tokio::test]
async fn generations_and_edits_take_the_direct_images_path() {
    let auth_dir = AuthDir::new();
    let (proxy, seen) = proxy(&auth_dir).await;
    for (model, upstream_model) in ROUTED {
        seen.lock().unwrap().clear();
        let body = serde_json::json!({"model": model, "prompt": "a lighthouse"}).to_string();
        let (status, text) = post(
            format!("{proxy}/v1/images/generations"),
            "application/json",
            body.into_bytes(),
        )
        .await;
        assert_eq!(status, 200, "{model} generation: {text}");
        let (content_type, form) = edit_form(model);
        let (status, text) = post(format!("{proxy}/v1/images/edits"), &content_type, form).await;
        assert_eq!(status, 200, "{model} edit: {text}");

        let seen = seen.lock().unwrap().clone();
        let paths: Vec<&str> = seen.iter().map(|(path, _, _)| path.as_str()).collect();
        assert_eq!(paths, ["/images/generations", "/images/edits"], "{model}");
        for (path, content_type, body) in &seen {
            assert!(
                content_type.starts_with("application/json"),
                "{model} {path}: {content_type}"
            );
            let body: Value = serde_json::from_str(body).unwrap();
            assert_eq!(body["model"], upstream_model, "{model} {path}");
        }
    }
}

/// The supported-model error lists the 2.6 models after the 2.5 ones; the bare
/// `gpt-image-2.6` alias stays unsupported.
#[tokio::test]
async fn unsupported_image_models_list_the_gpt_image_built_ins() {
    let auth_dir = AuthDir::new();
    let (proxy, seen) = proxy(&auth_dir).await;
    for model in [BARE_26, "dall-e-3"] {
        let body = serde_json::json!({"model": model, "prompt": "a lighthouse"}).to_string();
        let (status, text) = post(
            format!("{proxy}/v1/images/generations"),
            "application/json",
            body.into_bytes(),
        )
        .await;
        assert_eq!(status, 400, "{model}: {text}");
        let error: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            error["error"]["message"],
            format!(
                "Model {model} is not supported on /v1/images/generations or /v1/images/edits. \
                 Use gpt-image-1.5, gpt-image-2, gpt-image-2.5-flare, gpt-image-2.5-sunburst, \
                 gpt-image-2.5, gpt-image-2.6-flare, gpt-image-2.6-sunburst, grok-imagine-image, \
                 grok-imagine-image-quality, grok-imagine-image-2.0, or a configured \
                 openai-compatibility image model."
            )
        );
        assert_eq!(error["error"]["type"], "invalid_request_error");
    }
    assert!(seen.lock().unwrap().is_empty(), "nothing went upstream");
}

/// Chat Completions and Responses refuse every GPT Image built-in, prefixed or not, as
/// they refuse the other image-only models, before any upstream request.
#[tokio::test]
async fn text_routes_reject_the_image_only_models() {
    let auth_dir = AuthDir::new();
    let (proxy, seen) = proxy(&auth_dir).await;
    for (model, base) in BUILTINS
        .iter()
        .flat_map(|(id, _)| [(id.to_string(), *id), (format!("codex/{id}"), *id)])
    {
        let chat = serde_json::json!({"model": model, "messages": [{"role": "user", "content": "hi"}]});
        let responses = serde_json::json!({"model": model, "input": "hi"});
        for (path, body) in [("/v1/chat/completions", chat), ("/v1/responses", responses)] {
            let (status, text) = post(
                format!("{proxy}{path}"),
                "application/json",
                body.to_string().into_bytes(),
            )
            .await;
            assert_eq!(status, 503, "{model} on {path}: {text}");
            assert!(
                text.contains(&format!(
                    "model {base} is only supported on /v1/images/generations and /v1/images/edits"
                )),
                "{model} on {path}: {text}"
            );
        }
    }
    assert!(seen.lock().unwrap().is_empty(), "nothing went upstream");
}

/// `/v1/models` lists the built-ins for a Codex credential; the Codex client catalog
/// carries them hidden, so they never show as text models.
#[tokio::test]
async fn models_list_the_built_ins_and_the_codex_catalog_hides_them() {
    let auth_dir = AuthDir::new();
    let (proxy, _) = proxy(&auth_dir).await;
    let list = get(format!("{proxy}/v1/models")).await;
    let ids: Vec<&str> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    for (id, _) in BUILTINS {
        assert!(ids.contains(&id), "{id} listed: {ids:?}");
    }
    assert!(!ids.contains(&BARE_26), "no bare {BARE_26}");

    let catalog = get(format!("{proxy}/v1/models?client_version=0.150.0")).await;
    let models = catalog["models"].as_array().unwrap();
    for (id, display_name) in BUILTINS {
        let entry = models
            .iter()
            .find(|m| m["slug"] == id)
            .unwrap_or_else(|| panic!("{id} in the Codex catalog"));
        assert_eq!(entry["visibility"], "hide", "{id}");
        assert_eq!(entry["display_name"], display_name, "{id}");
        assert!(entry["apply_patch_tool_type"].is_null(), "{id}");
    }
    assert!(models.iter().all(|m| m["slug"] != BARE_26), "no bare {BARE_26}");
}

/// The built-ins follow the Codex registration rules: a key without models lists them,
/// `excluded-models` removes matching ones, and a key with configured models lists only
/// those.
#[tokio::test]
async fn built_ins_follow_codex_key_registration_and_exclusions() {
    let listed = |extra: &str| {
        let auth_dir = AuthDir::new();
        let cfg = auth_dir.config(extra);
        async move {
            let credentials = cpa_core::config::credentials::from_config(&cfg);
            let rt = Arc::new(cpa_server::testing::runtime(cfg, credentials, executors()));
            let proxy = serve(cpa_server::router(rt)).await;
            let list = get(format!("{proxy}/v1/models")).await;
            drop(auth_dir);
            let ids: Vec<String> = list["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["id"].as_str().unwrap().to_owned())
                .collect();
            ids
        }
    };
    let key = "codex-api-key:\n  - api-key: sk-FAKE-codex\n    base-url: http://127.0.0.1:1\n";

    let defaults = listed(key).await;
    for (id, _) in BUILTINS {
        assert!(defaults.iter().any(|m| m == id), "{id} by default: {defaults:?}");
    }

    let excluded = listed(&format!("{key}    excluded-models: ['gpt-image-2.6-*']\n")).await;
    for (id, _) in BUILTINS {
        let want = !id.starts_with("gpt-image-2.6-");
        assert_eq!(excluded.iter().any(|m| m == id), want, "{id}: {excluded:?}");
    }

    let configured = listed(&format!("{key}    models:\n      - name: gpt-5.5\n")).await;
    for (id, _) in BUILTINS {
        assert!(
            !configured.iter().any(|m| m == id),
            "{id} with configured models: {configured:?}"
        );
    }
}
