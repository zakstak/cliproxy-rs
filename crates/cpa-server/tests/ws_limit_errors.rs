//! Subscription-limit failures on the Responses WebSocket, through the real router and
//! Codex executor against a scripted loopback upstream (fake API keys, private
//! `auth-dir`). A native Codex client gets the final limit as the wrapped `error` event
//! current Codex classifies (docs/DIFFERENCES-FROM-GO.md, "Codex subscription-limit
//! errors on the Responses WebSocket"); every other client keeps Go's silent close.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_server::router;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use wreq::ws::message::Message;

const CODEX_UA: &str = "codex_cli_rs/0.150.0 (Linux; x86_64)";
const MODEL: &str = "gpt-fixture";
const CREATE: &str = r#"{"type":"response.create","model":"gpt-fixture","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#;

/// Scripted replies per upstream API key, and what each key received.
#[derive(Default)]
struct Mock {
    script: Mutex<HashMap<String, VecDeque<Value>>>,
    /// Replies in arrival order, whichever key asks, once its own script is empty.
    shared: Mutex<VecDeque<Value>>,
    /// (api key, "http" | "ws_dial" | "ws_frame", body)
    seen: Mutex<Vec<(String, String, String)>>,
}

impl Mock {
    fn push(&self, key: &str, reply: Value) {
        self.script
            .lock()
            .unwrap()
            .entry(key.to_owned())
            .or_default()
            .push_back(reply);
    }

    fn pop(&self, key: &str) -> Option<Value> {
        let own = self.script.lock().unwrap().get_mut(key).and_then(VecDeque::pop_front);
        own.or_else(|| self.shared.lock().unwrap().pop_front())
    }

    fn seen(&self, key: &str, kind: &str) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, t, _)| k == key && t == kind)
            .map(|(_, _, body)| body.clone())
            .collect()
    }
}

fn bearer(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default()
        .to_owned()
}

async fn upstream(State(mock): State<Arc<Mock>>, mut req: axum::extract::Request) -> Response {
    let headers = req.headers().clone();
    let key = bearer(&headers);
    if let Some(accept) = headers.get("sec-websocket-key") {
        mock.seen
            .lock()
            .unwrap()
            .push((key.clone(), "ws_dial".into(), String::new()));
        let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(accept.as_bytes());
        let upgrade = hyper::upgrade::on(&mut req);
        tokio::spawn(async move {
            if let Ok(upgraded) = upgrade.await {
                socket(mock, key, hyper_util::rt::TokioIo::new(upgraded)).await;
            }
        });
        return Response::builder()
            .status(StatusCode::SWITCHING_PROTOCOLS)
            .header("upgrade", "websocket")
            .header("connection", "Upgrade")
            .header("sec-websocket-accept", accept)
            .body(axum::body::Body::empty())
            .unwrap();
    }
    let body = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap();
    mock.seen
        .lock()
        .unwrap()
        .push((key.clone(), "http".into(), String::from_utf8_lossy(&body).into_owned()));
    let Some(reply) = mock.pop(&key) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    if let Some(status) = reply["status"].as_u64() {
        let mut response = (
            StatusCode::from_u16(status as u16).unwrap(),
            [("content-type", "application/json")],
            reply["body"].as_str().unwrap().to_owned(),
        )
            .into_response();
        for (k, v) in reply["headers"].as_object().into_iter().flatten() {
            response.headers_mut().append(
                k.parse::<axum::http::HeaderName>().unwrap(),
                v.as_str().unwrap().parse().unwrap(),
            );
        }
        return response;
    }
    let mut sse = String::new();
    for event in reply["events"].as_array().into_iter().flatten() {
        let event = event.as_str().unwrap();
        sse.push_str(&format!(
            "event: {}\ndata: {event}\n\n",
            cpa_common::rawjson::get(event, "type").str()
        ));
    }
    ([("content-type", "text/event-stream")], sse).into_response()
}

async fn read_frame<S: AsyncRead + Unpin>(io: &mut S) -> Option<(u8, Vec<u8>)> {
    let mut head = [0u8; 2];
    io.read_exact(&mut head).await.ok()?;
    let len = match head[1] & 0x7f {
        126 => u64::from(io.read_u16().await.ok()?),
        127 => io.read_u64().await.ok()?,
        n => u64::from(n),
    };
    let mut mask = [0u8; 4];
    if head[1] & 0x80 != 0 {
        io.read_exact(&mut mask).await.ok()?;
    }
    let mut payload = vec![0u8; usize::try_from(len).ok()?];
    io.read_exact(&mut payload).await.ok()?;
    for (i, b) in payload.iter_mut().enumerate() {
        *b ^= mask[i % 4];
    }
    Some((head[0] & 0x0f, payload))
}

async fn write_frame<S: AsyncWrite + Unpin>(io: &mut S, opcode: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut frame = vec![0x80 | opcode];
    match payload.len() {
        n if n < 126 => frame.push(n as u8),
        n if n <= 0xffff => {
            frame.push(126);
            frame.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            frame.push(127);
            frame.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    frame.extend_from_slice(payload);
    io.write_all(&frame).await?;
    io.flush().await
}

async fn socket<S: AsyncRead + AsyncWrite + Unpin>(mock: Arc<Mock>, key: String, mut io: S) {
    while let Some((opcode, payload)) = read_frame(&mut io).await {
        match opcode {
            0x1 => {}
            0x8 => {
                let _ = write_frame(&mut io, 0x8, &payload).await;
                return;
            }
            0x9 => {
                let _ = write_frame(&mut io, 0xa, &payload).await;
                continue;
            }
            _ => continue,
        }
        mock.seen.lock().unwrap().push((
            key.clone(),
            "ws_frame".into(),
            String::from_utf8_lossy(&payload).into_owned(),
        ));
        let Some(reply) = mock.pop(&key) else { continue };
        for event in reply["events"].as_array().into_iter().flatten() {
            let _ = write_frame(&mut io, 0x1, event.as_str().unwrap().as_bytes()).await;
        }
        if reply["then"] == "close" {
            let _ = write_frame(&mut io, 0x8, &[&1000u16.to_be_bytes()[..], b"bye"].concat()).await;
            return;
        }
    }
}

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("127.0.0.1:{}", addr.port())
}

struct Proxy {
    addr: String,
    mock: Arc<Mock>,
    _auth_dir: AuthDir,
}

/// A private, empty `auth-dir`, removed afterwards.
struct AuthDir(std::path::PathBuf);

impl Drop for AuthDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One proxy over `keys` (API key, upstream WebSocket on), each serving [`MODEL`].
async fn proxy(keys: &[(&str, bool)]) -> Proxy {
    proxy_with(keys, "").await
}

/// [`proxy`] with `extra` appended to the config.
async fn proxy_with(keys: &[(&str, bool)], extra: &str) -> Proxy {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let mock = Arc::new(Mock::default());
    let upstream_addr = serve(axum::Router::new().fallback(upstream).with_state(mock.clone())).await;
    let dir = std::env::temp_dir().join(format!(
        "cpa-ws-limit-auths-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let mut yaml = format!("host: \"\"\nauth-dir: '{}'\n{extra}codex-api-key:\n", dir.display());
    for (key, ws) in keys {
        yaml.push_str(&format!("  - api-key: {key}\n    base-url: http://{upstream_addr}\n"));
        if *ws {
            yaml.push_str("    websockets: true\n");
        }
        yaml.push_str(&format!("    models:\n      - name: {MODEL}\n"));
    }
    let cfg = Config::parse(&yaml).unwrap();
    let credentials = cpa_core::config::credentials::from_config(&cfg);
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        meta: Default::default(),
        xai: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(cfg, credentials, executors));
    cpa_server::install_registry(&rt);
    let addr = serve(router(rt.clone())).await;
    Proxy {
        addr,
        mock,
        _auth_dir: AuthDir(dir),
    }
}

/// Serializes the tests: the model registry overlay is process-wide.
static REGISTRY: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

type Socket = wreq::ws::WebSocket;

async fn connect(proxy: &Proxy, codex: bool) -> Socket {
    let mut request = wreq::Client::new().websocket(format!("ws://{}/backend-api/codex/responses", proxy.addr));
    if codex {
        request = request
            .header("user-agent", CODEX_UA)
            .header("originator", "codex_cli_rs");
    }
    request.send().await.unwrap().into_websocket().await.unwrap()
}

#[derive(Debug, PartialEq)]
enum Frame {
    Text(Value),
    Close(u16, String),
}

/// Frames until `stop` matches a text frame's type, or the connection closes.
async fn read(socket: &mut Socket, stop: &str) -> Vec<Frame> {
    let mut got = Vec::new();
    loop {
        let next = tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .unwrap_or_else(|_| panic!("read timed out after {got:?}"));
        match next {
            Some(Ok(Message::Text(text))) => {
                let value: Value = serde_json::from_str(text.as_str()).unwrap();
                let done = value["type"] == stop;
                got.push(Frame::Text(value));
                if done {
                    return got;
                }
            }
            Some(Ok(Message::Close(frame))) => {
                let (code, reason) = frame.map_or((1005, String::new()), |f| {
                    (u16::from(f.code), f.reason.as_str().to_owned())
                });
                got.push(Frame::Close(code, reason));
                return got;
            }
            Some(Ok(_)) => {}
            Some(Err(_)) | None => {
                got.push(Frame::Close(1006, String::new()));
                return got;
            }
        }
    }
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

/// The upstream's usage-limit body, with fields that must never reach the client.
fn limit_body(resets_at: i64) -> String {
    json!({
        "error": {
            "type": "usage_limit_reached",
            "message": "The usage limit has been reached for acct_private_123",
            "plan_type": "plus",
            "resets_at": resets_at,
            "limit_window_minutes": 300,
            "account_id": "acct_private_123",
            "details": {"access_token": "sk-leaked-token"}
        }
    })
    .to_string()
}

/// Upstream rate-limit headers: Codex's advice plus headers that must not be reflected.
fn limit_headers(resets_at: i64) -> Value {
    json!({
        "retry-after": "120",
        "x-codex-active-limit": "codex",
        "x-codex-primary-used-percent": "100.0",
        "x-codex-primary-window-minutes": "300",
        "x-codex-primary-reset-at": resets_at.to_string(),
        "x-codex-secondary-used-percent": "40",
        "x-codex-rate-limit-reached-type": "rate_limit_reached",
        "x-openai-account-id": "acct_private_123",
        "set-cookie": "session=secret",
        "x-request-id": "req_private",
        "x-other-primary-used-percent": "12",
    })
}

fn http_limit(resets_at: i64) -> Value {
    json!({"status": 429, "body": limit_body(resets_at), "headers": limit_headers(resets_at)})
}

fn completed(id: &str) -> Value {
    json!({"events": [
        json!({"type": "response.created", "response": {"id": id, "status": "in_progress", "output": []}}).to_string(),
        json!({"type": "response.completed", "response": {"id": id, "status": "completed", "output": []}}).to_string(),
    ]})
}

/// The wrapped error, as current Codex parses it: openai/codex e0aa043
/// `codex-api/src/endpoint/responses_websocket.rs` `WrappedWebsocketErrorEvent` (`type`,
/// `status`, `error`, `headers`) and `codex-api/src/api_bridge.rs` `UsageErrorResponse`
/// (`error.type`, `plan_type`, `resets_at`, `limit_window_minutes`).
fn assert_codex_usage_limit(event: &Value, plan: Option<&str>, resets_at: Option<i64>) {
    assert_eq!(event["type"], "error", "{event}");
    assert_eq!(event["status"], 429, "{event}");
    assert_eq!(event["error"]["type"], "usage_limit_reached", "{event}");
    assert_eq!(event["error"]["plan_type"].as_str(), plan, "{event}");
    match resets_at {
        Some(at) => assert_eq!(event["error"]["resets_at"].as_i64(), Some(at), "{event}"),
        None => assert!(
            event["error"]["resets_at"].as_i64().is_some_and(|at| at > now()),
            "{event}"
        ),
    }
    let text = event.to_string();
    for secret in [
        "acct_private_123",
        "sk-leaked-token",
        "session=secret",
        "req_private",
        "sk-fake",
    ] {
        assert!(!text.contains(secret), "{secret} reached the client: {text}");
    }
}

fn error_frames(frames: &[Frame]) -> Vec<&Value> {
    frames
        .iter()
        .filter_map(|f| match f {
            Frame::Text(v) if v["type"] == "error" => Some(v),
            _ => None,
        })
        .collect()
}

fn closed(frames: &[Frame]) -> bool {
    matches!(frames.last(), Some(Frame::Close(..)))
}

#[tokio::test]
async fn one_exhausted_account_fails_over_without_an_error() {
    let _registry = REGISTRY.lock().await;
    let proxy = proxy(&[("sk-fake-a", false), ("sk-fake-b", false)]).await;
    // Whichever key the scheduler picks first is exhausted; the other one serves.
    proxy
        .mock
        .shared
        .lock()
        .unwrap()
        .extend([http_limit(now() + 3600), completed("resp_1")]);
    let mut socket = connect(&proxy, true).await;
    socket.send(Message::text(CREATE)).await.unwrap();
    let frames = read(&mut socket, "response.completed").await;
    assert!(error_frames(&frames).is_empty() && !closed(&frames), "{frames:?}");
    let a = proxy.mock.seen("sk-fake-a", "http").len();
    let b = proxy.mock.seen("sk-fake-b", "http").len();
    assert_eq!((a, b), (1, 1), "one failed attempt, then the other key");
}

#[tokio::test]
async fn all_accounts_exhausted_deliver_the_structured_limit() {
    let _registry = REGISTRY.lock().await;
    let proxy = proxy(&[("sk-fake-a", false), ("sk-fake-b", false)]).await;
    let resets_at = now() + 3600;
    for key in ["sk-fake-a", "sk-fake-b"] {
        proxy.mock.push(key, http_limit(resets_at));
    }
    let mut socket = connect(&proxy, true).await;
    socket.send(Message::text(CREATE)).await.unwrap();
    let frames = read(&mut socket, "").await;
    let errors = error_frames(&frames);
    assert_eq!(errors.len(), 1, "{frames:?}");
    let event = errors[0];
    assert_codex_usage_limit(event, Some("plus"), Some(resets_at));
    assert_eq!(event["error"]["limit_window_minutes"], 300);
    // Only Codex's rate-limit and retry headers, never arbitrary upstream headers.
    let headers = event["headers"].as_object().unwrap();
    let mut names: Vec<&str> = headers.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "retry-after",
            "x-codex-active-limit",
            "x-codex-primary-reset-at",
            "x-codex-primary-used-percent",
            "x-codex-primary-window-minutes",
            "x-codex-rate-limit-reached-type",
            "x-codex-secondary-used-percent",
        ]
    );
    assert_eq!(headers["retry-after"], "120");
    assert_eq!(headers["x-codex-primary-used-percent"], "100.0");
    assert!(closed(&frames), "{frames:?}");
    assert_eq!(proxy.mock.seen("sk-fake-a", "http").len(), 1);
    assert_eq!(proxy.mock.seen("sk-fake-b", "http").len(), 1);

    // The next connection finds every key cooling: no upstream call, and the limit with
    // the scheduler's earliest retry instead of a silent close.
    let mut socket = connect(&proxy, true).await;
    socket.send(Message::text(CREATE)).await.unwrap();
    let frames = read(&mut socket, "").await;
    let errors = error_frames(&frames);
    assert_eq!(errors.len(), 1, "{frames:?}");
    let event = errors[0];
    assert_codex_usage_limit(event, None, None);
    let at = event["error"]["resets_at"].as_i64().unwrap();
    assert!((resets_at - 5..=resets_at + 5).contains(&at), "{event}");
    let retry: i64 = event["headers"]["retry-after"].as_str().unwrap().parse().unwrap();
    assert!((3590..=3605).contains(&retry), "{event}");
    assert!(closed(&frames), "{frames:?}");
    assert_eq!(proxy.mock.seen("sk-fake-a", "http").len(), 1);
    assert_eq!(proxy.mock.seen("sk-fake-b", "http").len(), 1);
}

#[tokio::test]
async fn other_clients_keep_the_silent_close() {
    let _registry = REGISTRY.lock().await;
    let proxy = proxy(&[("sk-fake-a", false)]).await;
    proxy.mock.push("sk-fake-a", http_limit(now() + 3600));
    let mut socket = connect(&proxy, false).await;
    socket.send(Message::text(CREATE)).await.unwrap();
    let frames = read(&mut socket, "").await;
    assert!(error_frames(&frames).is_empty(), "{frames:?}");
    assert!(closed(&frames), "{frames:?}");
}

#[tokio::test]
async fn other_rate_limits_keep_the_silent_close() {
    let _registry = REGISTRY.lock().await;
    let proxy = proxy(&[("sk-fake-a", false)]).await;
    let body = json!({"error": {"type": "rate_limit_error", "code": "rate_limit_exceeded", "message": "slow down"}});
    proxy.mock.push(
        "sk-fake-a",
        json!({"status": 429, "body": body.to_string(), "headers": {"retry-after": "3"}}),
    );
    let mut socket = connect(&proxy, true).await;
    socket.send(Message::text(CREATE)).await.unwrap();
    let frames = read(&mut socket, "").await;
    assert!(error_frames(&frames).is_empty(), "{frames:?}");
    assert!(closed(&frames), "{frames:?}");
}

/// The upstream socket's first frame rejects the turn (the ChatGPT backend's wrapped
/// error event with its own headers).
#[tokio::test]
async fn first_frame_rejection_delivers_the_structured_limit() {
    let _registry = REGISTRY.lock().await;
    let proxy = proxy(&[("sk-fake-ws", true)]).await;
    let resets_at = now() + 1800;
    let event = json!({
        "type": "error",
        "status": 429,
        "error": serde_json::from_str::<Value>(&limit_body(resets_at)).unwrap()["error"],
        "headers": limit_headers(resets_at),
    });
    proxy.mock.push("sk-fake-ws", json!({"events": [event.to_string()]}));
    let mut socket = connect(&proxy, true).await;
    socket.send(Message::text(CREATE)).await.unwrap();
    let frames = read(&mut socket, "").await;
    let errors = error_frames(&frames);
    assert_eq!(errors.len(), 1, "{frames:?}");
    assert_codex_usage_limit(errors[0], Some("plus"), Some(resets_at));
    assert_eq!(errors[0]["headers"]["retry-after"], "120");
    assert!(errors[0]["headers"].get("x-openai-account-id").is_none());
    assert!(closed(&frames), "{frames:?}");
}

/// `resets_in_seconds` alone still tells Codex when the limit resets.
#[tokio::test]
async fn relative_reset_becomes_an_absolute_reset() {
    let _registry = REGISTRY.lock().await;
    let proxy = proxy(&[("sk-fake-ws", true)]).await;
    let event = r#"{"type":"error","status":429,"error":{"type":"usage_limit_reached","message":"limit","resets_in_seconds":600}}"#;
    proxy.mock.push("sk-fake-ws", json!({"events": [event]}));
    let mut socket = connect(&proxy, true).await;
    let before = now();
    socket.send(Message::text(CREATE)).await.unwrap();
    let frames = read(&mut socket, "").await;
    let errors = error_frames(&frames);
    assert_eq!(errors.len(), 1, "{frames:?}");
    assert_codex_usage_limit(errors[0], None, None);
    let at = errors[0]["error"]["resets_at"].as_i64().unwrap();
    assert!((before + 595..=now() + 605).contains(&at), "{}", errors[0]);
    assert!(errors[0].get("headers").is_none(), "{}", errors[0]);
}

/// A pinned continuation reaches its account's limit: the continuation never moves to
/// another account. With another account still eligible the client replays (1012); with
/// none it gets the limit.
#[tokio::test]
async fn pinned_continuation_at_its_limit() {
    let _registry = REGISTRY.lock().await;
    let resets_at = now() + 900;
    let limit_event = json!({
        "type": "error",
        "status": 429,
        "error": serde_json::from_str::<Value>(&limit_body(resets_at)).unwrap()["error"],
    })
    .to_string();
    let continuation = |id: &str| {
        json!({"type": "response.create", "model": MODEL, "previous_response_id": id, "input": []}).to_string()
    };

    // The only account: the limit reaches the client.
    let proxy1 = proxy(&[("sk-fake-ws", true)]).await;
    proxy1.mock.push("sk-fake-ws", completed("resp_1"));
    proxy1.mock.push("sk-fake-ws", json!({"events": [limit_event.clone()]}));
    let mut socket = connect(&proxy1, true).await;
    socket.send(Message::text(CREATE)).await.unwrap();
    read(&mut socket, "response.completed").await;
    socket.send(Message::text(continuation("resp_1"))).await.unwrap();
    let frames = read(&mut socket, "").await;
    let errors = error_frames(&frames);
    assert_eq!(errors.len(), 1, "{frames:?}");
    assert_codex_usage_limit(errors[0], Some("plus"), Some(resets_at));
    assert!(closed(&frames) && !frames.contains(&Frame::Close(1012, "upstream requires HTTP replay".into())));
    drop(socket);
    drop(proxy1);

    // Another account is still eligible: replay (1012) and no frame on that account.
    let proxy2 = proxy(&[("sk-fake-a", true), ("sk-fake-b", true)]).await;
    for key in ["sk-fake-a", "sk-fake-b"] {
        proxy2.mock.push(key, completed("resp_1"));
        proxy2.mock.push(key, json!({"events": [limit_event.clone()]}));
    }
    let mut socket = connect(&proxy2, true).await;
    socket.send(Message::text(CREATE)).await.unwrap();
    read(&mut socket, "response.completed").await;
    socket.send(Message::text(continuation("resp_1"))).await.unwrap();
    let frames = read(&mut socket, "").await;
    assert_eq!(
        frames,
        [Frame::Close(1012, "upstream requires HTTP replay".into())],
        "{frames:?}"
    );
    let frames_a = proxy2.mock.seen("sk-fake-a", "ws_frame").len();
    let frames_b = proxy2.mock.seen("sk-fake-b", "ws_frame").len();
    assert_eq!(
        (frames_a.min(frames_b), frames_a.max(frames_b)),
        (0, 2),
        "the continuation stayed on its account"
    );
}

/// The limit arrives after output streamed: the client keeps the output and then gets
/// the limit, not a bare disconnect.
#[tokio::test]
async fn limit_after_streamed_output() {
    limit_after_output(false).await;
}

#[tokio::test]
async fn limit_after_streamed_output_on_the_upstream_socket() {
    limit_after_output(true).await;
}

async fn limit_after_output(websocket: bool) {
    let _registry = REGISTRY.lock().await;
    let proxy = proxy(&[("sk-fake-a", websocket)]).await;
    let resets_at = now() + 7200;
    let failed = json!({
        "type": "response.failed",
        "response": {
            "id": "resp_1",
            "status": "failed",
            "error": serde_json::from_str::<Value>(&limit_body(resets_at)).unwrap()["error"],
        }
    });
    proxy.mock.push(
        "sk-fake-a",
        json!({"events": [
            json!({"type": "response.created", "response": {"id": "resp_1", "status": "in_progress", "output": []}}).to_string(),
            json!({"type": "response.output_text.delta", "delta": "partial"}).to_string(),
            failed.to_string(),
        ]}),
    );
    let mut socket = connect(&proxy, true).await;
    socket.send(Message::text(CREATE)).await.unwrap();
    let frames = read(&mut socket, "").await;
    let kinds: Vec<&str> = frames
        .iter()
        .filter_map(|f| match f {
            Frame::Text(v) => v["type"].as_str(),
            Frame::Close(..) => None,
        })
        .collect();
    assert_eq!(
        kinds,
        ["response.created", "response.output_text.delta", "error"],
        "{frames:?}"
    );
    assert_codex_usage_limit(error_frames(&frames)[0], Some("plus"), Some(resets_at));
    assert!(closed(&frames), "{frames:?}");
}

const SECRETS: [&str; 5] = [
    "acct_private_123",
    "sk-leaked-token",
    "session=secret",
    "req_private",
    "sk-fake",
];

/// No frame the client received carries an account identifier or token.
fn assert_no_secrets(frames: &[Frame]) {
    for frame in frames {
        if let Frame::Text(value) = frame {
            let text = value.to_string();
            for secret in SECRETS {
                assert!(!text.contains(secret), "{secret} reached the client: {text}");
            }
        }
    }
}

const STEERING: &str = "codex:\n  response-steering: true\n";

fn created(id: &str) -> String {
    json!({"type": "response.created", "response": {"id": id, "status": "in_progress", "output": []}}).to_string()
}

/// The two account failures a native upstream socket sends: a terminal
/// `response.failed`, and the wrapped status `error` frame.
fn account_failures(resets_at: i64) -> [(&'static str, String); 2] {
    let error = serde_json::from_str::<Value>(&limit_body(resets_at)).unwrap()["error"].clone();
    [
        (
            "terminal",
            json!({"type": "response.failed", "response": {"id": "resp_1", "status": "failed", "error": error}})
                .to_string(),
        ),
        (
            "wrapped",
            json!({"type": "error", "status": 429, "error": error, "headers": limit_headers(resets_at)}).to_string(),
        ),
    ]
}

/// A native turn's account failure after `response.created`, with and without response
/// steering: the frame never reaches the client as sent (it names the account and carries
/// a token). A Codex client gets the allowlisted limit event, any other client a silent
/// close, and the credential still cools: the next connection does not reach it.
#[tokio::test]
async fn native_account_failures_after_start_never_reach_the_client_as_sent() {
    for steering in [false, true] {
        for codex_client in [true, false] {
            for (shape, failure) in account_failures(now() + 7200) {
                let _registry = REGISTRY.lock().await;
                let config = if steering { STEERING } else { "" };
                let proxy = proxy_with(&[("sk-fake-a", true)], config).await;
                let name = format!("steering={steering} codex={codex_client} {shape}");
                proxy.mock.push(
                    "sk-fake-a",
                    json!({"events": [
                        created("resp_1"),
                        json!({"type": "response.output_text.delta", "delta": "partial"}).to_string(),
                        failure,
                    ]}),
                );
                let mut socket = connect(&proxy, codex_client).await;
                socket.send(Message::text(CREATE)).await.unwrap();
                let frames = read(&mut socket, "").await;
                assert_no_secrets(&frames);
                let errors = error_frames(&frames);
                if codex_client {
                    assert_eq!(errors.len(), 1, "{name}: {frames:?}");
                    assert_codex_usage_limit(errors[0], Some("plus"), None);
                } else {
                    assert!(errors.is_empty(), "{name}: {frames:?}");
                }
                assert!(
                    !frames
                        .iter()
                        .any(|f| matches!(f, Frame::Text(v) if v["type"] == "response.failed")),
                    "{name}: {frames:?}"
                );
                assert!(closed(&frames), "{name}: {frames:?}");
                // The scheduler saw the classification: the credential cools.
                let sent = proxy.mock.seen("sk-fake-a", "ws_frame").len();
                let mut socket = connect(&proxy, codex_client).await;
                socket.send(Message::text(CREATE)).await.unwrap();
                let frames = read(&mut socket, "").await;
                assert_no_secrets(&frames);
                assert!(closed(&frames), "{name}: {frames:?}");
                assert_eq!(
                    proxy.mock.seen("sk-fake-a", "ws_frame").len(),
                    sent,
                    "{name}: the cooled credential was not used again"
                );
            }
        }
    }
}

/// With response steering, an account failure that rejects a create the client queued
/// while a response runs ends the stream the same way: no raw frame, the allowlisted
/// limit for a Codex client.
#[tokio::test]
async fn native_account_failure_of_a_pending_create_never_reaches_the_client_as_sent() {
    for (shape, failure) in account_failures(now() + 7200) {
        let _registry = REGISTRY.lock().await;
        let proxy = proxy_with(&[("sk-fake-a", true)], STEERING).await;
        proxy.mock.push("sk-fake-a", json!({"events": [created("resp_1")]}));
        proxy.mock.push("sk-fake-a", json!({"events": [failure]}));
        let mut socket = connect(&proxy, true).await;
        socket.send(Message::text(CREATE)).await.unwrap();
        let started = read(&mut socket, "response.created").await;
        assert_no_secrets(&started);
        socket.send(Message::text(CREATE)).await.unwrap();
        let frames = read(&mut socket, "").await;
        assert_no_secrets(&frames);
        let errors = error_frames(&frames);
        assert_eq!(errors.len(), 1, "{shape}: {frames:?}");
        assert_codex_usage_limit(errors[0], Some("plus"), None);
        assert!(closed(&frames), "{shape}: {frames:?}");
        assert_eq!(proxy.mock.seen("sk-fake-a", "ws_frame").len(), 2, "{shape}");
    }
}
