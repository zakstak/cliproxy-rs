//! The history store records every finished upstream attempt through the real router,
//! dispatch and usage tracker, against a loopback mock upstream, whether or not Go's
//! usage statistics are on, and keeps neither the client key nor the provider key.

use std::sync::Arc;
use std::time::Duration;

use cliproxy_internal::config::Config;
use cliproxy_internal::persistence::history::memory::MemoryHistory;
use cliproxy_internal::persistence::history::{Cursor, HistoryStore, RequestFilter};
use cliproxy_internal::providers::Executors;
use cliproxy_internal::providers::claude::ClaudeExecutor;
use cliproxy_internal::runtime::history::Recorder;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

const CREATED: &str = r#"{"type":"response.created","response":{"id":"resp_1","object":"response","model":"grok-4.7","status":"in_progress","output":[]}}"#;
const DELTA: &str =
    r#"{"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"hi"}"#;
const COMPLETED_USAGE: &str = r#"{"type":"response.completed","response":{"id":"resp_1","object":"response","model":"grok-4.7","status":"completed","output":[],"usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}}}"#;

#[derive(Clone, Copy)]
#[allow(dead_code)]
enum Upstream {
    Hang,
    Partial,
    Reset,
    Complete(&'static str),
}

fn chunk(data: &str) -> String {
    format!("{:x}\r\n{data}\r\n", data.len())
}

fn sse(event: &str) -> String {
    chunk(&format!("data: {event}\n\n"))
}

const SSE_HEAD: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";

/// Reads one HTTP/1.1 request (headers and Content-Length body).
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

/// One mock upstream connection per call; `ready` fires once the mock has answered as
/// far as it is going to before the client leaves.
async fn mock(behaviour: Upstream) -> (String, mpsc::UnboundedReceiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (ready, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let (mut conn, _) = listener.accept().await.unwrap();
            let ready = ready.clone();
            tokio::spawn(async move {
                read_request(&mut conn).await;
                match behaviour {
                    Upstream::Hang => {}
                    Upstream::Partial | Upstream::Reset => {
                        let head = format!("{SSE_HEAD}{}{}", sse(CREATED), sse(DELTA));
                        conn.write_all(head.as_bytes()).await.unwrap();
                        conn.flush().await.unwrap();
                    }
                    Upstream::Complete(completed) => {
                        let body = format!("{SSE_HEAD}{}{}0\r\n\r\n", sse(CREATED), sse(completed));
                        conn.write_all(body.as_bytes()).await.unwrap();
                    }
                }
                let _ = ready.send(());
                if let Upstream::Reset = behaviour {
                    drop(conn);
                    return;
                }
                // Keep the connection open until the test runtime ends.
                std::future::pending::<()>().await;
                drop(conn);
            });
        }
    });
    (addr, rx)
}

async fn proxy(statistics: bool) -> (String, Arc<cliproxy_internal::runtime::Runtime>, Arc<MemoryHistory>) {
    let (upstream, _ready) = mock(Upstream::Complete(COMPLETED_USAGE)).await;
    // A private auth-dir: without one, credential loading reads the real ~/.cli-proxy-api.
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let auth_dir = std::env::temp_dir().join(format!("cpa-history-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&auth_dir).unwrap();
    let config = Config::parse(&format!(
        "auth-dir: {}\naccess:\n  api-keys: [client-key-secret]\napi-keys:\n  xai:\n    - name: xai-1\n      base-url: http://{upstream}/v1\n      keys:\n        - api-key: sk-fake-xai-secret\n",
        auth_dir.display()
    ))
    .unwrap();
    let credentials = cliproxy_internal::config::credentials::load(&config);
    let executors = Executors {
        claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: Default::default(),
        meta: Default::default(),
        xai: Default::default(),
    };
    let rt = Arc::new(cliproxy_internal::gateway::testing::runtime(
        config,
        credentials,
        executors,
    ));
    rt.usage_queue().configure(
        true,
        &Config::parse(&format!(
            "observability: {{usage: {{usage-statistics-enabled: {statistics}}}}}\n"
        ))
        .unwrap(),
    );
    let store = Arc::new(MemoryHistory::default());
    rt.usage_queue().set_history(Recorder::start(store.clone(), 0));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let app = cliproxy_internal::gateway::router(rt.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, rt, store)
}

async fn complete_request(addr: &str) {
    let body = r#"{"model":"grok-4.7","input":"hi","stream":true}"#;
    let mut conn = TcpStream::connect(addr).await.unwrap();
    let head = format!(
        "POST /v1/responses HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer client-key-secret\r\nX-Request-Id: req-history-1\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    conn.write_all(head.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    conn.read_to_end(&mut response).await.unwrap();
    assert!(
        response.starts_with(b"HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&response)
    );
}

async fn stored(store: &MemoryHistory) -> Vec<cliproxy_internal::persistence::history::StoredRequest> {
    // Generous: only stops a broken test from hanging.
    for _ in 0..500 {
        let page = store
            .requests(RequestFilter::default(), Cursor::Newest, 10)
            .await
            .unwrap();
        if !page.rows.is_empty() {
            return page.rows;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Vec::new()
}

#[tokio::test]
async fn history_records_attempts_with_usage_statistics_off_and_keeps_no_keys() {
    let (addr, rt, store) = proxy(false).await;
    complete_request(&addr).await;
    let rows = stored(&store).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let e = &rows[0].entry;
    assert_eq!(e.provider, "xai");
    assert_eq!(e.model, "grok-4.7");
    assert_eq!(e.endpoint, "POST /v1/responses");
    assert!(!e.failed);
    assert_eq!(e.status, 200);
    assert_eq!((e.input_tokens, e.output_tokens, e.total_tokens), (7, 3, 10));
    assert!(!e.auth_index.is_empty());
    assert_eq!(e.account, "", "a key-only credential has no account name to keep");
    assert!(e.at.0 > 0);
    let json = serde_json::to_string(&rows[0]).unwrap();
    assert!(!json.contains("client-key-secret"), "{json}");
    assert!(!json.contains("sk-fake-xai-secret"), "{json}");
    assert!(
        rt.usage_queue().pop_oldest(10).is_empty(),
        "the queue stays off with usage statistics off"
    );
}

#[tokio::test]
async fn history_and_the_usage_queue_both_get_the_attempt() {
    let (addr, rt, store) = proxy(true).await;
    complete_request(&addr).await;
    assert_eq!(stored(&store).await.len(), 1);
    let queued = rt.usage_queue().pop_oldest(10);
    assert_eq!(queued.len(), 1);
    let record: serde_json::Value = serde_json::from_slice(&queued[0]).unwrap();
    assert_eq!(record["tokens"]["total_tokens"], 10, "Go's record is unchanged");
}
