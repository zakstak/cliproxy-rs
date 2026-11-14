//! `routing.session-affinity-rebind-on: never` (#97): client -> cliproxy-rs -> loopback
//! mock Anthropic upstream, two fake Claude API keys, private `auth-dir`s, under a
//! network-denied runtime.
//!
//! A bound session never moves to the other key: an upstream failure reaches the
//! client as the bound key's own error, retry rounds retry only the bound key, and while
//! the bound key is cooling or removed the client gets `session_auth_unavailable`
//! naming no account. The default policy keeps moving the session.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use serde_json::json;

const OK: &str = r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-4-6","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;
const LIMIT: &str = r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#;
const BROKEN: &str = r#"{"type":"error","error":{"type":"api_error","message":"upstream broke"}}"#;
const SESSION: &str = "6f0e8a51-3c2d-4b7a-9e10-2a4b6c8d0e1f";

/// The upstream key of every call, in order.
type Calls = Arc<Mutex<Vec<String>>>;

/// A mock where `sk-fake-a` answers with the status in `a` (200, 429 with a one-hour
/// reset, or 500) and `sk-fake-b` always succeeds.
async fn mock(a: Arc<AtomicU16>) -> (String, Calls) {
    let calls: Calls = Arc::default();
    let seen = calls.clone();
    let app = axum::Router::new().fallback(move |req: Request| {
        let (seen, a) = (seen.clone(), a.clone());
        async move {
            let key = req
                .headers()
                .get("x-api-key")
                .or_else(|| req.headers().get("authorization"))
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .trim_start_matches("Bearer ")
                .to_owned();
            seen.lock().unwrap().push(key.clone());
            let status = if key == "sk-fake-a" {
                a.load(Ordering::SeqCst)
            } else {
                200
            };
            let body = match status {
                200 => OK,
                429 => LIMIT,
                _ => BROKEN,
            };
            (
                StatusCode::from_u16(status).unwrap(),
                [("content-type", "application/json"), ("retry-after", "3600")],
                body,
            )
                .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, calls)
}

fn auth_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("never-rebind-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Two keys under fill-first with affinity on; `sk-fake-a` takes precedence.
fn config(dir: &Path, upstream: &str, routing: &str) -> Config {
    Config::parse(&format!(
        "api-keys: [client-key]\nauth-dir: {}\nrouting:\n  strategy: fill-first\n  session-affinity: true\n{routing}\
         claude-api-key:\n  - api-key: sk-fake-a\n    base-url: {upstream}\n    priority: 1\n  \
         - api-key: sk-fake-b\n    base-url: {upstream}\n",
        dir.display()
    ))
    .unwrap()
}

async fn start(config: Config) -> (Arc<cpa_server::Runtime>, String) {
    let credentials = cpa_core::config::credentials::load(&config);
    let executors = Executors {
        claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: Default::default(),
        meta: Default::default(),
        xai: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(config, credentials, executors));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = cpa_server::app(rt.clone(), axum::Router::new());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (rt, url)
}

struct Reply {
    status: u16,
    retry_after: Option<String>,
    body: String,
}

async fn post(url: &str, session: Option<&str>) -> Reply {
    let body = json!({"model": "claude-opus-4-6", "max_tokens": 8, "messages": [{"role": "user", "content": "hi"}]});
    let mut request = wreq::Client::new()
        .post(format!("{url}/v1/messages"))
        .header("x-api-key", "client-key")
        .header("content-type", "application/json");
    if let Some(session) = session {
        request = request.header("x-claude-code-session-id", session);
    }
    let response = request.body(body.to_string()).send().await.unwrap();
    Reply {
        status: response.status().as_u16(),
        retry_after: response
            .headers()
            .get("retry-after")
            .map(|v| v.to_str().unwrap().to_owned()),
        body: response.text().await.unwrap(),
    }
}

/// The refusal names neither key nor any credential ID or label.
fn assert_anonymous(reply: &Reply, rt: &cpa_server::Runtime) {
    assert!(reply.body.contains("session_auth_unavailable"), "{}", reply.body);
    assert!(!reply.body.contains("sk-fake"), "{}", reply.body);
    for c in rt.store().snapshot() {
        assert!(!reply.body.contains(&c.id), "{}", reply.body);
        if !c.label.is_empty() {
            assert!(!reply.body.contains(&c.label), "{}", reply.body);
        }
    }
}

/// An exhausted bound key: the client gets that key's own 429, then the refusal with
/// the key's cooldown as `Retry-After`, while another session uses the other key. The
/// default policy moves the session to the other key at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_exhausted_bound_account_fails_the_session_instead_of_moving_it() {
    let dir = auth_dir("exhausted");
    let a = Arc::new(AtomicU16::new(200));
    let (upstream, calls) = mock(a.clone()).await;
    let (rt, url) = start(config(&dir, &upstream, "  session-affinity-rebind-on: never\n")).await;

    assert_eq!(post(&url, Some(SESSION)).await.status, 200);
    a.store(429, Ordering::SeqCst);
    let limited = post(&url, Some(SESSION)).await;
    assert_eq!(limited.status, 429, "the bound key's own error: {}", limited.body);
    assert!(limited.body.contains("slow down"), "{}", limited.body);
    let refused = post(&url, Some(SESSION)).await;
    assert_eq!(refused.status, 503, "{}", refused.body);
    assert_anonymous(&refused, &rt);
    let wait: u64 = refused.retry_after.as_deref().expect("Retry-After").parse().unwrap();
    assert!((3000..=3600).contains(&wait), "the bound key's cooldown: {wait}");
    assert_eq!(
        *calls.lock().unwrap(),
        ["sk-fake-a", "sk-fake-a"],
        "never the other key"
    );

    // Another session, and a request without one, use the other key.
    assert_eq!(post(&url, Some("another-session")).await.status, 200);
    assert_eq!(post(&url, None).await.status, 200);
    assert_eq!(calls.lock().unwrap()[2..], ["sk-fake-b", "sk-fake-b"]);
    let _ = std::fs::remove_dir_all(&dir);

    let dir = auth_dir("exhausted-default");
    let a = Arc::new(AtomicU16::new(200));
    let (upstream, calls) = mock(a.clone()).await;
    let (_rt, url) = start(config(&dir, &upstream, "")).await;
    assert_eq!(post(&url, Some(SESSION)).await.status, 200);
    a.store(429, Ordering::SeqCst);
    assert_eq!(post(&url, Some(SESSION)).await.status, 200, "default moves the session");
    assert_eq!(*calls.lock().unwrap(), ["sk-fake-a", "sk-fake-a", "sk-fake-b"]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// With `request-retry`, a transient failure of the bound key is retried on that key
/// alone; without a retry round left, the client gets the key's own error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_rounds_retry_only_the_bound_account() {
    let dir = auth_dir("retry");
    let a = Arc::new(AtomicU16::new(200));
    let (upstream, calls) = mock(a.clone()).await;
    let routing = "  session-affinity-rebind-on: never\n  retry: {request-retry: 1}\n  \
                   cooldown: {transient-error-cooldown-seconds: -1}\n";
    let (_rt, url) = start(config(&dir, &upstream, routing)).await;
    assert_eq!(post(&url, Some(SESSION)).await.status, 200);
    a.store(500, Ordering::SeqCst);
    let failed = post(&url, Some(SESSION)).await;
    assert_eq!(failed.status, 500, "{}", failed.body);
    assert!(failed.body.contains("upstream broke"), "{}", failed.body);
    assert_eq!(
        *calls.lock().unwrap(),
        ["sk-fake-a", "sk-fake-a", "sk-fake-a"],
        "two rounds, both on the bound key"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A removed bound key refuses the session (without `Retry-After`) until the binding
/// expires one TTL after the last request the key served; then the session starts
/// again on the remaining key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_removed_bound_account_fails_the_session_until_the_pin_expires() {
    let dir = auth_dir("removed");
    let a = Arc::new(AtomicU16::new(200));
    let (upstream, calls) = mock(a.clone()).await;
    let routing = "  session-affinity-rebind-on: never\n  session-affinity-ttl: 1s\n";
    let (rt, url) = start(config(&dir, &upstream, routing)).await;
    assert_eq!(post(&url, Some(SESSION)).await.status, 200);
    let remaining: Vec<_> = rt
        .store()
        .snapshot()
        .iter()
        .filter(|c| c.attributes.get("api_key").map(String::as_str) == Some("sk-fake-b"))
        .map(|c| cpa_core::credential::Credential::clone(c))
        .collect();
    assert_eq!(remaining.len(), 1);
    rt.store().reconcile(remaining);

    let refused = post(&url, Some(SESSION)).await;
    assert_eq!(refused.status, 503, "{}", refused.body);
    assert_anonymous(&refused, &rt);
    assert_eq!(refused.retry_after, None);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(post(&url, Some(SESSION)).await.status, 200, "the pin expired");
    assert_eq!(*calls.lock().unwrap(), ["sk-fake-a", "sk-fake-b"]);
    let _ = std::fs::remove_dir_all(&dir);
}
