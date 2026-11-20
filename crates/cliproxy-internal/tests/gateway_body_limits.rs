//! Request-body resource limits through the in-process middleware stack.
//! Expectations come from the configured cap, authentication order, and supported routes.
//! Fake credentials and private auth directories only.
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use axum::body::{Body, Bytes, HttpBody};
use cliproxy_internal::config::Config;
use cliproxy_internal::gateway::management::{self, Management};
use cliproxy_internal::providers::Executors;
use cliproxy_internal::providers::claude::ClaudeExecutor;
use http_body::{Frame, SizeHint};
use serde_json::Value;
use tower::ServiceExt as _;

/// The client sends this many bytes per frame.
const FRAME: usize = 16;

/// A request body that counts what the server pulled from it.
struct Counted {
    rest: Bytes,
    read: Arc<AtomicU64>,
    declared: bool,
}

impl HttpBody for Counted {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        if self.rest.is_empty() {
            return Poll::Ready(None);
        }
        let n = self.rest.len().min(FRAME);
        let chunk = self.rest.split_to(n);
        self.read.fetch_add(n as u64, Ordering::Relaxed);
        Poll::Ready(Some(Ok(Frame::data(chunk))))
    }

    fn is_end_stream(&self) -> bool {
        self.rest.is_empty()
    }

    fn size_hint(&self) -> SizeHint {
        if self.declared {
            SizeHint::with_exact(self.rest.len() as u64)
        } else {
            SizeHint::default()
        }
    }
}

fn app(dir: &std::path::Path, yaml: &str) -> (axum::Router, Arc<cliproxy_internal::runtime::Runtime>) {
    std::fs::create_dir_all(dir.join("auth")).unwrap();
    let path = dir.join("config.yaml");
    std::fs::write(&path, format!("{yaml}auth-dir: {}\n", dir.join("auth").display())).unwrap();
    let rt = Arc::new(cliproxy_internal::gateway::testing::runtime(
        Config::load(&path).unwrap(),
        vec![],
        Executors {
            claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            meta: Default::default(),
            xai: Default::default(),
        },
    ));
    let state = Management::with_options(rt.clone(), path, management::Options::default());
    // crates/cliproxy/src/main.rs, without Home, observability and the heap trimmer.
    let app = cliproxy_internal::gateway::app(rt.clone(), management::router(state.clone()));
    let app = cliproxy_internal::gateway::safe_mode::router(&rt, app);
    let app = app.layer(axum::middleware::from_fn(management::cors));
    let app = cliproxy_internal::gateway::request_logging::router(&state, app);
    (cliproxy_internal::gateway::body_limit::layer(&rt, app), rt)
}

/// Send a streaming body and record exactly which frames middleware consumes.
async fn counted_request(
    app: axum::Router,
    method: &str,
    path: &str,
    body: Vec<u8>,
    declared: bool,
    key: Option<&str>,
) -> (u16, Value, u64) {
    let read = Arc::new(AtomicU64::new(0));
    let mut request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if declared {
        request = request.header("content-length", body.len());
    }
    if let Some(key) = key {
        request = request.header("authorization", format!("Bearer {key}"));
    }
    let mut request = request
        .body(Body::new(Counted {
            rest: Bytes::from(body),
            read: read.clone(),
            declared,
        }))
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "192.0.2.1:1234".parse::<SocketAddr>().unwrap(),
    ));
    let response = app.oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value =
        serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, value, read.load(Ordering::Relaxed))
}

/// Body limits protect supported handlers independently of their JSON parser or error prose.
#[tokio::test]
async fn supported_endpoints_enforce_declared_and_streaming_body_limits() {
    let dir = std::env::temp_dir().join(format!("cpa-body-cap-{}", uuid::Uuid::new_v4()));
    let yaml = "config-version: 8\nserver:\n  max-request-body-bytes: 64\naccess:\n  api-keys: [test-key]\nmanagement:\n  allow-remote: true\n  secret-key: mgmt-secret\n";
    let (app, _) = app(&dir, yaml);
    let serving = [
        "/v1/chat/completions",
        "/v1/completions",
        "/v1/responses",
        "/v1/responses/compact",
        "/backend-api/codex/responses",
        "/v1/messages",
        "/v1/messages/count_tokens",
        "/v1/images/generations",
        "/v1/images/edits",
        "/v1/audio/speech",
        "/v1/tts",
        "/v1/videos",
        "/v1/videos/generations",
        "/openai/v1/videos",
        "/v1/realtime/client_secrets",
        "/v1/realtime/sessions",
        "/v1/realtime/calls",
        "/v1/live",
    ];
    let oversized = format!(r#"{{"model":"unserved","input":"{}"}}"#, "x".repeat(256)).into_bytes();
    for path in serving {
        for declared in [false, true] {
            let (status, detail, consumed) =
                counted_request(app.clone(), "POST", path, oversized.clone(), declared, Some("test-key")).await;
            assert_eq!(status, 413, "{path}, declared={declared}: {detail}");
            assert!(!detail.to_string().is_empty());
            if declared {
                assert_eq!(consumed, 0, "reject Content-Length before polling");
            } else {
                assert!(
                    consumed <= 64 + FRAME as u64,
                    "stop after the crossing frame: {path}: {consumed}"
                );
            }
        }
    }
    for (method, path) in [
        ("PUT", "/v8/management/config"),
        ("PATCH", "/v8/management/config"),
        ("PUT", "/v8/management/config.yaml"),
        ("PUT", "/v8/management/config/server/port"),
        ("POST", "/v0/management/api-call"),
        ("POST", "/v8/management/requests/api-call"),
        ("POST", "/v0/management/auth-files?name=x.json"),
        ("POST", "/v8/management/credentials?name=x.json"),
        ("DELETE", "/v0/management/auth-files"),
        ("PATCH", "/v0/management/auth-files/status"),
        ("PATCH", "/v8/management/credentials/fields"),
        ("POST", "/v0/management/auth-files/refresh"),
        ("POST", "/v0/management/oauth-callback"),
        ("POST", "/v8/management/oauth/callback"),
        ("POST", "/v0/management/reset-quota"),
        ("POST", "/v8/management/routing/cooldown/reset"),
    ] {
        for declared in [false, true] {
            let (status, detail, consumed) = counted_request(
                app.clone(),
                method,
                path,
                oversized.clone(),
                declared,
                Some("mgmt-secret"),
            )
            .await;
            assert_eq!(status, 413, "{method} {path}: {detail}");
            assert!(consumed <= if declared { 0 } else { 64 + FRAME as u64 });
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn authentication_and_removed_routes_do_not_consume_streaming_bodies() {
    for logging in [false, true] {
        let dir = std::env::temp_dir().join(format!("cpa-body-auth-{}", uuid::Uuid::new_v4()));
        let yaml = format!(
            "config-version: 8\nserver:\n  max-request-body-bytes: 64\naccess:\n  api-keys: [test-key]\nmanagement:\n  allow-remote: true\n  secret-key: mgmt-secret\nobservability:\n  logs:\n    request-log: {logging}\n"
        );
        let (app, _) = app(&dir, &yaml);
        for (path, key, status) in [
            ("/v1/responses", None, 401),
            ("/v8/management/config", None, 404),
            ("/v8/management/plugins", Some("mgmt-secret"), 410),
            ("/v1beta/models/gemini:generateContent", Some("test-key"), 410),
        ] {
            let (actual, detail, consumed) =
                counted_request(app.clone(), "POST", path, vec![b'x'; 256], false, key).await;
            assert_eq!(actual, status, "{path}: {detail}");
            assert_eq!(consumed, 0, "{path} with request logging {logging}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[tokio::test]
async fn body_limit_is_inclusive_and_nonpositive_limits_disable_it() {
    let raw = br#"{"model":"unserved","input":"ok"}"#.to_vec();
    for limit in [-1, 0, raw.len() as i64 - 1, raw.len() as i64, raw.len() as i64 + 1] {
        for declared in [false, true] {
            let dir = std::env::temp_dir().join(format!("cpa-body-boundary-{}", uuid::Uuid::new_v4()));
            let yaml = format!(
                "config-version: 8\nserver:\n  max-request-body-bytes: {limit}\naccess:\n  api-keys: [test-key]\n"
            );
            let (app, _) = app(&dir, &yaml);
            let (status, detail, consumed) =
                counted_request(app, "POST", "/v1/responses", raw.clone(), declared, Some("test-key")).await;
            let capped = limit > 0 && limit < raw.len() as i64;
            assert_eq!(status, if capped { 413 } else { 400 }, "limit={limit}: {detail}");
            if !capped {
                assert_eq!(consumed, raw.len() as u64);
            }
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}

/// Each request reads the limit from the current config on every request.
#[tokio::test]
async fn a_published_limit_applies_to_the_next_request() {
    let dir = std::env::temp_dir().join(format!("cpa-body-limit-reload-{}", std::process::id()));
    let yaml = "config-version: 8\naccess:\n  api-keys:\n    - test-key\n";
    let (app, rt) = app(&dir, yaml);
    let send = |app: axum::Router| async move {
        let request = axum::http::Request::post("/v1/responses")
            .header("authorization", "Bearer test-key")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"model":"gpt-5","input":"ok"}"#))
            .unwrap();
        app.oneshot(request).await.unwrap().status().as_u16()
    };
    assert_eq!(send(app.clone()).await, 400, "no limit: the model lookup answers");
    let path = dir.join("config.yaml");
    // Keeps the private auth-dir app() wrote.
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("{text}server:\n  max-request-body-bytes: 29\n")).unwrap();
    rt.publish_config(Config::load(&path).unwrap());
    assert_eq!(send(app).await, 413);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Middleware stores its limit on the request: a reload while the body is still
/// arriving does not change the limit its decoding applies.
#[tokio::test]
async fn a_reload_during_a_request_keeps_its_limit() {
    use futures_util::StreamExt as _;
    let dir = std::env::temp_dir().join(format!("cpa-body-limit-mid-{}", std::process::id()));
    let yaml = "config-version: 8\nserver:\n  max-request-body-bytes: 128\naccess:\n  api-keys:\n    - test-key\n";
    let (app, rt) = app(&dir, yaml);
    let decoded = format!(r#"{{"model":"gpt-5","input":"{}"}}"#, "z".repeat(512));
    let compressed = zstd::encode_all(decoded.as_bytes(), 3).unwrap();
    assert!(compressed.len() < 128);
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(1);
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|chunk| (Ok::<_, std::io::Error>(chunk), rx))
    });
    let request = axum::http::Request::post("/v1/responses")
        .header("authorization", "Bearer test-key")
        .header("content-type", "application/json")
        .header("content-encoding", "zstd")
        .body(Body::from_stream(stream.boxed()))
        .unwrap();
    let response = tokio::spawn(app.oneshot(request));
    let (head, tail) = compressed.split_at(compressed.len() / 2);
    tx.send(Bytes::copy_from_slice(head)).await.unwrap();
    // The handler has taken the first half, so the middleware has run.
    while tx.capacity() < tx.max_capacity() {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let path = dir.join("config.yaml");
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replace("max-request-body-bytes: 128", "max-request-body-bytes: 0");
    std::fs::write(&path, text).unwrap();
    rt.publish_config(Config::load(&path).unwrap());
    assert_eq!(rt.config().max_request_body_bytes(), None);
    tx.send(Bytes::copy_from_slice(tail)).await.unwrap();
    drop(tx);
    let response = response.await.unwrap().unwrap();
    let status = response.status().as_u16();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(status, 413);
    let error: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(error["error"]["type"], "invalid_request_error");
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|text| text.contains("too large"))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A v8 config write checks its `If-Match` before it reads the body: a stale write
/// fails at once, even while the body is still arriving.
#[tokio::test]
async fn a_stale_config_write_fails_before_its_body_is_read() {
    struct Open(Arc<AtomicU64>);
    impl HttpBody for Open {
        type Data = Bytes;
        type Error = std::io::Error;
        fn poll_frame(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
            // One byte, then the stream stays open.
            if self.0.fetch_add(1, Ordering::Relaxed) == 0 {
                Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(b"{")))))
            } else {
                Poll::Pending
            }
        }
    }
    // Config-write order: lock, read the file, If-Match, parse the stored config, then
    // read the body. Each case answers before the pending body is polled.
    let non_utf8: &[u8] = b"config-version: 8\nserver: \xff\n";
    // Name, the stored file written after the runtime loads, the condition header, status.
    type Case = (
        &'static str,
        Option<&'static [u8]>,
        Option<(&'static str, &'static str)>,
        u16,
    );
    let cases: [Case; 4] = [
        ("stale", None, Some(("if-match", "\"definitely-stale\"")), 412),
        ("malformed", Some(b"server: ["), None, 500),
        (
            "malformed, If-None-Match",
            Some(b"server: ["),
            Some(("if-none-match", "*")),
            500,
        ),
        (
            "non-UTF-8, stale",
            Some(non_utf8),
            Some(("if-match", "\"definitely-stale\"")),
            412,
        ),
    ];
    for limit in ["32", "0"] {
        for (name, stored, condition, status) in cases {
            let dir = std::env::temp_dir().join(format!("cpa-body-limit-stale-{limit}-{}", uuid::Uuid::new_v4()));
            let yaml = format!(
                "config-version: 8\nserver:\n  max-request-body-bytes: {limit}\nmanagement:\n  allow-remote: true\n  secret-key: mgmt-secret\n"
            );
            let (app, _) = app(&dir, &yaml);
            // Corrupted after the runtime loaded it.
            if let Some(bytes) = stored {
                std::fs::write(dir.join("config.yaml"), bytes).unwrap();
            }
            let file = std::fs::read(dir.join("config.yaml")).unwrap();
            let polls = Arc::new(AtomicU64::new(0));
            let mut request = axum::http::Request::put("/v8/management/config")
                .header("authorization", "Bearer mgmt-secret")
                .header("content-type", "application/json");
            if let Some((key, value)) = condition {
                request = request.header(key, value);
            }
            let mut request = request.body(Body::new(Open(polls.clone()))).unwrap();
            let peer: SocketAddr = "192.0.2.1:1234".parse().unwrap();
            request.extensions_mut().insert(axum::extract::ConnectInfo(peer));
            let response = tokio::time::timeout(std::time::Duration::from_secs(5), app.oneshot(request))
                .await
                .unwrap_or_else(|_| panic!("limit {limit}, {name}: waited for the body"))
                .unwrap();
            assert_eq!(response.status().as_u16(), status, "limit {limit}, {name}");
            assert_eq!(
                polls.load(Ordering::Relaxed),
                0,
                "limit {limit}, {name}: the body was polled"
            );
            if status == 412 {
                // The ETag hashes the raw file bytes, UTF-8 or not.
                use sha2::Digest as _;
                let hex: String = sha2::Sha256::digest(&file).iter().map(|b| format!("{b:02x}")).collect();
                assert_eq!(response.headers()["etag"], format!("\"{hex}\"").as_str(), "{name}");
            } else {
                let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
                let body: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(body["error"], "invalid_config", "limit {limit}, {name}");
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
