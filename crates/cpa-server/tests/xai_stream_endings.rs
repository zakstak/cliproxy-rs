//! Credential health after a non-streaming xAI turn fails inside a 200 body, through the
//! real router, dispatch and scheduler, against a loopback mock upstream (v8.0.67
//! 949b43b3, issue #84). A body that ends without a terminal event is a request-scoped
//! 502: the single credential stays available and the next request is served. A
//! provider failure event keeps the provider's status, and its cooldown.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const CREATED: &str = r#"{"type":"response.created","response":{"id":"resp_1","object":"response","model":"grok-4.7","status":"in_progress","output":[]}}"#;
const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"resp_2","object":"response","status":"completed","model":"grok-4.7","output":[{"id":"msg_2","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"ready"}]}],"usage":{"input_tokens":5,"output_tokens":1,"total_tokens":6}}}"#;
const FAILED: &str = r#"{"type":"response.failed","sequence_number":3,"response":{"id":"resp_1","status":"failed","error":{"type":"server_error","code":"server_error","message":"Upstream generation failed."}}}"#;

async fn read_request(conn: &mut TcpStream) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = conn.read(&mut tmp).await.unwrap();
        assert!(n > 0, "upstream connection closed before the request was read");
        buf.extend_from_slice(&tmp[..n]);
        let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
        let length: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .map_or(0, |v| v.trim().parse().unwrap());
        if buf.len() >= end + 4 + length {
            return;
        }
    }
}

/// Answers the first request with `first` and every later one with a completed turn,
/// all as 200 SSE bodies; counts the requests.
async fn mock(first: String) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    tokio::spawn(async move {
        loop {
            let (mut conn, _) = listener.accept().await.unwrap();
            let n = counted.fetch_add(1, Ordering::SeqCst);
            let first = first.clone();
            tokio::spawn(async move {
                read_request(&mut conn).await;
                let body = if n == 0 {
                    first
                } else {
                    format!("data: {CREATED}\n\ndata: {COMPLETED}\n\n")
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = conn.write_all(head.as_bytes()).await;
                let _ = conn.write_all(body.as_bytes()).await;
                let _ = conn.shutdown().await;
            });
        }
    });
    (addr, calls)
}

struct Proxy {
    addr: String,
    calls: Arc<AtomicUsize>,
    _rt: Arc<cpa_server::Runtime>,
}

async fn proxy(first: String) -> Proxy {
    let (upstream, calls) = mock(first).await;
    // A private auth-dir: without one, credential loading reads the real ~/.cli-proxy-api.
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let auth_dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("xai-stream-endings-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&auth_dir).unwrap();
    let config = Config::parse(&format!(
        "auth-dir: {}\naccess:\n  api-keys: [client-key]\napi-keys:\n  xai:\n    - name: xai-1\n      base-url: http://{upstream}/v1\n      keys:\n        - api-key: sk-fake-xai\n",
        auth_dir.display()
    ))
    .unwrap();
    let credentials = cpa_core::config::credentials::load(&config);
    let executors = Executors {
        claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: Default::default(),
        meta: Default::default(),
        xai: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(config, credentials, executors));
    cpa_server::install_registry(&rt);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let app = cpa_server::router(rt.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Proxy { addr, calls, _rt: rt }
}

/// One non-streaming `/v1/responses` request: the status and the JSON body.
async fn turn(addr: &str) -> (u16, Value) {
    let body = r#"{"model":"grok-4.7","input":"hi","stream":false}"#;
    let mut conn = TcpStream::connect(addr).await.unwrap();
    let head = format!(
        "POST /v1/responses HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer client-key\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    conn.write_all(head.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    conn.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8_lossy(&out);
    let status = text[9..12].parse().unwrap();
    let json = text.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (status, serde_json::from_str(json).unwrap_or(Value::Null))
}

/// An unterminated body and an empty one fail only that request: one upstream call,
/// a 502 that is no timeout, and the same credential serves the next request.
#[tokio::test]
async fn unterminated_body_keeps_the_credential_available() {
    for first in [format!("data: {CREATED}\n\n"), String::new()] {
        let p = proxy(first.clone()).await;
        let (status, body) = turn(&p.addr).await;
        assert_eq!(status, 502, "{first:?}: {body}");
        assert_ne!(body["error"]["code"], "request_timeout", "{first:?}: {body}");
        assert_eq!(p.calls.load(Ordering::SeqCst), 1, "{first:?}: not retried");
        let (status, body) = turn(&p.addr).await;
        assert_eq!(status, 200, "{first:?}: credential still available: {body}");
        assert_eq!(body["output"][0]["content"][0]["text"], "ready", "{body}");
        assert_eq!(p.calls.load(Ordering::SeqCst), 2);
    }
}

/// A provider failure event is the provider's 502 with its message, and the credential
/// cools as for an upstream 502 status: the next request does not reach the upstream.
#[tokio::test]
async fn failed_event_keeps_provider_status_and_cooldown() {
    let p = proxy(format!("data: {CREATED}\n\ndata: {FAILED}\n\n")).await;
    let (status, body) = turn(&p.addr).await;
    assert_eq!(status, 502, "{body}");
    assert!(
        body.to_string().contains("Upstream generation failed."),
        "the provider's message reaches the client: {body}"
    );
    let calls = p.calls.load(Ordering::SeqCst);
    let (status, body) = turn(&p.addr).await;
    assert_ne!(status, 200, "the cooled credential served a request: {body}");
    assert_eq!(p.calls.load(Ordering::SeqCst), calls, "no upstream call while cooled");
}
