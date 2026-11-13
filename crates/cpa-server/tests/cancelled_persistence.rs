//! Credential-file persistence after a request result (H1 #67, parent #66), end to end:
//! client -> cliproxy-rs -> loopback mock Anthropic upstream, one file-backed API-key
//! credential with a fake key in a private `auth-dir`.
//!
//! Go v8.0.35 `MarkResult` saves the credential with the request context, and
//! `beginForegroundStoreSave` refuses a cancelled context before `Store.Save`
//! (sdk/cliproxy/auth/conductor_persistence.go, conductor_lifecycle.go `persist`). A
//! client that goes away therefore leaves the auth file as it was, while a successful or
//! availability-neutral result still writes it with `"disabled": false` (sdk/auth/filestore.go).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use cpa_server::runtime::AttemptStats;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Notify;

const FILE: &str = "claude-persist.json";
const MODEL: &str = "claude-opus-4-6";

fn event(e: Value) -> String {
    format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap())
}

fn message_start() -> String {
    event(
        json!({"type":"message_start","message":{"id":"msg_p","type":"message","role":"assistant",
        "model":MODEL,"content":[],"stop_reason":null,"usage":{"input_tokens":1,"output_tokens":0}}}),
    )
}

#[derive(Default)]
struct Up {
    /// Request paths in arrival order.
    paths: Mutex<Vec<String>>,
    /// Fires once a hanging mode has answered as far as it ever will.
    parked: Notify,
}

async fn upstream(State(up): State<Arc<Up>>, req: axum::extract::Request) -> Response {
    up.paths.lock().unwrap().push(req.uri().path().to_owned());
    let path = req.uri().path().to_owned();
    let body = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8_lossy(&body);
    if path.ends_with("/count_tokens") {
        // A missing count endpoint: availability-neutral (`is_count_endpoint_missing`).
        return (
            StatusCode::NOT_FOUND,
            [("content-type", "text/plain")],
            "404 page not found",
        )
            .into_response();
    }
    if text.contains("MODE_HANG_HEADERS") {
        up.parked.notify_one();
        std::future::pending::<()>().await;
        unreachable!();
    }
    if text.contains("MODE_HANG_STREAM") {
        // The first event, a ping that lets any framer release it, then nothing more.
        let head = format!("{}{}", message_start(), event(json!({"type":"ping"})));
        let up = up.clone();
        let parts = futures_util::stream::iter([Ok::<_, std::convert::Infallible>(head)]).chain(
            futures_util::stream::once(async move {
                up.parked.notify_one();
                std::future::pending::<Result<String, std::convert::Infallible>>().await
            }),
        );
        return (
            [("content-type", "text/event-stream")],
            axum::body::Body::from_stream(parts),
        )
            .into_response();
    }
    let ok = r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-4-6","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;
    ([("content-type", "application/json")], ok).into_response()
}

struct Fixture {
    url: String,
    addr: String,
    rt: Arc<cpa_server::Runtime>,
    up: Arc<Up>,
    file: PathBuf,
    original: Vec<u8>,
}

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// A private auth-dir: without one, credential loading reads the real ~/.cli-proxy-api.
fn auth_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("cancel-persist-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn fixture(name: &str) -> Fixture {
    let up = Arc::new(Up::default());
    let upstream_url = format!(
        "http://{}",
        serve(axum::Router::new().fallback(upstream).with_state(up.clone())).await
    );
    let dir = auth_dir(name);
    // One attempt per request, so each result is counted exactly once.
    let yaml = format!(
        "api-keys: [client-key]\nauth-dir: {}\nrequest-retry: 0\n",
        dir.display()
    );
    let meta = json!({"type": "claude", "api_key": "sk-fake-claude", "base_url": upstream_url});
    let file = dir.join(FILE);
    let original = meta.to_string().into_bytes();
    std::fs::write(&file, &original).unwrap();
    let mut credential = Credential::from_file(&dir, &file, meta.as_object().unwrap().clone()).unwrap();
    credential.attributes.insert("api_key".into(), "sk-fake-claude".into());
    credential.attributes.insert("base_url".into(), upstream_url.clone());
    let rt = Arc::new(cpa_server::testing::runtime(
        Config::parse(&yaml).unwrap(),
        vec![credential],
        Executors {
            claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            meta: Default::default(),
            xai: Default::default(),
        },
    ));
    let addr = serve(cpa_server::app(rt.clone(), axum::Router::new())).await;
    Fixture {
        url: format!("http://{addr}"),
        addr,
        rt,
        up,
        file,
        original,
    }
}

impl Fixture {
    async fn post(&self, path: &str, body: Value) -> (u16, Vec<u8>) {
        let response = wreq::Client::new()
            .post(format!("{}{path}", self.url))
            .header("x-api-key", "client-key")
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        (status, response.bytes().await.unwrap().to_vec())
    }

    /// Opens a raw client connection and sends a `/v1/messages` request on it.
    async fn open(&self, marker: &str, stream: bool) -> TcpStream {
        let body = messages(marker, stream).to_string();
        let mut conn = TcpStream::connect(&self.addr).await.unwrap();
        let head = format!(
            "POST /v1/messages HTTP/1.1\r\nHost: {}\r\nx-api-key: client-key\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            self.addr,
            body.len()
        );
        conn.write_all(head.as_bytes()).await.unwrap();
        conn
    }

    fn stats(&self) -> AttemptStats {
        self.rt.store().stats()
    }

    /// Waits until the store has recorded `want`.
    async fn settled(&self, want: AttemptStats) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while self.stats() != want {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("attempts {:?}, want {want:?}", self.stats()));
    }

    fn on_disk(&self) -> Vec<u8> {
        std::fs::read(&self.file).unwrap()
    }

    /// The file and the in-memory credential stay exactly as loaded for a while after
    /// the cancelled result was counted. The pre-fix write follows the count
    /// synchronously in the same call, so it shows up well inside this window.
    async fn stays_unchanged(&self) {
        for _ in 0..60 {
            assert_eq!(
                String::from_utf8_lossy(&self.on_disk()),
                String::from_utf8_lossy(&self.original),
                "a cancelled request must not save the credential file"
            );
            let current = self.rt.store().get(FILE).unwrap();
            assert_eq!(current.metadata.get("disabled"), None, "no save reached memory either");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Waits until the file holds the loaded document plus `"disabled": false`.
    async fn persisted_disabled_false(&self) {
        let mut want: Value = serde_json::from_slice(&self.original).unwrap();
        want["disabled"] = Value::Bool(false);
        let read = || serde_json::from_slice::<Value>(&self.on_disk()).ok();
        tokio::time::timeout(Duration::from_secs(10), async {
            while read().as_ref() != Some(&want) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("file {:?}, want {want}", String::from_utf8_lossy(&self.on_disk())));
        // Exactly one JSON value with `disabled` a real boolean, not 0 or "false".
        assert_eq!(read().unwrap()["disabled"], Value::Bool(false));
    }
}

fn messages(marker: &str, stream: bool) -> Value {
    json!({"model": MODEL, "max_tokens": 16, "stream": stream,
        "messages": [{"role": "user", "content": marker}]})
}

fn stats(success: u64, failure: u64, cancelled: u64) -> AttemptStats {
    AttemptStats {
        success,
        failure,
        cancelled,
    }
}

/// The client reads the first stream event and leaves while the upstream stream is still
/// open: the attempt ends `Cancelled` and the auth file is not written. A later
/// successful request on the same credential still persists `disabled: false`, so the
/// skipped save leaves nothing behind that suppresses it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abandoned_stream_leaves_the_credential_file_unchanged() {
    let f = fixture("stream").await;
    let mut conn = f.open("MODE_HANG_STREAM", true).await;
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    while !String::from_utf8_lossy(&seen).contains("message_start") {
        let n = tokio::time::timeout(Duration::from_secs(10), conn.read(&mut buf))
            .await
            .expect("the first event reaches the client")
            .unwrap();
        assert!(n > 0, "stream ended early: {}", String::from_utf8_lossy(&seen));
        seen.extend_from_slice(&buf[..n]);
    }
    tokio::time::timeout(Duration::from_secs(10), f.up.parked.notified())
        .await
        .expect("the upstream stream is parked open");
    assert!(
        String::from_utf8_lossy(&seen).starts_with("HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&seen)
    );
    assert_eq!(f.stats(), stats(0, 0, 0), "the stream is still running");
    drop(conn);
    f.settled(stats(0, 0, 1)).await;
    f.stays_unchanged().await;

    // Positive control on the same credential: success persists after the skipped save.
    let (status, body) = f.post("/v1/messages", messages("after", false)).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    f.settled(stats(1, 0, 1)).await;
    f.persisted_disabled_false().await;
    assert_eq!(
        f.up.paths
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.starts_with("/v1/messages"))
            .count(),
        2,
        "both requests reached the mock upstream"
    );
}

/// A non-streaming client leaves while the executor waits for the upstream headers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abandoned_buffered_request_leaves_the_credential_file_unchanged() {
    let f = fixture("buffered").await;
    let conn = f.open("MODE_HANG_HEADERS", false).await;
    tokio::time::timeout(Duration::from_secs(10), f.up.parked.notified())
        .await
        .expect("the upstream received the request");
    assert_eq!(f.stats(), stats(0, 0, 0), "the request is still waiting");
    drop(conn);
    f.settled(stats(0, 0, 1)).await;
    f.stays_unchanged().await;
}

/// Positive control: a successful result writes `disabled: false`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_successful_result_persists_disabled_false() {
    let f = fixture("success").await;
    let (status, body) = f.post("/v1/messages", messages("hello", false)).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    f.settled(stats(1, 0, 0)).await;
    f.persisted_disabled_false().await;
}

/// Positive control: an availability-neutral result (a missing count_tokens endpoint)
/// still writes `disabled: false` and cools nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_neutral_result_persists_disabled_false() {
    let f = fixture("neutral").await;
    let body = json!({"model": MODEL, "messages": [{"role": "user", "content": "count"}]});
    let (status, reply) = f.post("/v1/messages/count_tokens", body).await;
    assert_eq!(status, 404, "{}", String::from_utf8_lossy(&reply));
    assert_eq!(
        f.up.paths.lock().unwrap().as_slice(),
        ["/v1/messages/count_tokens"],
        "the count reached the mock upstream once"
    );
    f.settled(stats(0, 1, 0)).await;
    assert!(
        f.rt.store().cooldowns(FILE).is_empty(),
        "a neutral result cools nothing: {:?}",
        f.rt.store().cooldowns(FILE)
    );
    f.persisted_disabled_false().await;
}
