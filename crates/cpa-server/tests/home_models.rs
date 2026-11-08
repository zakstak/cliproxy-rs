//! Home-mode model listings through the real router, with a fake control plane that
//! answers every catalog request with one fixed payload (Go
//! internal/api/home_models_empty_test.go). Nothing is sent upstream.

use std::sync::Arc;

use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_server::remote::{ModelsError, RemoteDispatch, RemoteError, RemoteGrant, RemoteRequest};
use futures_util::future::BoxFuture;
use serde_json::Value;

struct Catalog(&'static str);

impl RemoteDispatch for Catalog {
    fn available(&self) -> bool {
        true
    }

    fn dispatch(&self, _: RemoteRequest) -> BoxFuture<'_, Result<RemoteGrant, RemoteError>> {
        unreachable!("model listings never dispatch")
    }

    fn models(
        &self,
        _: Vec<(String, String)>,
        _: Vec<(String, String)>,
    ) -> BoxFuture<'_, Result<Vec<u8>, ModelsError>> {
        Box::pin(async move { Ok(self.0.as_bytes().to_vec()) })
    }
}

/// A Home-mode server whose control plane answers `payload`; a local credential is
/// registered so a local catalog could leak into the answer. Its auth dir is private
/// and empty.
async fn serve(payload: &'static str) -> String {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let auth = std::env::temp_dir().join(format!(
        "home-models-auth-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&auth).unwrap();
    let cfg = Config::parse(&format!(
        "auth-dir: {}\ncodex-api-key:\n  - api-key: sk-FAKE-codex\n    base-url: http://127.0.0.1:1\n    models:\n      - name: local-only-model\n",
        auth.display()
    ))
    .unwrap();
    let credentials = cpa_core::config::credentials::from_config(&cfg);
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: Default::default(),
        meta: Default::default(),
        xai: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(cfg, credentials, executors));
    rt.set_remote_dispatch(Some(Arc::new(Catalog(payload))));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, cpa_server::router(rt)).await.unwrap() });
    base
}

async fn get(base: &str, path: &str, headers: &[(&str, &str)]) -> (u16, Value) {
    let mut request = wreq::Client::new()
        .get(format!("{base}{path}"))
        .header("authorization", "Bearer scoped-key");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.bytes().await.unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&bytes)));
    (status, body)
}

/// Go `TestHomeEmptyModelCatalogResponses`: an empty catalog lists no models in every
/// shape, without falling back to local models, and a model lookup is not found.
// ponytail: Go's codex and cpa `client_version` cases (`models: []`) are not here:
// Home-mode Codex clients get the OpenAI list (see models.rs).
#[tokio::test]
async fn home_empty_model_catalog_responses() {
    let base = serve("{}").await;
    for (name, path, headers, field) in [
        ("openai", "/v1/models", &[][..], "data"),
        (
            "claude",
            "/v1/models",
            &[("anthropic-version", "2023-06-01")][..],
            "data",
        ),
        ("gemini", "/v1beta/models", &[][..], "models"),
        ("grok", "/v1/models", &[("user-agent", "grok-shell")][..], "data"),
    ] {
        let (status, body) = get(&base, path, headers).await;
        assert_eq!(status, 200, "{name}: {body}");
        assert_eq!(body[field], Value::Array(Vec::new()), "{name}: {body}");
    }
    let (status, body) = get(&base, "/v1beta/models/inaccessible-model", &[]).await;
    assert_eq!(status, 404, "{body}");
}

/// Go `TestHomeModelCatalogErrorsRemainErrors`: error envelopes, with any shape, and
/// malformed catalogs answer an error, never a (possibly empty) list.
#[tokio::test]
async fn home_model_catalog_errors_remain_errors() {
    for (name, payload, want) in [
        (
            "missing credentials",
            r#"{"error":{"type":"no_credentials","message":"Missing API key"}}"#,
            401,
        ),
        (
            "invalid credentials",
            r#"{"error":{"type":"invalid_credential","message":"Invalid API key"}}"#,
            401,
        ),
        (
            "policy failure",
            r#"{"error":{"type":"internal_error","message":"Model access service error"}}"#,
            502,
        ),
        ("empty array error", r#"{"error":[]}"#, 502),
        ("error alongside empty section", r#"{"error":[],"claude":[]}"#, 502),
        (
            "error containing model-shaped records",
            r#"{"error":[{"id":"not-a-model"}]}"#,
            502,
        ),
        (
            "error alongside models",
            r#"{"error":[],"claude":[{"id":"model-a"}]}"#,
            502,
        ),
        ("empty response", "", 502),
        ("malformed JSON", "{bad}", 502),
        ("null catalog", "null", 502),
        ("null section", r#"{"claude":null}"#, 502),
        ("missing model IDs", r#"{"claude":[{}]}"#, 502),
    ] {
        let base = serve(payload).await;
        let (status, body) = get(&base, "/v1/models", &[]).await;
        assert_eq!(status, want, "{name}: {body}");
        assert!(body.get("error").is_some(), "{name}: {body}");
    }
}
