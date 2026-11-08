//! Usage records of the translated Codex HTTP route (#54): a Chat Completions, legacy
//! Completions or Claude Messages client served by a Codex credential, through the real
//! router, dispatch and usage tracker, against a loopback mock upstream with a fake API
//! key and a private auth-dir. Each case runs with stream bootstrap buffering off and on:
//! with it on, the executor holds the handshake events until it decides the attempt, so
//! the client can leave inside the executor's bootstrap rather than inside the stream.
//!
//! An attempt the client abandons is recorded once, as a failure with status 499 and
//! `context canceled`: the proxy's own accounting convention (`cpa_exec::cancel`), which
//! no Codex source defines. An attempt that ended before the client left keeps the
//! record it already settled, and a cancellation changes nothing the scheduler sees.
//!
//! Each phase is synchronized on a signal, not a sleep: the mock reports when it has
//! read the request and when the proxy closed the upstream connection, a streamed client
//! reads its first event, and a pending executor is known to hold the response head from
//! the quota header it observes on arrival.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::timeout;

const CREATED: &str = r#"{"type":"response.created","response":{"id":"resp_1","object":"response","model":"gpt-5.5","status":"in_progress","output":[]}}"#;
const DELTA: &str =
    r#"{"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"hi"}"#;
const COMPLETED_USAGE: &str = r#"{"type":"response.completed","response":{"id":"resp_1","object":"response","model":"gpt-5.5","status":"completed","output":[{"type":"message","id":"msg_1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"hi","annotations":[]}]}],"usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}}}"#;
const COMPLETED_BARE: &str = r#"{"type":"response.completed","response":{"id":"resp_1","object":"response","model":"gpt-5.5","status":"completed","output":[{"type":"message","id":"msg_1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"hi","annotations":[]}]}]}}"#;
const FAILED: &str = r#"{"type":"response.failed","response":{"id":"resp_1","object":"response","status":"failed","error":{"code":"server_error","message":"upstream broke"}}}"#;

/// Every wait in these tests ends by this deadline.
const BOUND: Duration = Duration::from_secs(5);
/// How long a test watches for a second record or a replayed request after the first.
const QUIET: Duration = Duration::from_millis(150);

/// The translated client formats, each on its own route.
#[derive(Clone, Copy, Debug)]
enum Client {
    Chat,
    Completions,
    Messages,
}

const CLIENTS: [Client; 3] = [Client::Chat, Client::Completions, Client::Messages];

impl Client {
    fn path(self) -> &'static str {
        match self {
            Client::Chat => "/v1/chat/completions",
            Client::Completions => "/v1/completions",
            Client::Messages => "/v1/messages",
        }
    }

    fn body(self, stream: bool) -> String {
        match self {
            Client::Chat => {
                format!(r#"{{"model":"gpt-5.5","messages":[{{"role":"user","content":"hi"}}],"stream":{stream}}}"#)
            }
            Client::Completions => format!(r#"{{"model":"gpt-5.5","prompt":"hi","stream":{stream}}}"#),
            Client::Messages => format!(
                r#"{{"model":"gpt-5.5","max_tokens":16,"messages":[{{"role":"user","content":"hi"}}],"stream":{stream}}}"#
            ),
        }
    }

    /// The text that ends a complete client stream.
    fn stream_end(self) -> &'static str {
        match self {
            Client::Chat | Client::Completions => "[DONE]",
            Client::Messages => "message_stop",
        }
    }
}

/// What the mock upstream does on one connection, after reading the request.
#[derive(Clone, Copy)]
enum Upstream {
    /// Never answers.
    Hang,
    /// Sends the SSE head and `response.created` (a handshake event), then waits.
    Handshake,
    /// Sends the SSE head, `response.created` and a text delta, then waits.
    PartialStream,
    /// Sends what `PartialStream` sends, then closes the connection mid-body.
    Reset,
    /// Sends what `PartialStream` sends, then `response.failed`, then waits.
    FailedEvent,
    /// A complete SSE body: `response.created`, a text delta, then this terminal event.
    Complete(&'static str),
}

fn chunk(data: &str) -> String {
    format!("{:x}\r\n{data}\r\n", data.len())
}

fn sse(event: &str) -> String {
    format!("data: {event}\n\n")
}

/// The quota header marks, for the test, that the executor saw the response head.
const QUOTA: &str = "x-ratelimit-remaining-requests: 9\r\n";

fn sse_head() -> String {
    format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n{QUOTA}Transfer-Encoding: chunked\r\n\r\n")
}

/// Reads one HTTP/1.1 request (headers and Content-Length body).
async fn read_request(conn: &mut TcpStream) -> bool {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let Ok(n) = conn.read(&mut tmp).await else { return false };
        if n == 0 {
            return false;
        }
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
            return true;
        }
    }
}

struct Mock {
    addr: String,
    /// One message per request read, once the mock answered as far as it will.
    answered: mpsc::UnboundedReceiver<()>,
    /// One message per connection the proxy closed.
    closed: mpsc::UnboundedReceiver<()>,
    requests: Arc<AtomicUsize>,
}

/// The mock answers its n-th connection with `script[n]` (the last entry repeats). A
/// connection task ends when the proxy closes the connection, or with the test's runtime.
async fn mock(script: Vec<Upstream>) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (answered_tx, answered) = mpsc::unbounded_channel();
    let (closed_tx, closed) = mpsc::unbounded_channel();
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = requests.clone();
    tokio::spawn(async move {
        let mut n = 0;
        while let Ok((mut conn, _)) = listener.accept().await {
            let behaviour = script[n.min(script.len() - 1)];
            n += 1;
            let (answered, closed, counter) = (answered_tx.clone(), closed_tx.clone(), counter.clone());
            tokio::spawn(async move {
                if read_request(&mut conn).await {
                    counter.fetch_add(1, Ordering::SeqCst);
                    let partial = || format!("{}{}{}", sse_head(), chunk(&sse(CREATED)), chunk(&sse(DELTA)));
                    let reply = match behaviour {
                        Upstream::Hang => String::new(),
                        Upstream::Handshake => format!("{}{}", sse_head(), chunk(&sse(CREATED))),
                        Upstream::PartialStream | Upstream::Reset => partial(),
                        Upstream::FailedEvent => format!("{}{}", partial(), chunk(&sse(FAILED))),
                        Upstream::Complete(terminal) => format!(
                            "{}{}0\r\n\r\n",
                            sse_head(),
                            chunk(&format!("{}{}{}", sse(CREATED), sse(DELTA), sse(terminal)))
                        ),
                    };
                    if conn.write_all(reply.as_bytes()).await.is_ok() {
                        let _ = conn.flush().await;
                    }
                    let _ = answered.send(());
                    if let Upstream::Reset = behaviour {
                        return;
                    }
                }
                // Hold the connection until the proxy closes it.
                let mut tmp = [0u8; 1024];
                while matches!(conn.read(&mut tmp).await, Ok(n) if n > 0) {}
                let _ = closed.send(());
            });
        }
    });
    Mock {
        addr,
        answered,
        closed,
        requests,
    }
}

struct Proxy {
    addr: String,
    rt: Arc<cpa_server::Runtime>,
    credential_id: String,
    upstream: Mock,
    _auth_dir: TempDir,
}

/// A private auth-dir, removed with the test: without one, credential loading reads the
/// real `~/.cli-proxy-api`.
struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn proxy(script: Vec<Upstream>, buffering: bool) -> Proxy {
    let upstream = mock(script).await;
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let auth_dir =
        TempDir(std::env::temp_dir().join(format!("cpa-codex-translated-usage-cancel-{}-{n}", std::process::id())));
    std::fs::create_dir_all(&auth_dir.0).unwrap();
    let config = Config::parse(&format!(
        "auth-dir: {}\naccess:\n  api-keys: [client-key]\ncodex:\n  stream-bootstrap-buffering: {buffering}\ncodex-api-key:\n  - api-key: sk-fake-codex\n    base-url: http://{}/v1\n",
        auth_dir.0.display(),
        upstream.addr
    ))
    .unwrap();
    let credentials = cpa_core::config::credentials::load(&config);
    assert_eq!(credentials.len(), 1, "one configured Codex key");
    let credential_id = credentials[0].id.clone();
    let executors = Executors {
        claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: Default::default(),
        meta: Default::default(),
        xai: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(config, credentials, executors));
    cpa_server::install_registry(&rt);
    rt.usage_queue().configure(
        true,
        &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let app = cpa_server::router(rt.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Proxy {
        addr,
        rt,
        credential_id,
        upstream,
        _auth_dir: auth_dir,
    }
}

impl Proxy {
    /// Opens a client connection and sends `client`'s request.
    async fn request(&self, client: Client, stream: bool) -> TcpStream {
        let body = client.body(stream);
        let mut conn = TcpStream::connect(&self.addr).await.unwrap();
        let head = format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer client-key\r\nx-api-key: client-key\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            client.path(),
            self.addr,
            body.len()
        );
        conn.write_all(head.as_bytes()).await.unwrap();
        conn
    }

    /// The mock read the request and answered as far as it will.
    async fn answered(&mut self) {
        timeout(BOUND, self.upstream.answered.recv())
            .await
            .expect("the upstream received the request")
            .unwrap();
    }

    /// The proxy closed the upstream connection.
    async fn upstream_closed(&mut self) {
        timeout(BOUND, self.upstream.closed.recv())
            .await
            .expect("the proxy closes the abandoned upstream connection")
            .unwrap();
    }

    /// The executor saw the response head (its quota header).
    async fn headers_observed(&self) {
        let codex = &self.rt.executors.codex;
        timeout(BOUND, async {
            while codex.quota().snapshot(&self.credential_id).is_none() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the executor observes the upstream response head");
    }

    /// The records published so far, after waiting for `want` and then [`QUIET`] for
    /// any extra one.
    async fn records(&self, want: usize) -> Vec<Value> {
        let queue = self.rt.usage_queue();
        let mut got = Vec::new();
        let _ = timeout(BOUND, async {
            while got.len() < want {
                got.extend(queue.pop_oldest(10));
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        tokio::time::sleep(QUIET).await;
        got.extend(queue.pop_oldest(10));
        got.iter().map(|r| serde_json::from_slice(r).unwrap()).collect()
    }

    /// The one record of the attempt.
    async fn record(&self, case: &str) -> Value {
        let mut got = self.records(1).await;
        assert_eq!(got.len(), 1, "{case}: {got:?}");
        got.remove(0)
    }

    fn requests(&self) -> usize {
        self.upstream.requests.load(Ordering::SeqCst)
    }
}

/// (failed, status, failure text, total tokens).
fn outcome(record: &Value) -> (bool, i64, String, i64) {
    (
        record["failed"].as_bool().unwrap(),
        record["fail"]["status_code"].as_i64().unwrap(),
        record["fail"]["body"].as_str().unwrap().to_owned(),
        record["tokens"]["total_tokens"].as_i64().unwrap(),
    )
}

fn canceled() -> (bool, i64, String, i64) {
    (true, 499, "context canceled".into(), 0)
}

/// The record carries the upstream response head the server received (a stream's).
fn upstream_head(record: &Value) -> bool {
    record["response_headers"]["X-Ratelimit-Remaining-Requests"].is_array()
}

/// Reads until the client has received its first event.
async fn first_event(conn: &mut TcpStream, case: &str) {
    let mut seen = Vec::new();
    let mut tmp = [0u8; 4096];
    while !String::from_utf8_lossy(&seen).contains("data:") {
        let n = timeout(BOUND, conn.read(&mut tmp))
            .await
            .unwrap_or_else(|_| panic!("{case}: the first event reaches the client"))
            .unwrap();
        assert!(n > 0, "{case}: stream ended early: {}", String::from_utf8_lossy(&seen));
        seen.extend_from_slice(&tmp[..n]);
    }
}

/// Reads the whole client response: its status line and de-chunked body.
async fn read_response(conn: &mut TcpStream) -> (String, String) {
    let mut raw = Vec::new();
    timeout(BOUND, conn.read_to_end(&mut raw))
        .await
        .expect("the proxy ends the response")
        .unwrap();
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("response head");
    let head = String::from_utf8_lossy(&raw[..end]).into_owned();
    let mut rest = &raw[end + 4..];
    if !head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        return (head, String::from_utf8_lossy(rest).into_owned());
    }
    let mut body = Vec::new();
    loop {
        let line = rest.windows(2).position(|w| w == b"\r\n").expect("chunk size");
        let size = usize::from_str_radix(std::str::from_utf8(&rest[..line]).unwrap().trim(), 16).unwrap();
        rest = &rest[line + 2..];
        if size == 0 {
            return (head, String::from_utf8_lossy(&body).into_owned());
        }
        body.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}

/// The client leaves while the executor waits for the upstream response head, streamed
/// or buffered: one canceled record, and the abandoned upstream request is closed.
#[tokio::test]
async fn cancel_while_awaiting_headers_records_one_cancellation() {
    for client in CLIENTS {
        for stream in [true, false] {
            for buffering in [false, true] {
                let case = format!("{client:?} stream={stream} buffering={buffering}");
                let mut p = proxy(vec![Upstream::Hang], buffering).await;
                let conn = p.request(client, stream).await;
                p.answered().await;
                drop(conn);
                p.upstream_closed().await;
                let record = p.record(&case).await;
                assert_eq!(outcome(&record), canceled(), "{case}");
                assert!(!upstream_head(&record), "{case}: {record}");
            }
        }
    }
}

/// The client leaves after the response head arrived and before any generated output:
/// with bootstrap buffering, the executor still holds the handshake event (no response
/// is committed); without it, the stream is the client's and waits for the next event.
#[tokio::test]
async fn cancel_after_the_handshake_records_one_cancellation() {
    for client in CLIENTS {
        for buffering in [false, true] {
            let case = format!("{client:?} buffering={buffering}");
            let mut p = proxy(vec![Upstream::Handshake], buffering).await;
            let conn = p.request(client, true).await;
            p.answered().await;
            p.headers_observed().await;
            drop(conn);
            p.upstream_closed().await;
            let record = p.record(&case).await;
            assert_eq!(outcome(&record), canceled(), "{case}");
        }
    }
}

/// The client leaves while the stream waits for the next upstream event, after the first
/// event reached it.
#[tokio::test]
async fn cancel_while_awaiting_stream_data_records_one_cancellation() {
    for client in CLIENTS {
        for buffering in [false, true] {
            let case = format!("{client:?} buffering={buffering}");
            let mut p = proxy(vec![Upstream::PartialStream], buffering).await;
            let mut conn = p.request(client, true).await;
            p.answered().await;
            first_event(&mut conn, &case).await;
            drop(conn);
            p.upstream_closed().await;
            let record = p.record(&case).await;
            assert_eq!(outcome(&record), canceled(), "{case}");
            assert!(upstream_head(&record), "{case}: {record}");
        }
    }
}

/// A buffered client leaves while the executor reads the upstream events to aggregate.
#[tokio::test]
async fn cancel_while_reading_buffered_body_records_one_cancellation() {
    for client in CLIENTS {
        let case = format!("{client:?}");
        let mut p = proxy(vec![Upstream::PartialStream], false).await;
        let conn = p.request(client, false).await;
        p.answered().await;
        p.headers_observed().await;
        drop(conn);
        p.upstream_closed().await;
        let record = p.record(&case).await;
        assert_eq!(outcome(&record), canceled(), "{case}");
    }
}

/// A completion keeps its record when the client then leaves, streamed or buffered: its
/// usage, or, for a completion without usage, a success with no tokens rather than a
/// cancellation.
#[tokio::test]
async fn a_completion_keeps_its_record_when_the_client_then_leaves() {
    for client in CLIENTS {
        for stream in [true, false] {
            for buffering in [false, true] {
                for (terminal, tokens) in [(COMPLETED_USAGE, 10), (COMPLETED_BARE, 0)] {
                    let case = format!("{client:?} stream={stream} buffering={buffering} tokens={tokens}");
                    let mut p = proxy(vec![Upstream::Complete(terminal)], buffering).await;
                    let mut conn = p.request(client, stream).await;
                    p.answered().await;
                    let (head, body) = read_response(&mut conn).await;
                    assert!(head.starts_with("HTTP/1.1 200"), "{case}: {head}");
                    if stream {
                        assert!(body.contains(client.stream_end()), "{case}: {body}");
                    } else {
                        let _: Value = serde_json::from_str(&body).unwrap_or_else(|e| panic!("{case}: {e}: {body}"));
                    }
                    drop(conn);
                    let record = p.record(&case).await;
                    assert_eq!(outcome(&record), (false, 200, String::new(), tokens), "{case}");
                }
            }
        }
    }
}

/// An upstream failure keeps its own record when the client then leaves: a connection
/// reset mid-stream, or a `response.failed` event while the upstream stays open.
#[tokio::test]
async fn an_upstream_failure_keeps_its_record_when_the_client_then_leaves() {
    for client in CLIENTS {
        for buffering in [false, true] {
            for (name, upstream) in [("reset", Upstream::Reset), ("failed", Upstream::FailedEvent)] {
                let case = format!("{client:?} buffering={buffering} {name}");
                let mut p = proxy(vec![upstream], buffering).await;
                let mut conn = p.request(client, true).await;
                p.answered().await;
                let (head, _) = read_response(&mut conn).await;
                assert!(head.starts_with("HTTP/1.1 200"), "{case}: {head}");
                drop(conn);
                let record = p.record(&case).await;
                let (failed, status, body, tokens) = outcome(&record);
                assert!(failed, "{case}: {record}");
                assert_ne!((status, body.as_str()), (499, "context canceled"), "{case}");
                assert_eq!(tokens, 0, "{case}");
            }
        }
    }
}

/// A cancellation is accounting only: the abandoned request is not replayed, and the only
/// credential is not cooled down, so the next request on it succeeds.
#[tokio::test]
async fn a_cancellation_neither_replays_nor_cools_the_credential() {
    for client in CLIENTS {
        for buffering in [false, true] {
            let case = format!("{client:?} buffering={buffering}");
            let mut p = proxy(
                vec![Upstream::PartialStream, Upstream::Complete(COMPLETED_USAGE)],
                buffering,
            )
            .await;
            let mut conn = p.request(client, true).await;
            p.answered().await;
            first_event(&mut conn, &case).await;
            drop(conn);
            p.upstream_closed().await;
            assert_eq!(outcome(&p.record(&case).await), canceled(), "{case}");
            assert_eq!(p.requests(), 1, "{case}: no replay after the client left");

            let mut conn = p.request(client, true).await;
            p.answered().await;
            let (head, body) = read_response(&mut conn).await;
            assert!(head.starts_with("HTTP/1.1 200"), "{case}: {head}");
            assert!(body.contains(client.stream_end()), "{case}: {body}");
            assert_eq!(
                outcome(&p.record(&case).await),
                (false, 200, String::new(), 10),
                "{case}"
            );
            assert_eq!(p.requests(), 2, "{case}");
        }
    }
}
