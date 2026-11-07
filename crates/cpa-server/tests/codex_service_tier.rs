//! `service_tier` on Codex requests through the real router (issue #8, #23). Every
//! Responses client keeps its explicit tier byte for byte, `flex` and `fast` included, and
//! the generated `x-codex-routing-hint` names the tier the body carries. Translated
//! clients (Chat Completions) keep Go's rule (only `priority`, alias `fast`, and
//! `ultrafast`). An upstream that refuses a tier
//! reaches the client with its own status and message. Loopback upstream, fake tokens and
//! a private `auth-dir`; nothing here shows that a real subscription backend accepts
//! `flex`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_exec::Executors;
use serde_json::Value;

type Seen = Arc<Mutex<Vec<(HeaderMap, Bytes)>>>;

const SSE: &str = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n";
const CODEX_UA: &str = "codex_cli_rs/0.160.0 (Linux; x86_64) xterm-256color";
const HINT: &str = "x-codex-routing-hint";
/// The mock upstream refuses this tier, as a backend without it might.
const REFUSED_TIER: &str = "refused-tier";
const REFUSAL: &str = r#"{"error":{"message":"Unsupported service_tier: refused-tier","type":"invalid_request_error","param":"service_tier","code":"unsupported_value"}}"#;

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

/// The router with one Codex credential whose upstream records each request and refuses
/// [`REFUSED_TIER`] with [`REFUSAL`].
async fn proxy(siwc: bool) -> Proxy {
    let seen: Seen = Arc::default();
    let record = seen.clone();
    let upstream = serve(axum::Router::new().fallback(move |headers: HeaderMap, body: Bytes| {
        let record = record.clone();
        async move {
            let refused = serde_json::from_slice::<Value>(&body)
                .ok()
                .is_some_and(|v| v["service_tier"] == REFUSED_TIER);
            record.lock().unwrap().push((headers, body));
            if refused {
                (StatusCode::BAD_REQUEST, [("content-type", "application/json")], REFUSAL).into_response()
            } else {
                ([("content-type", "text/event-stream")], SSE).into_response()
            }
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

/// A request in current Codex's field order (core/src/client.rs `build_request`) with
/// `service_tier` as raw JSON, or omitted. The spaced `"text" :` key marks bytes that only
/// an untouched pass-through keeps.
fn responses_body(tier: Option<&str>) -> String {
    let tier = tier.map(|t| format!(r#","service_tier":{t}"#)).unwrap_or_default();
    format!(
        r#"{{"model":"gpt-5.5","instructions":"be brief","input":[{{"type":"message","role":"user","content":[{{"type":"input_text","text":"hi"}}]}}],"tools":[],"tool_choice":"auto","parallel_tool_calls":false,"reasoning":{{"effort":"low"}},"store":false,"stream":true,"include":["reasoning.encrypted_content"]{tier},"prompt_cache_key":"pck-1",  "text" : {{"verbosity":"low"}}}}"#
    )
}

/// Sends `body` to `path` with `headers`; returns the client's status and body, and the
/// request upstream received.
async fn send(proxy: &Proxy, path: &str, headers: &[(&str, &str)], body: String) -> (u16, String, HeaderMap, String) {
    let mut request = wreq::Client::new()
        .post(format!("{}{path}", proxy.url))
        .header("authorization", "Bearer client-key")
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = request.body(body).send().await.unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    let (headers, body) = proxy.seen.lock().unwrap().pop().expect("upstream request");
    (status, text, headers, String::from_utf8(body.to_vec()).unwrap())
}

fn tier(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).unwrap();
    value.get("service_tier").map(Value::to_string)
}

fn hint(headers: &HeaderMap) -> Option<&str> {
    headers.get(HINT).map(|v| v.to_str().unwrap())
}

#[tokio::test]
async fn native_codex_keeps_explicit_tiers_in_body_and_routing_hint() {
    let proxy = proxy(false).await;
    let codex = [("user-agent", CODEX_UA), ("originator", "codex_cli_rs")];
    // (client tier, upstream body tier, routing hint)
    for (sent, want, want_hint) in [
        (Some(r#""flex""#), Some(r#""flex""#), "model=gpt-5.5;tier=flex"),
        (
            Some(r#""priority""#),
            Some(r#""priority""#),
            "model=gpt-5.5;tier=priority",
        ),
        (
            Some(r#""ultrafast""#),
            Some(r#""ultrafast""#),
            "model=gpt-5.5;tier=ultrafast",
        ),
        // The native route (#23) forwards the client's spelling; the generated hint
        // names the tier the body carries.
        (Some(r#""fast""#), Some(r#""fast""#), "model=gpt-5.5;tier=fast"),
        (None, None, "model=gpt-5.5"),
        // A tier only a catalog would name: forwarded for the upstream to decide.
        (Some(r#""scale""#), Some(r#""scale""#), "model=gpt-5.5;tier=scale"),
        // Malformed: forwarded unchanged; the hint carries only a string tier, as in Go.
        (Some("42"), Some("42"), "model=gpt-5.5"),
    ] {
        let (status, text, headers, body) = send(&proxy, "/v1/responses", &codex, responses_body(sent)).await;
        assert_eq!(status, 200, "{sent:?}: {text}");
        assert_eq!(tier(&body).as_deref(), want, "{sent:?}: {body}");
        assert_eq!(hint(&headers), Some(want_hint), "{sent:?}");
        assert!(
            body.contains(r#",  "text" : {"verbosity":"low"}"#),
            "{sent:?}: other bytes kept: {body}"
        );
        assert_eq!(headers["authorization"], "Bearer at-FAKE", "client key never forwarded");
    }
    // Codex identified by `originator` alone.
    let (_, _, headers, body) = send(
        &proxy,
        "/v1/responses",
        &[("originator", "codex_cli_rs")],
        responses_body(Some(r#""flex""#)),
    )
    .await;
    assert_eq!(tier(&body).as_deref(), Some(r#""flex""#));
    assert_eq!(hint(&headers), Some("model=gpt-5.5;tier=flex"));
}

#[tokio::test]
async fn native_codex_receives_the_upstream_tier_refusal() {
    let proxy = proxy(false).await;
    let (status, text, headers, body) = send(
        &proxy,
        "/v1/responses",
        &[("user-agent", CODEX_UA)],
        responses_body(Some(&format!(r#""{REFUSED_TIER}""#))),
    )
    .await;
    assert_eq!(tier(&body), Some(format!(r#""{REFUSED_TIER}""#)));
    assert_eq!(
        hint(&headers),
        Some(format!("model=gpt-5.5;tier={REFUSED_TIER}").as_str())
    );
    assert_eq!(status, 400, "{text}");
    assert!(text.contains("Unsupported service_tier: refused-tier"), "{text}");
}

#[tokio::test]
async fn translated_clients_keep_gos_service_tier_rule() {
    let proxy = proxy(false).await;
    // Any Responses client takes the native route (#23) and keeps its tier as sent.
    let python = [("user-agent", "OpenAI/Python 2.1.0")];
    for (sent, want, want_hint) in [
        (r#""flex""#, Some(r#""flex""#), "model=gpt-5.5;tier=flex"),
        (r#""scale""#, Some(r#""scale""#), "model=gpt-5.5;tier=scale"),
        (r#""fast""#, Some(r#""fast""#), "model=gpt-5.5;tier=fast"),
        (r#""ultrafast""#, Some(r#""ultrafast""#), "model=gpt-5.5;tier=ultrafast"),
    ] {
        let (status, text, headers, body) = send(&proxy, "/v1/responses", &python, responses_body(Some(sent))).await;
        assert_eq!(status, 200, "{text}");
        assert_eq!(tier(&body).as_deref(), want, "{sent}: {body}");
        assert_eq!(hint(&headers), Some(want_hint), "{sent}");
    }
    // Chat Completions from a Codex user agent is a translated request too.
    for (sent, want, want_hint) in [
        ("flex", None, "model=gpt-5.5"),
        ("priority", Some(r#""priority""#), "model=gpt-5.5;tier=priority"),
    ] {
        let chat = format!(
            r#"{{"model":"gpt-5.5","stream":true,"service_tier":"{sent}","messages":[{{"role":"user","content":"hi"}}]}}"#
        );
        let (status, text, headers, body) =
            send(&proxy, "/v1/chat/completions", &[("user-agent", CODEX_UA)], chat).await;
        assert_eq!(status, 200, "{text}");
        assert_eq!(tier(&body).as_deref(), want, "chat {sent}: {body}");
        assert_eq!(hint(&headers), Some(want_hint), "chat {sent}");
    }
}

#[tokio::test]
async fn native_siwc_keeps_flex_without_a_routing_hint() {
    let proxy = proxy(true).await;
    let (status, text, headers, body) = send(
        &proxy,
        "/v1/responses",
        &[("user-agent", CODEX_UA)],
        responses_body(Some(r#""flex""#)),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(headers["authorization"], "Bearer siwc-at-FAKE");
    assert_eq!(tier(&body).as_deref(), Some(r#""flex""#), "{body}");
    assert_eq!(hint(&headers), None, "Go sends no routing hint to SIWC");
}
