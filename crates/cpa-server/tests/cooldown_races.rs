//! Go 35af74f9 (sdk/cliproxy/auth/conductor_issue6643_test.go) through the real router:
//! a success from a request dispatched before a cooldown was recorded leaves that cooldown,
//! and a request dispatched after it clears it. Loopback upstreams, fake keys and private
//! `auth-dir`s only.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use serde_json::json;
use tokio::sync::Semaphore;

const REPLY: &str = r#"{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"m","stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;
const STREAM: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"m\",\"stop_reason\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
const MODEL: &str = "claude-opus-4-6";
const WAIT: Duration = Duration::from_secs(10);

/// A scripted upstream: request `n` waits for `gates[n]` when it has one, then answers
/// with a ten-minute 429 (inside the default `max-trusted-cooldown`, so Go's plain
/// cooldown) if `n` is in `refused`, or succeeds.
struct Up {
    calls: AtomicUsize,
    gates: Vec<Semaphore>,
    refused: Vec<usize>,
}

async fn upstream(State(up): State<Arc<Up>>, req: axum::extract::Request) -> Response {
    let body = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap();
    let stream = serde_json::from_slice::<serde_json::Value>(&body).unwrap()["stream"] == true;
    let call = up.calls.fetch_add(1, Ordering::SeqCst);
    if let Some(gate) = up.gates.get(call) {
        gate.acquire().await.unwrap().forget();
    }
    if up.refused.contains(&call) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("content-type", "application/json"), ("retry-after", "600")],
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"rate limited"}}"#,
        )
            .into_response();
    }
    if stream {
        ([("content-type", "text/event-stream")], STREAM).into_response()
    } else {
        ([("content-type", "application/json")], REPLY).into_response()
    }
}

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

async fn message(url: &str, stream: bool) -> (u16, String) {
    let body = json!({"model": MODEL, "max_tokens": 8, "stream": stream,
        "messages": [{"role": "user", "content": "hi"}]});
    let res = wreq::Client::new()
        .post(format!("{url}/v1/messages"))
        .header("x-api-key", "client-key")
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    (res.status().as_u16(), res.text().await.unwrap())
}

/// Waits, boundedly, until `done` holds.
async fn until(what: &str, done: impl Fn() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// A private `auth-dir` holding one Claude API-key credential at `upstream_url`, and
/// the config YAML that points at it (plus `extra`).
fn claude_credential(name: &str, upstream_url: &str, extra: &str) -> (Config, Credential, std::path::PathBuf) {
    let dir =
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("cooldown-races-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let yaml = format!("api-keys: [client-key]\nauth-dir: {}\n{extra}", dir.display());
    let meta = json!({"type": "claude", "api_key": "sk-fake-claude", "base_url": upstream_url});
    std::fs::write(dir.join("claude.json"), meta.to_string()).unwrap();
    let mut credential =
        Credential::from_file(&dir, &dir.join("claude.json"), meta.as_object().unwrap().clone()).unwrap();
    credential.attributes.insert("api_key".into(), "sk-fake-claude".into());
    credential.attributes.insert("base_url".into(), upstream_url.into());
    (Config::parse(&yaml).unwrap(), credential, dir)
}

fn executors() -> Executors {
    Executors {
        claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: Default::default(),
        meta: Default::default(),
        xai: Default::default(),
    }
}

/// TestManager_Issue6643_InFlightSuccessesKeepFreshQuotaCooldown, plus the watermark rule:
/// a stream and a buffered request are in flight around a refused one, which was itself
/// dispatched before the second of them. Its 429, recorded last, stamps every dispatch so
/// far, so neither success clears the cooldown and the next request never reaches the
/// upstream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_flight_successes_keep_a_fresh_quota_cooldown() {
    let up = Arc::new(Up {
        calls: AtomicUsize::new(0),
        gates: (0..3).map(|_| Semaphore::new(0)).collect(),
        refused: vec![1],
    });
    let upstream_url = serve(axum::Router::new().fallback(upstream).with_state(up.clone())).await;
    let (cfg, credential, _dir) = claude_credential("in-flight", &upstream_url, "");
    let rt = Arc::new(cpa_server::testing::runtime(cfg, vec![credential], executors()));
    let url = serve(cpa_server::app(rt.clone(), axum::Router::new())).await;

    let send = |stream| {
        let url = url.clone();
        tokio::spawn(async move { message(&url, stream).await })
    };
    let streamed = send(true);
    until("the stream's dispatch", || up.calls.load(Ordering::SeqCst) == 1).await;
    let refused = send(false);
    until("the refused dispatch", || up.calls.load(Ordering::SeqCst) == 2).await;
    let buffered = send(false);
    until("the buffered dispatch", || up.calls.load(Ordering::SeqCst) == 3).await;

    // The request dispatched second is refused after the third was dispatched.
    up.gates[1].add_permits(1);
    let (status, body) = refused.await.unwrap();
    assert_eq!(status, 429, "{body}");

    up.gates[0].add_permits(1);
    up.gates[2].add_permits(1);
    let (status, body) = streamed.await.unwrap();
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("message_stop"), "{body}");
    let (status, body) = buffered.await.unwrap();
    assert_eq!(status, 200, "{body}");
    // The stream's success is recorded as its body ends.
    until("both successes recorded", || rt.store().stats().success == 2).await;

    let (status, body) = message(&url, false).await;
    assert_eq!(status, 429, "still cooling: {body}");
    assert_eq!(
        up.calls.load(Ordering::SeqCst),
        3,
        "the cooling credential was not tried"
    );
}
