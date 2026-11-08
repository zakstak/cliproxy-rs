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
        meta: Default::default(),
        xai: Default::default(),
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

/// The Codex Images path reads the client's JSON without coercion (#22,
/// docs/codex/CONTRACT.md "Request boundary"): the handler refuses a `stream` that is not
/// a boolean before it rewrites `stream`, and the adapter refuses a body that is not
/// UTF-8 or repeats a key, before any upstream request. Go streams for `"stream":"true"`
/// and forwards the others. A valid body still reaches the backend.
#[tokio::test]
async fn codex_images_json_meets_the_request_boundary() {
    let auth_dir = AuthDir::new();
    let (proxy, seen) = proxy(&auth_dir).await;
    for (path, body) in [
        (
            "/v1/images/generations",
            br#"{"model":"gpt-image-2","prompt":"p","stream":"true"}"#.to_vec(),
        ),
        (
            "/v1/images/edits",
            br#"{"model":"gpt-image-2","prompt":"p","stream":1}"#.to_vec(),
        ),
        (
            "/v1/images/generations",
            b"{\"model\":\"gpt-image-2\",\"prompt\":\"\xff\"}".to_vec(),
        ),
        (
            "/v1/images/generations",
            br#"{"model":"gpt-image-2","prompt":"a","prompt":"b"}"#.to_vec(),
        ),
        (
            "/v1/images/edits",
            br#"{"model":"gpt-image-2","prompt":"p","images":[{"image_url":"u","image_url":"v"}]}"#.to_vec(),
        ),
    ] {
        let (status, text) = post(format!("{proxy}{path}"), "application/json", body.clone()).await;
        assert_eq!(status, 400, "{path} {}: {text}", String::from_utf8_lossy(&body));
    }
    assert!(seen.lock().unwrap().is_empty(), "nothing went upstream");

    let body = br#"{"model":"gpt-image-2","prompt":"p","stream":false,"future":{"n":1e400}}"#.to_vec();
    let (status, text) = post(format!("{proxy}/v1/images/generations"), "application/json", body).await;
    assert_eq!(status, 200, "{text}");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].2,
        r#"{"model":"gpt-image-2","prompt":"p","future":{"n":1e400}}"#
    );
}

/// One request a recording upstream saw: its `Authorization` header and body.
type Calls = Arc<Mutex<Vec<(String, String)>>>;

/// A loopback upstream that records each request and answers one Images result.
async fn recording_upstream() -> (String, Calls) {
    let calls: Calls = Arc::default();
    let record = calls.clone();
    let url = serve(
        axum::Router::new().fallback(move |headers: HeaderMap, body: axum::body::Bytes| {
            let record = record.clone();
            async move {
                let authorization = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                record
                    .lock()
                    .unwrap()
                    .push((authorization, String::from_utf8_lossy(&body).into_owned()));
                (
                    [("content-type", "application/json")],
                    r#"{"created":1,"data":[{"b64_json":"aGk="}]}"#,
                )
            }
        }),
    )
    .await;
    (url, calls)
}

/// A multipart edit with one image, the given text fields first.
fn form_with(fields: &[(&str, &[u8])]) -> (String, Vec<u8>) {
    let boundary = "cpa-image-boundary";
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(value);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"in.png\"\r\n\
             Content-Type: image/png\r\n\r\nPNGDATA\r\n--{boundary}--\r\n"
        )
        .as_bytes(),
    );
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// The Images handler rewrites `model` and `stream` before the Codex adapter sees the
/// request, so it reads the client's own JSON and form under the adapter's rules first
/// (#22, docs/codex/CONTRACT.md "Request boundary"). A repeated `stream`, in either order,
/// a repeated `model`, a `stream` form value that is not UTF-8 or not a boolean spelling,
/// and a repeated form field the handler reads once, are refused before anything is sent.
/// Before, deleting a `"stream":false` left the other `stream` for the adapter, and the
/// form's first `stream` was read lossily, with any other text read as false.
#[tokio::test]
async fn codex_images_read_the_original_request_before_rewriting_it() {
    let auth_dir = AuthDir::new();
    let (proxy, seen) = proxy(&auth_dir).await;
    let duplicates: [&[u8]; 4] = [
        br#"{"model":"gpt-image-2","prompt":"p","stream":false,"stream":true}"#,
        br#"{"model":"gpt-image-2","prompt":"p","stream":true,"stream":false}"#,
        br#"{"model":"gpt-image-2","prompt":"p","stream":false,"stream":false}"#,
        br#"{"model":"gpt-image-2","model":"gpt-image-1.5","prompt":"p"}"#,
    ];
    for path in ["/v1/images/generations", "/v1/images/edits"] {
        for body in duplicates {
            let (status, text) = post(format!("{proxy}{path}"), "application/json", body.to_vec()).await;
            let shown = String::from_utf8_lossy(body);
            assert_eq!(status, 400, "{path} {shown}: {text}");
            assert!(
                text.contains("request body repeats an object key at byte "),
                "{path} {shown}: {text}"
            );
        }
        let (status, text) = post(
            format!("{proxy}{path}"),
            "application/json",
            br#"{"model":"gpt-image-2","prompt":"p","stream":"true"}"#.to_vec(),
        )
        .await;
        assert_eq!(status, 400, "{path}: {text}");
        assert!(text.contains("request field `stream` must be a boolean"), "{text}");
    }
    for (fields, message) in [
        (
            vec![("model", &b"gpt-image-2"[..]), ("prompt", b"p"), ("stream", b"\xff")],
            "request form field `stream` is not valid UTF-8 at byte 0",
        ),
        (
            vec![("model", &b"gpt-image-2"[..]), ("prompt", b"p"), ("stream", b"nope")],
            "request form field `stream` must be a boolean",
        ),
        (
            vec![
                ("model", &b"gpt-image-2"[..]),
                ("prompt", b"p"),
                ("stream", b"false"),
                ("stream", b"true"),
            ],
            "request form field `stream` is repeated",
        ),
        (
            vec![
                ("model", &b"gpt-image-2"[..]),
                ("model", b"gpt-image-1.5"),
                ("prompt", b"p"),
            ],
            "request form field `model` is repeated",
        ),
        (
            vec![("model", &b"\xff/gpt-image-2"[..]), ("prompt", b"p")],
            "request form field `model` is not valid UTF-8 at byte 0",
        ),
    ] {
        let (content_type, body) = form_with(&fields);
        let (status, text) = post(format!("{proxy}/v1/images/edits"), &content_type, body).await;
        assert_eq!(status, 400, "{fields:?}: {text}");
        assert!(text.contains(message), "{fields:?}: {text}");
    }
    assert!(seen.lock().unwrap().is_empty(), "nothing went upstream");

    // Valid spellings still reach the backend, with the routed `stream`.
    let (content_type, body) = form_with(&[("model", b"gpt-image-2"), ("prompt", b"p"), ("stream", b" False ")]);
    let (status, text) = post(format!("{proxy}/v1/images/edits"), &content_type, body).await;
    assert_eq!(status, 200, "{text}");
    let (status, text) = post(
        format!("{proxy}/v1/images/generations"),
        "application/json",
        br#"{"model":"gpt-image-2","prompt":"p","stream":null}"#.to_vec(),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].0, "/images/edits");
    assert_eq!(
        seen[0].2,
        r#"{"model":"gpt-image-2","prompt":"p","images":[{"image_url":"data:image/png;base64,UE5HREFUQQ=="}]}"#
    );
    assert_eq!(
        seen[1],
        (
            "/images/generations".to_owned(),
            "application/json".to_owned(),
            r#"{"model":"gpt-image-2","prompt":"p"}"#.to_owned()
        )
    );
}

/// xAI keeps Go's Images ingress, and the Codex rules decide nothing for it: a repeated
/// `stream` on an xAI model still reaches xAI. A repeated `model` cannot move a request
/// between providers: Go's first `model` picks xAI, and then only the xAI credential is
/// used; a first Codex `model` meets the Codex rules and nothing is sent anywhere.
#[tokio::test]
async fn xai_images_keep_go_ingress_and_duplicates_cannot_cross_providers() {
    let auth_dir = AuthDir::new();
    let (codex_url, codex_calls) = recording_upstream().await;
    let (xai_url, xai_calls) = recording_upstream().await;
    let cfg = auth_dir.config(&format!(
        "xai-api-key:\n  - api-key: xai-key-FAKE\n    base-url: {xai_url}\n"
    ));
    let mut credentials = cpa_core::config::credentials::from_config(&cfg);
    assert_eq!(credentials.len(), 1, "the xAI key");
    credentials.push(codex_oauth(&codex_url));
    let rt = Arc::new(cpa_server::testing::runtime(cfg, credentials, executors()));
    let proxy = serve(cpa_server::router(rt)).await;

    for body in [
        &br#"{"model":"grok-imagine-image","prompt":"p","stream":false,"stream":true}"#[..],
        br#"{"model":"grok-imagine-image","model":"gpt-image-2","prompt":"p"}"#,
    ] {
        let (status, text) = post(
            format!("{proxy}/v1/images/generations"),
            "application/json",
            body.to_vec(),
        )
        .await;
        assert_eq!(status, 200, "{}: {text}", String::from_utf8_lossy(body));
    }
    let (content_type, form) = form_with(&[
        ("model", b"grok-imagine-image"),
        ("model", b"gpt-image-2"),
        ("prompt", b"p"),
    ]);
    let (status, text) = post(format!("{proxy}/v1/images/edits"), &content_type, form).await;
    assert_eq!(status, 200, "{text}");
    {
        let xai = xai_calls.lock().unwrap();
        assert_eq!(xai.len(), 3);
        for (authorization, body) in xai.iter() {
            assert_eq!(authorization, "Bearer xai-key-FAKE");
            assert!(body.contains(r#""model":"grok-imagine-image""#), "{body}");
        }
    }
    assert!(
        codex_calls.lock().unwrap().is_empty(),
        "the Codex credential took no xAI request"
    );

    let (status, text) = post(
        format!("{proxy}/v1/images/generations"),
        "application/json",
        br#"{"model":"gpt-image-2","model":"grok-imagine-image","prompt":"p"}"#.to_vec(),
    )
    .await;
    assert_eq!(status, 400, "{text}");
    let (content_type, form) = form_with(&[
        ("model", b"gpt-image-2"),
        ("model", b"grok-imagine-image"),
        ("prompt", b"p"),
    ]);
    let (status, text) = post(format!("{proxy}/v1/images/edits"), &content_type, form).await;
    assert_eq!(status, 400, "{text}");
    assert_eq!(xai_calls.lock().unwrap().len(), 3, "no further xAI request");
    assert!(codex_calls.lock().unwrap().is_empty(), "no Codex request");
}
