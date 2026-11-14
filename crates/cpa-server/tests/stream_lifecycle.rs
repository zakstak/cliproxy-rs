//! Streaming lifecycle on translated and non-native routes (#114), through the real
//! router, dispatch and xAI executor against a loopback mock upstream with a fake API
//! key and a private auth-dir.
//!
//! The mock holds each answer until the test releases it (a semaphore, not a sleep), and
//! writes its SSE body in small pieces that split field names, line ends and JSON.
//! Each case checks what the client sees: every upstream event once, and exactly one
//! terminal outcome. Native Codex byte preservation, including `response.metadata`,
//! is pinned in `codex_native_request.rs` and `codex_native_ws.rs`; cancellation before
//! the first chunk in `xai_usage_cancel.rs` and `codex_usage_cancel.rs`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use futures_util::StreamExt;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use wreq::ws::message::Message;

const CREATED: &str = r#"{"type":"response.created","response":{"id":"resp_1","object":"response","model":"grok-4.7","status":"in_progress","output":[]}}"#;
const DELTA: &str =
    r#"{"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"rea"}"#;
const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"resp_1","object":"response","status":"completed","model":"grok-4.7","output":[{"id":"msg_1","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"ready"}]}],"usage":{"input_tokens":5,"output_tokens":1,"total_tokens":6}}}"#;
const FAILED: &str = r#"{"type":"response.failed","sequence_number":3,"response":{"id":"resp_1","status":"failed","error":{"type":"server_error","code":"server_error","message":"Upstream generation failed."}}}"#;

const TERMINAL: [&str; 5] = [
    "response.completed",
    "response.incomplete",
    "response.failed",
    "response.done",
    "error",
];

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

/// One upstream answer: the SSE body, and the Content-Length it announces (a larger
/// one ends the body early, as a cut connection).
#[derive(Clone)]
struct Answer {
    body: String,
    length: Option<usize>,
}

/// Answers every request with `answer` once `release` grants a permit, writing the body
/// `piece` bytes at a time with a flush and a pause between pieces.
async fn mock(answer: Answer, release: Arc<Semaphore>, piece: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let (mut conn, _) = listener.accept().await.unwrap();
            let (answer, release) = (answer.clone(), release.clone());
            tokio::spawn(async move {
                read_request(&mut conn).await;
                release.acquire().await.unwrap().forget();
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    answer.length.unwrap_or(answer.body.len())
                );
                let _ = conn.write_all(head.as_bytes()).await;
                for piece in answer.body.as_bytes().chunks(piece) {
                    let _ = conn.write_all(piece).await;
                    let _ = conn.flush().await;
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                let _ = conn.shutdown().await;
            });
        }
    });
    addr
}

async fn proxy(upstream: &str, extra: &str) -> (String, Arc<cpa_server::Runtime>) {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    // A private auth-dir: without one, credential loading reads the real ~/.cli-proxy-api.
    let auth_dir =
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("stream-lifecycle-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&auth_dir).unwrap();
    let config = Config::parse(&format!(
        "auth-dir: {}\naccess:\n  api-keys: [client-key]\napi-keys:\n  xai:\n    - name: xai-1\n      base-url: http://{upstream}/v1\n      keys:\n        - api-key: sk-fake-xai\n{extra}",
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
    (addr, rt)
}

/// A streamed `/v1/responses` turn: the status and the events the client parsed, as
/// (event name, data JSON).
async fn streamed(addr: &str) -> (u16, Vec<(String, Value)>) {
    let response = wreq::Client::new()
        .post(format!("http://{addr}/v1/responses"))
        .header("authorization", "Bearer client-key")
        .header("content-type", "application/json")
        .body(r#"{"model":"grok-4.7","input":"hi","stream":true}"#)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body = response.bytes().await.unwrap();
    (status, parse_sse(&String::from_utf8_lossy(&body)))
}

/// WHATWG `text/event-stream` parsing: events end at a blank line, lines at CRLF, LF or
/// CR; comments are skipped and an event the body cuts off is not dispatched.
fn parse_sse(text: &str) -> Vec<(String, Value)> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut events = vec![];
    let (mut name, mut data): (String, Option<String>) = (String::new(), None);
    for line in normalized.split_inclusive('\n') {
        let complete = line.ends_with('\n');
        let line = line.trim_end_matches('\n');
        if !complete {
            break;
        }
        if line.is_empty() {
            if let Some(data) = data.take() {
                let json = serde_json::from_str(&data).unwrap_or(Value::String(data));
                events.push((std::mem::take(&mut name), json));
            }
            name.clear();
            continue;
        }
        if line.starts_with(':') {
            continue;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => name = value.to_owned(),
            "data" => match &mut data {
                Some(d) => {
                    d.push('\n');
                    d.push_str(value);
                }
                None => data = Some(value.to_owned()),
            },
            _ => {}
        }
    }
    events
}

fn kinds(events: &[(String, Value)]) -> Vec<String> {
    events
        .iter()
        .map(|(_, data)| data["type"].as_str().unwrap_or_default().to_owned())
        .collect()
}

fn terminals(events: &[(String, Value)]) -> usize {
    kinds(events).iter().filter(|k| TERMINAL.contains(&k.as_str())).count()
}

/// Upstream frames split at every byte, with LF and CRLF line ends, named and unnamed
/// events in any order, a comment and split JSON payloads, reach the client as the same
/// events, each once, ending in one terminal event.
///
/// Witness: before #114 an unnamed event followed by a named one reached the client
/// as one frame carrying the second event's name. A `response.completed` name thus
/// labelled the `response.created` payload as the terminal event, and the real
/// completion was dropped.
#[tokio::test]
async fn fragmented_frames_reach_the_client_whole() {
    let body = format!(
        "event: response.created\r\ndata: {CREATED}\r\n\r\n: upstream comment\n\ndata: {DELTA}\n\nevent: response.output_text.delta\ndata: {DELTA}\n\nevent: response.completed\ndata: {COMPLETED}\n\n"
    );
    for piece in [1, 3, 7] {
        let release = Arc::new(Semaphore::new(1));
        let upstream = mock(
            Answer {
                body: body.clone(),
                length: None,
            },
            release,
            piece,
        )
        .await;
        let (addr, _rt) = proxy(&upstream, "").await;
        let (status, events) = streamed(&addr).await;
        assert_eq!(status, 200, "piece {piece}");
        assert_eq!(
            kinds(&events),
            [
                "response.created",
                "response.output_text.delta",
                "response.output_text.delta",
                "response.completed"
            ],
            "piece {piece}: {events:?}"
        );
        let created: Value = serde_json::from_str(CREATED).unwrap();
        assert_eq!(events[0].1["response"]["id"], created["response"]["id"]);
        for (name, data) in &events {
            assert!(
                name.is_empty() || data["type"] == name.as_str(),
                "piece {piece}: event {name:?} carries {data}"
            );
        }
        for (_, delta) in &events[1..3] {
            assert_eq!(delta["delta"], "rea", "piece {piece}");
        }
        assert_eq!(
            events[3].1["response"]["output"][0]["content"][0]["text"], "ready",
            "piece {piece}"
        );
        assert_eq!(terminals(&events), 1, "piece {piece}");
    }
}

/// A body cut off after its first event, its connection closed mid-frame, ends the
/// client's stream with exactly one terminal error and no completion.
#[tokio::test]
async fn a_cut_stream_ends_with_one_terminal_error() {
    let body = format!("data: {CREATED}\n\ndata: {}", &DELTA[..40]);
    let answer = Answer {
        length: Some(body.len() + 100),
        body,
    };
    let upstream = mock(answer, Arc::new(Semaphore::new(1)), 5).await;
    let (addr, _rt) = proxy(&upstream, "").await;
    let (status, events) = streamed(&addr).await;
    assert_eq!(status, 200);
    let kinds = kinds(&events);
    assert_eq!(
        kinds.first().map(String::as_str),
        Some("response.created"),
        "{events:?}"
    );
    assert_eq!(kinds.last().map(String::as_str), Some("error"), "{events:?}");
    assert_eq!(terminals(&events), 1, "{events:?}");
    assert!(!kinds.iter().any(|k| k == "response.output_text.delta"), "{events:?}");
}

/// An upstream failure is the stream's one terminal event: a completion the upstream
/// sends after it never reaches the client.
#[tokio::test]
async fn nothing_follows_an_upstream_failure() {
    let body = format!("data: {CREATED}\n\ndata: {FAILED}\n\ndata: {DELTA}\n\ndata: {COMPLETED}\n\n");
    let upstream = mock(Answer { body, length: None }, Arc::new(Semaphore::new(1)), 9).await;
    let (addr, _rt) = proxy(&upstream, "").await;
    let (status, events) = streamed(&addr).await;
    assert_eq!(status, 200);
    let kinds = kinds(&events);
    assert_eq!(
        kinds.first().map(String::as_str),
        Some("response.created"),
        "{events:?}"
    );
    assert_eq!(terminals(&events), 1, "{events:?}");
    assert!(
        matches!(kinds.last().map(String::as_str), Some("error" | "response.failed")),
        "{events:?}"
    );
    assert!(!kinds.iter().any(|k| k == "response.completed"), "{events:?}");
}

/// With `requests.streaming.keepalive-seconds`, a WebSocket turn gets ping control
/// frames while it waits for the upstream's first event, not only once events flow
/// (docs/codex/CONTRACT.md: "while a turn is idle"). The upstream answers only after
/// the client saw two pings; the turn then completes once.
#[tokio::test]
async fn a_bootstrapping_websocket_turn_gets_pings() {
    let release = Arc::new(Semaphore::new(0));
    let body = format!("data: {CREATED}\n\ndata: {COMPLETED}\n\n");
    let upstream = mock(Answer { body, length: None }, release.clone(), 64).await;
    let (addr, _rt) = proxy(&upstream, "requests:\n  streaming:\n    keepalive-seconds: 1\n").await;
    let mut socket = wreq::Client::new()
        .websocket(format!("ws://{addr}/v1/responses"))
        .header("authorization", "Bearer client-key")
        .send()
        .await
        .unwrap()
        .into_websocket()
        .await
        .unwrap();
    socket
        .send(Message::text(
            r#"{"type":"response.create","model":"grok-4.7","input":[{"type":"message","role":"user","content":"hi"}]}"#,
        ))
        .await
        .unwrap();
    let mut pings = 0;
    while pings < 2 {
        let next = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .unwrap_or_else(|_| panic!("no ping while the turn bootstraps, after {pings}"));
        match next {
            Some(Ok(Message::Ping(_))) => pings += 1,
            Some(Ok(Message::Pong(_))) => {}
            other => panic!("only pings before the upstream answers: {other:?}"),
        }
    }
    release.add_permits(1);
    let mut kinds = vec![];
    loop {
        let next = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .expect("the turn ends");
        match next {
            Some(Ok(Message::Text(text))) => {
                let event: Value = serde_json::from_str(text.as_str()).unwrap();
                let kind = event["type"].as_str().unwrap_or_default().to_owned();
                let done = TERMINAL.contains(&kind.as_str());
                kinds.push(kind);
                if done {
                    break;
                }
            }
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            other => panic!("unexpected {other:?} after {kinds:?}"),
        }
    }
    assert_eq!(kinds, ["response.created", "response.completed"]);
}
