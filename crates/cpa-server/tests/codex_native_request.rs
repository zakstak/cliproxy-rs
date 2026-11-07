//! Native Codex Responses requests through the real router (issue #2): an official Codex
//! client keeps `parallel_tool_calls` (false, true or omitted) and its output-replay
//! `include`, while other Responses clients keep Go's forced values. Standard, Responses
//! Lite and Sign in with ChatGPT credentials, against a loopback upstream with fake
//! tokens and a private `auth-dir`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::http::HeaderMap;
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_exec::Executors;
use serde_json::Value;

type Seen = Arc<Mutex<Vec<(HeaderMap, Bytes)>>>;

const SSE: &str = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n";
const CODEX_UA: &str = "codex_cli_rs/0.160.0 (Linux; x86_64) xterm-256color";
const REPLAY: &str = r#"["reasoning.encrypted_content","output.encrypted_content"]"#;

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
}

impl Drop for AuthDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn credential(siwc: bool, base_url: &str) -> Credential {
    let mut metadata = serde_json::json!({
        "type": "codex",
        "access_token": "at-FAKE",
        "account_id": "acct-FAKE",
        "expired": "2099-01-01T00:00:00Z",
    });
    if siwc {
        metadata["use_siwc"] = true.into();
        metadata["siwc"] = serde_json::json!({"access_token": "siwc-at-FAKE", "client_id": "client-FAKE"});
    }
    let mut credential = Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/codex.json"),
        metadata.as_object().unwrap().clone(),
    )
    .unwrap();
    for (key, value) in [("base_url", base_url), ("plan_type", "plus")] {
        credential.attributes.insert(key.into(), value.into());
    }
    credential
}

struct Proxy {
    url: String,
    seen: Seen,
    _auth_dir: AuthDir,
}

async fn proxy(siwc: bool) -> Proxy {
    let seen: Seen = Arc::default();
    let record = seen.clone();
    let upstream = serve(axum::Router::new().fallback(move |headers: HeaderMap, body: Bytes| {
        let record = record.clone();
        async move {
            record.lock().unwrap().push((headers, body));
            ([("content-type", "text/event-stream")], SSE)
        }
    }))
    .await;
    let auth_dir = AuthDir::new();
    let cfg = Config::parse(&format!(
        "auth-dir: {}\naccess:\n  api-keys: [client-key]\n",
        auth_dir.0.display()
    ))
    .unwrap();
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(
        cfg,
        vec![credential(siwc, &upstream)],
        executors,
    ));
    Proxy {
        url: serve(cpa_server::router(rt)).await,
        seen,
        _auth_dir: auth_dir,
    }
}

/// A request in current Codex's field order (core/src/client.rs `build_request`). The
/// spaced `"text" :` key marks bytes that only an untouched pass-through keeps.
fn body(parallel: Option<bool>, include: &str) -> String {
    let parallel = parallel
        .map(|p| format!(r#","parallel_tool_calls":{p}"#))
        .unwrap_or_default();
    format!(
        r#"{{"model":"gpt-5.5","instructions":"be brief","input":[{{"type":"message","role":"user","content":[{{"type":"input_text","text":"hi"}}]}}],"tools":[{{"type":"function","name":"f","description":"d","strict":false,"parameters":{{"type":"object","properties":{{}}}}}}],"tool_choice":"auto"{parallel},"reasoning":{{"effort":"low"}},"store":false,"stream":true,"include":{include},"prompt_cache_key":"pck-1","prompt_cache_options":{{"ttl":"30m"}},  "text" : {{"verbosity":"low"}}}}"#
    )
}

/// Sends `body` with `headers` and returns what upstream received.
async fn send(proxy: &Proxy, headers: &[(&str, &str)], body: String) -> (HeaderMap, String) {
    let mut request = wreq::Client::new()
        .post(format!("{}/v1/responses", proxy.url))
        .header("authorization", "Bearer client-key")
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = request.body(body).send().await.unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    assert_eq!(status, 200, "{text}");
    let (headers, body) = proxy.seen.lock().unwrap().pop().expect("upstream request");
    (headers, String::from_utf8(body.to_vec()).unwrap())
}

fn field(body: &str, path: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).unwrap();
    value.pointer(path).map(Value::to_string)
}

#[tokio::test]
async fn native_codex_keeps_tool_parallelism_and_output_replay() {
    let proxy = proxy(false).await;
    let codex = [("user-agent", CODEX_UA), ("originator", "codex_cli_rs")];
    for parallel in [Some(false), Some(true), None] {
        let (headers, sent) = send(&proxy, &codex, body(parallel, REPLAY)).await;
        assert_eq!(
            field(&sent, "/parallel_tool_calls"),
            parallel.map(|p| p.to_string()),
            "{parallel:?}: {sent}"
        );
        assert!(sent.contains(&format!(r#""include":{REPLAY}"#)), "{sent}");
        assert!(
            sent.contains(r#",  "text" : {"verbosity":"low"}"#),
            "untouched bytes: {sent}"
        );
        // Still removed: no evidence shows that the ChatGPT Codex backend accepts it.
        assert_eq!(field(&sent, "/prompt_cache_options"), None);
        assert_eq!(headers["authorization"], "Bearer at-FAKE", "client key never forwarded");
    }
    // Codex identified by `originator` alone.
    let (_, sent) = send(&proxy, &[("originator", "codex_cli_rs")], body(Some(false), REPLAY)).await;
    assert_eq!(field(&sent, "/parallel_tool_calls").as_deref(), Some("false"));
}

#[tokio::test]
async fn translated_responses_clients_keep_gos_forced_fields() {
    let proxy = proxy(false).await;
    for parallel in [Some(false), None] {
        let (_, sent) = send(&proxy, &[("user-agent", "OpenAI/Python 2.1.0")], body(parallel, REPLAY)).await;
        assert_eq!(field(&sent, "/parallel_tool_calls").as_deref(), Some("true"), "{sent}");
        assert_eq!(
            field(&sent, "/include").as_deref(),
            Some(r#"["reasoning.encrypted_content"]"#)
        );
    }
}

#[tokio::test]
async fn native_responses_lite_still_serializes_tool_calls() {
    let proxy = proxy(false).await;
    let (_, sent) = send(
        &proxy,
        &[
            ("user-agent", CODEX_UA),
            ("x-openai-internal-codex-responses-lite", "true"),
        ],
        body(Some(true), REPLAY),
    )
    .await;
    assert_eq!(field(&sent, "/parallel_tool_calls").as_deref(), Some("false"), "{sent}");
    assert!(sent.contains(&format!(r#""include":{REPLAY}"#)), "{sent}");
}

#[tokio::test]
async fn native_siwc_keeps_parallelism_and_drops_unproven_fields() {
    let proxy = proxy(true).await;
    let (headers, sent) = send(&proxy, &[("user-agent", CODEX_UA)], body(Some(false), REPLAY)).await;
    assert_eq!(headers["authorization"], "Bearer siwc-at-FAKE");
    assert_eq!(field(&sent, "/parallel_tool_calls").as_deref(), Some("false"), "{sent}");
    assert_eq!(
        field(&sent, "/include").as_deref(),
        Some(r#"["reasoning.encrypted_content"]"#),
        "no evidence that the SIWC upstream accepts output replay"
    );
    assert_eq!(
        field(&sent, "/prompt_cache_options"),
        None,
        "Go's SIWC unsupported field"
    );
}
