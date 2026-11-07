//! Connection lifecycle under backpressure. Behavioural parity with Go is covered by
//! `tests/ws_e2e.rs` (Go fixtures); this checks the bound Rust adds where Go closes the
//! socket under a blocked writer.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message as AxMessage, WebSocket as AxSocket, WebSocketUpgrade};
use cpa_core::config::Config;
use cpa_exec::Executors;
use serde_json::Value;

use crate::router;
use crate::websocket_tools::is_retained;

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("127.0.0.1:{}", addr.port())
}

/// Answers the first request with far more than the loopback buffers hold, then closes
/// with 1009.
async fn flood_then_close(mut socket: AxSocket) {
    if socket.recv().await.is_none() {
        return;
    }
    let created = r#"{"type":"response.created","response":{"id":"r1","output":[]}}"#;
    let _ = socket.send(AxMessage::Text(created.into())).await;
    let delta = format!(
        r#"{{"type":"response.output_text.delta","delta":"{}"}}"#,
        "x".repeat(4 * 1024 * 1024)
    );
    for _ in 0..8 {
        let _ = socket.send(AxMessage::Text(delta.clone().into())).await;
    }
    let _ = socket
        .send(AxMessage::Close(Some(CloseFrame {
            code: 1009,
            reason: "too big".into(),
        })))
        .await;
}

/// The client stops reading mid-response and upstream then fails. The close frame cannot
/// be flushed behind the queued data, yet the connection must still end and release its
/// session state (Go closes the socket instead of waiting behind the writer).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_loss_while_the_client_stalls_releases_the_session() {
    let upstream =
        serve(axum::Router::new().fallback(|ws: WebSocketUpgrade| async move { ws.on_upgrade(flood_then_close) }))
            .await;
    let cfg = Config::parse(&format!(
        "codex-api-key:\n  - api-key: sk-FAKE\n    base-url: http://{upstream}\n    websockets: true\n    models:\n      - name: gpt-fixture\n"
    ))
    .unwrap();
    let credentials = cpa_core::config::credentials::from_config(&cfg);
    assert_eq!(
        credentials[0].metadata["models"],
        serde_json::json!([{ "name": "gpt-fixture" }])
    );
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let proxy = serve(router(Arc::new(crate::testing::runtime(cfg, credentials, executors)))).await;

    let key = "stalled-client-session";
    let mut socket = wreq::Client::new()
        .websocket(format!("ws://{proxy}/v1/responses"))
        .header("session_id", key)
        .send()
        .await
        .unwrap()
        .into_websocket()
        .await
        .unwrap();
    let create =
        r#"{"type":"response.create","model":"gpt-fixture","input":[{"type":"message","role":"user","content":"hi"}]}"#;
    socket.send(wreq::ws::message::Message::text(create)).await.unwrap();

    let started = Instant::now();
    let mut seen = false;
    while started.elapsed() < Duration::from_secs(10) {
        let retained = is_retained(key);
        seen |= retained;
        if seen && !retained {
            // The client never read a byte: the server gave up on the flush and cleaned up.
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let state: Value = serde_json::json!({ "connection_seen": seen });
    panic!("connection still holds its session after 10s: {state}");
}

/// Drops with the upstream response body: the proxy cancelled its request.
struct BodyDropped(Arc<std::sync::atomic::AtomicBool>);

impl Drop for BodyDropped {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// With steering configured, Go's downstream reader cancels the request context when the
/// client leaves, so a turn in flight on an HTTP credential stops its upstream request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steering_reader_cancels_the_turn_when_the_client_leaves() {
    use futures_util::StreamExt;
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = dropped.clone();
    let upstream = serve(axum::Router::new().fallback(move || {
        let guard = BodyDropped(flag.clone());
        async move {
            let events = concat!(
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\",\"output\":[]}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"hi\"}\n\n",
            );
            // The response never completes; only a cancelled request releases it.
            let body =
                futures_util::stream::once(async move { Ok::<_, std::io::Error>(axum::body::Bytes::from(events)) })
                    .chain(futures_util::stream::pending())
                    .map(move |chunk| {
                        let _ = &guard;
                        chunk
                    });
            (
                [("content-type", "text/event-stream")],
                axum::body::Body::from_stream(body),
            )
        }
    }))
    .await;
    let cfg = Config::parse(&format!(
        "codex:\n  response-steering: true\ncodex-api-key:\n  - api-key: sk-FAKE\n    base-url: http://{upstream}\n    models:\n      - name: gpt-fixture\n"
    ))
    .unwrap();
    let credentials = cpa_core::config::credentials::from_config(&cfg);
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let proxy = serve(router(Arc::new(crate::testing::runtime(cfg, credentials, executors)))).await;

    let mut socket = wreq::Client::new()
        .websocket(format!("ws://{proxy}/v1/responses"))
        .send()
        .await
        .unwrap()
        .into_websocket()
        .await
        .unwrap();
    let create =
        r#"{"type":"response.create","model":"gpt-fixture","input":[{"type":"message","role":"user","content":"hi"}]}"#;
    socket.send(wreq::ws::message::Message::text(create)).await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .expect("the turn streams")
        .expect("a frame")
        .unwrap();
    let wreq::ws::message::Message::Text(first) = first else {
        panic!("text frame: {first:?}");
    };
    assert!(first.contains("response.created"), "{first}");
    assert!(
        !dropped.load(std::sync::atomic::Ordering::SeqCst),
        "upstream still streaming"
    );
    drop(socket);

    let started = Instant::now();
    while !dropped.load(std::sync::atomic::Ordering::SeqCst) {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the upstream request outlived the client by 5s"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The request log of a Responses WebSocket connection: Go's downstream timeline
/// (`openai_responses_websocket_timeline.go`) with one `websocket.request` part per client
/// message, one `websocket.response` part per frame written and the read error that ended
/// the connection as `websocket.disconnect` (gorilla's close text), plus the upstream
/// capture of each turn under the upgrade request's log.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_log_records_the_websocket_timeline_and_each_turn() {
    use futures_util::StreamExt;
    use wreq::ws::message::{CloseFrame as WsClose, Message as WsMessage};
    const CREATED: &str = r#"{"type":"response.created","response":{"id":"r1","output":[]}}"#;
    const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"r1","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#;
    let upstream = serve(axum::Router::new().fallback(|| async {
        (
            [("content-type", "text/event-stream")],
            format!("data: {CREATED}\n\ndata: {COMPLETED}\n\n"),
        )
    }))
    .await;
    let dir = std::env::temp_dir().join(format!("cpa-ws-timeline-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = Config::parse(&format!(
        "auth-dir: {}\nobservability: {{logs: {{request-log: true}}}}\ncodex-api-key:\n  - api-key: sk-FAKE\n    base-url: http://{upstream}\n    models:\n      - name: gpt-fixture\n",
        dir.join("auths").display()
    ))
    .unwrap();
    let credentials = cpa_core::config::credentials::from_config(&cfg);
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let rt = Arc::new(crate::testing::runtime(cfg, credentials, executors));
    let management = crate::management::Management::with_options(
        rt.clone(),
        dir.join("config.yaml"),
        crate::management::Options {
            log_dir: Some(dir.clone()),
            management_password: Some(String::new()),
            ..Default::default()
        },
    );
    let proxy = serve(crate::request_logging::router(&management, router(rt))).await;

    let mut socket = wreq::Client::new()
        .websocket(format!("ws://{proxy}/v1/responses"))
        .send()
        .await
        .unwrap()
        .into_websocket()
        .await
        .unwrap();
    let create = r#"{"type":"response.create","model":"gpt-fixture","input":[]}"#;
    socket.send(WsMessage::text(create)).await.unwrap();
    let mut sent = Vec::new();
    while let Some(Ok(WsMessage::Text(text))) = tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .unwrap()
    {
        let done = text.as_str().contains("response.completed");
        sent.push(text.as_str().to_owned());
        if done {
            break;
        }
    }
    assert_eq!(sent.len(), 2, "{sent:?}");
    socket
        .send(WsMessage::Close(Some(WsClose {
            code: 1000.into(),
            reason: "bye".into(),
        })))
        .await
        .unwrap();
    drop(socket);

    let log = written_log(&dir).await;
    let _ = std::fs::remove_dir_all(&dir);
    let section = |name: &str| -> String {
        let start = log
            .find(&format!("=== {name}"))
            .unwrap_or_else(|| panic!("no {name} in\n{log}"));
        let rest = &log[start..];
        let body = &rest[rest.find('\n').unwrap() + 1..];
        body[..body.find("\n=== ").unwrap_or(body.len())].to_owned()
    };
    // Timestamps vary; the events, their order and payloads are Go's.
    let timeline: Vec<(String, String)> = section("WEBSOCKET TIMELINE ===")
        .split("\n\n")
        .filter(|part| !part.trim().is_empty())
        .map(|part| {
            let mut lines = part.lines();
            assert!(lines.next().unwrap().starts_with("Timestamp: "), "{part}");
            let event = lines.next().unwrap().trim_start_matches("Event: ").to_owned();
            (event, lines.collect::<Vec<_>>().join("\n"))
        })
        .collect();
    let mut want = vec![("websocket.request".to_owned(), create.to_owned())];
    want.extend(
        sent.iter()
            .map(|frame| ("websocket.response".to_owned(), frame.clone())),
    );
    want.push((
        "websocket.disconnect".into(),
        "websocket: close 1000 (normal): bye".into(),
    ));
    assert_eq!(timeline, want, "\n{log}");
    // The turn's upstream attempt is captured under the upgrade's log.
    let api_request = section("API REQUEST 1 ===");
    assert!(
        api_request.contains(&format!("Upstream URL: http://{upstream}/responses")),
        "{api_request}"
    );
    assert!(
        section("API RESPONSE").contains(&format!("data: {COMPLETED}")),
        "\n{log}"
    );
}

/// gorilla's `ReadMessage` returns the client's close at once. With steering on and the
/// client no longer reading, its close reply cannot be flushed, yet the turn must still
/// be cancelled (Go's reader cancels the request on that error).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_close_cancels_a_stalled_steering_turn() {
    use futures_util::StreamExt;
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = dropped.clone();
    let upstream = serve(axum::Router::new().fallback(move || {
        let guard = BodyDropped(flag.clone());
        async move {
            let created = axum::body::Bytes::from_static(
                b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\",\"output\":[]}}\n\n",
            );
            let delta = axum::body::Bytes::from(format!(
                "data: {{\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"{}\"}}\n\n",
                "x".repeat(1 << 20)
            ));
            // Far more than the socket buffers hold, then a response that never ends.
            let body = futures_util::stream::iter(
                std::iter::once(created)
                    .chain(std::iter::repeat_n(delta, 32))
                    .map(Ok::<_, std::io::Error>),
            )
            .chain(futures_util::stream::pending())
            .map(move |chunk| {
                let _ = &guard;
                chunk
            });
            (
                [("content-type", "text/event-stream")],
                axum::body::Body::from_stream(body),
            )
        }
    }))
    .await;
    let cfg = Config::parse(&format!(
        "codex:\n  response-steering: true\ncodex-api-key:\n  - api-key: sk-FAKE\n    base-url: http://{upstream}\n    models:\n      - name: gpt-fixture\n"
    ))
    .unwrap();
    let credentials = cpa_core::config::credentials::from_config(&cfg);
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let proxy = serve(router(Arc::new(crate::testing::runtime(cfg, credentials, executors)))).await;
    let mut socket = wreq::Client::new()
        .websocket(format!("ws://{proxy}/v1/responses"))
        .send()
        .await
        .unwrap()
        .into_websocket()
        .await
        .unwrap();
    let create =
        r#"{"type":"response.create","model":"gpt-fixture","input":[{"type":"message","role":"user","content":"hi"}]}"#;
    socket.send(wreq::ws::message::Message::text(create)).await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .expect("the turn streams")
        .expect("a frame")
        .unwrap();
    assert!(matches!(first, wreq::ws::message::Message::Text(_)), "{first:?}");
    // The client stops reading; the server's writes stall behind the full buffers.
    tokio::time::sleep(Duration::from_millis(500)).await;
    socket
        .send(wreq::ws::message::Message::Close(Some(wreq::ws::message::CloseFrame {
            code: 1000.into(),
            reason: "bye".into(),
        })))
        .await
        .unwrap();
    let started = Instant::now();
    while !dropped.load(std::sync::atomic::Ordering::SeqCst) {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the turn outlived the client's close by 5s"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    drop(socket);
}

/// Home with a gated request-log forward and a refused pick: the turn fails and the
/// server closes the connection.
struct GatedHome(Arc<tokio::sync::Notify>);

impl crate::remote::RemoteDispatch for GatedHome {
    fn available(&self) -> bool {
        true
    }
    fn dispatch(
        &self,
        _: crate::remote::RemoteRequest,
    ) -> futures_util::future::BoxFuture<'_, Result<crate::remote::RemoteGrant, crate::remote::RemoteError>> {
        Box::pin(async {
            Err(crate::remote::RemoteError::plain(
                cpa_core::exec::ExecError::local(503, cpa_core::exec::FailureScope::Credential, "busy"),
                "busy",
            ))
        })
    }
    fn models(
        &self,
        _: Vec<(String, String)>,
        _: Vec<(String, String)>,
    ) -> futures_util::future::BoxFuture<'_, Result<Vec<u8>, crate::remote::ModelsError>> {
        Box::pin(async { Err(crate::remote::ModelsError::Unavailable) })
    }
    fn request_log(&self, _: Vec<u8>) -> futures_util::future::BoxFuture<'_, Result<(), String>> {
        let gate = self.0.clone();
        Box::pin(async move {
            gate.notified().await;
            Ok(())
        })
    }
}

/// Go closes the socket as soon as the connection ends and writes its request log in a
/// deferred step: a slow log delivery must not hold the client's connection open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_socket_closes_before_the_request_log_is_delivered() {
    use futures_util::StreamExt;
    let dir = std::env::temp_dir().join(format!("cpa-ws-gated-log-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = Config::parse(&format!(
        "auth-dir: {}\nobservability: {{logs: {{request-log: true}}}}\ncodex-api-key:\n  - api-key: sk-FAKE\n    base-url: http://127.0.0.1:1\n    models:\n      - name: gpt-fixture\n",
        dir.join("auths").display()
    ))
    .unwrap();
    let credentials = cpa_core::config::credentials::from_config(&cfg);
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let rt = Arc::new(crate::testing::runtime(cfg, credentials, executors));
    let gate = Arc::new(tokio::sync::Notify::new());
    rt.set_remote_dispatch(Some(Arc::new(GatedHome(gate.clone()))));
    let management = crate::management::Management::with_options(
        rt.clone(),
        dir.join("config.yaml"),
        crate::management::Options {
            log_dir: Some(dir.clone()),
            management_password: Some(String::new()),
            ..Default::default()
        },
    );
    let proxy = serve(crate::request_logging::router(&management, router(rt))).await;
    let mut socket = wreq::Client::new()
        .websocket(format!("ws://{proxy}/v1/responses"))
        .send()
        .await
        .unwrap()
        .into_websocket()
        .await
        .unwrap();
    socket
        .send(wreq::ws::message::Message::text(
            r#"{"type":"response.create","model":"gpt-fixture","input":[]}"#,
        ))
        .await
        .unwrap();
    // Every frame until the connection ends, while the log forward is still held.
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match socket.next().await {
                Some(Ok(wreq::ws::message::Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            }
        }
    })
    .await;
    gate.notify_one();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(closed.is_ok(), "the socket stayed open while the request log was held");
}

/// One Responses WebSocket turn through the request-log layer against an upstream that
/// answers `status` with `body`: the text frames the client saw and the written log.
async fn logged_failure(status: u16, body: &'static str) -> (Vec<String>, String) {
    use futures_util::StreamExt;
    let upstream = serve(axum::Router::new().fallback(move || async move {
        (
            axum::http::StatusCode::from_u16(status).unwrap(),
            [("content-type", "application/json")],
            body,
        )
    }))
    .await;
    let dir = std::env::temp_dir().join(format!("cpa-ws-timeline-error-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = Config::parse(&format!(
        "auth-dir: {}\nobservability: {{logs: {{request-log: true}}}}\ncodex-api-key:\n  - api-key: sk-FAKE\n    base-url: http://{upstream}\n    models:\n      - name: gpt-fixture\n",
        dir.join("auths").display()
    ))
    .unwrap();
    let credentials = cpa_core::config::credentials::from_config(&cfg);
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let rt = Arc::new(crate::testing::runtime(cfg, credentials, executors));
    let management = crate::management::Management::with_options(
        rt.clone(),
        dir.join("config.yaml"),
        crate::management::Options {
            log_dir: Some(dir.clone()),
            management_password: Some(String::new()),
            ..Default::default()
        },
    );
    let proxy = serve(crate::request_logging::router(&management, router(rt))).await;
    let mut socket = wreq::Client::new()
        .websocket(format!("ws://{proxy}/v1/responses"))
        .send()
        .await
        .unwrap()
        .into_websocket()
        .await
        .unwrap();
    socket
        .send(wreq::ws::message::Message::text(
            r#"{"type":"response.create","model":"gpt-fixture","input":[]}"#,
        ))
        .await
        .unwrap();
    let mut frames = Vec::new();
    while let Ok(Some(Ok(message))) = tokio::time::timeout(Duration::from_secs(10), socket.next()).await {
        match message {
            wreq::ws::message::Message::Text(text) => frames.push(text.as_str().to_owned()),
            wreq::ws::message::Message::Close(_) => break,
            _ => {}
        }
    }
    drop(socket);
    let log = written_log(&dir).await;
    let _ = std::fs::remove_dir_all(&dir);
    (frames, log)
}

/// The `=== WEBSOCKET TIMELINE ===` parts as (event, payload), timestamps dropped.
fn timeline_events(log: &str) -> Vec<(String, String)> {
    let start = log
        .find("=== WEBSOCKET TIMELINE ===")
        .unwrap_or_else(|| panic!("no timeline in\n{log}"));
    let rest = &log[start..];
    let body = &rest[rest.find('\n').unwrap() + 1..];
    let body = &body[..body.find("\n=== ").unwrap_or(body.len())];
    body.split("\n\n")
        .filter(|part| !part.trim().is_empty())
        .map(|part| {
            let mut lines = part.lines();
            assert!(lines.next().unwrap().starts_with("Timestamp: "), "{part}");
            let event = lines.next().unwrap().trim_start_matches("Event: ").to_owned();
            (event, lines.collect::<Vec<_>>().join("\n"))
        })
        .collect()
}

/// Go's terminal-error timeline (`writeResponsesWebsocketTerminalError`): an exposed
/// request fault is the `websocket.response` part the client received; a hidden failure
/// (quota) keeps the upstream reason as a `websocket.disconnect` part although the client
/// only sees the close. Either way the connection then ends with gorilla's "close sent".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_log_records_terminal_errors_like_go() {
    const REQUEST: &str = r#"{"type":"response.create","model":"gpt-fixture","input":[]}"#;
    const CLOSE_SENT: &str = "websocket: close sent";

    let (frames, log) = logged_failure(
        400,
        r#"{"error":{"message":"bad input","type":"invalid_request_error","code":"invalid_value"}}"#,
    )
    .await;
    assert_eq!(frames.len(), 1, "the exposed error event: {frames:?}");
    assert_eq!(
        timeline_events(&log),
        [
            ("websocket.request".to_owned(), REQUEST.to_owned()),
            ("websocket.response".to_owned(), frames[0].clone()),
            ("websocket.disconnect".to_owned(), CLOSE_SENT.to_owned()),
        ],
        "\n{log}"
    );

    const QUOTA: &str = r#"{"error":{"type":"usage_limit_reached","message":"quota exhausted"}}"#;
    let (frames, log) = logged_failure(429, QUOTA).await;
    assert!(frames.is_empty(), "a quota failure closes silently: {frames:?}");
    let events = timeline_events(&log);
    assert_eq!(events.len(), 3, "\n{log}");
    assert_eq!(events[0], ("websocket.request".to_owned(), REQUEST.to_owned()));
    assert_eq!(events[1].0, "websocket.disconnect", "\n{log}");
    assert!(
        events[1].1.contains("usage_limit_reached"),
        "the upstream reason: {}",
        events[1].1
    );
    assert_eq!(events[2], ("websocket.disconnect".to_owned(), CLOSE_SENT.to_owned()));
}

/// Go `TestResponsesInterruptDiagnosticsAreSanitized` and the outcome check of
/// `TestResponsesLateInterruptDoesNotPoisonNextCreate`: every `response.interrupt` adds an
/// `interrupt` and an `interrupt.outcome` part with the response ID hashed (first 8 bytes
/// of its SHA-256) and nothing else from the frame, not a request part; the rejection's
/// error event is not a response part.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_diagnostics_are_sanitized() {
    use futures_util::StreamExt;
    use wreq::ws::message::{CloseFrame as WsClose, Message as WsMessage};
    const CREATED: &str = r#"{"type":"response.created","response":{"id":"r-http","output":[]}}"#;
    // Releases the bootstrap buffer, which holds `response.created`. Event lines close
    // each frame, as Codex sends them.
    const ITEM: &str = r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","role":"assistant","id":"msg_1","content":[{"type":"output_text","text":"partial"}]}}"#;
    // An HTTP turn that stays open until the proxy cancels it.
    let upstream = serve(axum::Router::new().fallback(|| async {
        let events = futures_util::stream::once(async {
            Ok::<_, std::io::Error>(format!(
                "event: response.created\ndata: {CREATED}\n\nevent: response.output_item.done\ndata: {ITEM}\n\n"
            ))
        })
        .chain(futures_util::stream::pending());
        (
            [("content-type", "text/event-stream")],
            axum::body::Body::from_stream(events),
        )
    }))
    .await;
    let dir = std::env::temp_dir().join(format!("cpa-ws-interrupt-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = Config::parse(&format!(
        "auth-dir: {}\nobservability: {{logs: {{request-log: true}}}}\ncodex-api-key:\n  - api-key: sk-FAKE\n    base-url: http://{upstream}\n    models:\n      - name: gpt-fixture\n",
        dir.join("auths").display()
    ))
    .unwrap();
    let credentials = cpa_core::config::credentials::from_config(&cfg);
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let rt = Arc::new(crate::testing::runtime(cfg, credentials, executors));
    let management = crate::management::Management::with_options(
        rt.clone(),
        dir.join("config.yaml"),
        crate::management::Options {
            log_dir: Some(dir.clone()),
            management_password: Some(String::new()),
            ..Default::default()
        },
    );
    let proxy = serve(crate::request_logging::router(&management, router(rt))).await;
    let mut socket = wreq::Client::new()
        .websocket(format!("ws://{proxy}/v1/responses"))
        .send()
        .await
        .unwrap()
        .into_websocket()
        .await
        .unwrap();
    let secret = r#"{"type":"response.interrupt","response_id":"private-id-token","mode":"secret-mode","authorization":"Bearer secret-credential","input":"private-conversation"}"#;
    socket.send(WsMessage::text(secret)).await.unwrap();
    let rejected = read_text(&mut socket).await;
    assert_eq!(
        gjson::get(&rejected, "error.message").str(),
        "no active upstream websocket for response.interrupt",
        "{rejected}"
    );
    let create = r#"{"type":"response.create","model":"gpt-fixture","input":[]}"#;
    socket.send(WsMessage::text(create)).await.unwrap();
    let created = read_text(&mut socket).await;
    assert_eq!(gjson::get(&created, "type").str(), "response.created", "{created}");
    assert_eq!(read_text(&mut socket).await, ITEM);
    socket
        .send(WsMessage::text(
            r#"{"type":"response.interrupt","response_id":"r-http"}"#,
        ))
        .await
        .unwrap();
    // The proxy stops the HTTP turn, so it acknowledges the interrupt first, as current
    // Codex expects (docs/DIFFERENCES-FROM-GO.md).
    let accepted = read_text(&mut socket).await;
    assert_eq!(
        accepted,
        r#"{"type":"response.interrupt.accepted","response_id":"r-http"}"#
    );
    let incomplete = read_text(&mut socket).await;
    assert_eq!(
        gjson::get(&incomplete, "type").str(),
        "response.incomplete",
        "{incomplete}"
    );
    socket
        .send(WsMessage::Close(Some(WsClose {
            code: 1000.into(),
            reason: "bye".into(),
        })))
        .await
        .unwrap();
    drop(socket);

    let log = written_log(&dir).await;
    let _ = std::fs::remove_dir_all(&dir);
    let part = |event: &str, payload: &str| (event.to_owned(), payload.to_owned());
    let hashed = |hash: &str, outcome: &str| {
        let outcome = if outcome.is_empty() {
            String::new()
        } else {
            format!(r#","outcome":"{outcome}""#)
        };
        format!(r#"{{"type":"response.interrupt","response_id_hash":"{hash}"{outcome}}}"#)
    };
    // sha256("private-id-token") and sha256("r-http"), first 8 bytes.
    let (private, http) = ("6e621ae080fb54bf", "31a0066549f97718");
    let mut events = timeline_events(&log);
    assert_eq!(events.len(), 10, "\n{log}");
    // The local outcome and the turn's acknowledgement and incomplete event are written
    // concurrently.
    events[6..9].sort();
    let mut want = vec![
        part("websocket.interrupt", &hashed(private, "")),
        part("websocket.interrupt.outcome", &hashed(private, "rejected")),
        part("websocket.request", create),
        part("websocket.response", &created),
        part("websocket.response", ITEM),
        part("websocket.interrupt", &hashed(http, "")),
        part("websocket.interrupt.outcome", &hashed(http, "local_http")),
        part("websocket.response", &accepted),
        part("websocket.response", &incomplete),
        part("websocket.disconnect", "websocket: close 1000 (normal): bye"),
    ];
    want[6..9].sort();
    assert_eq!(events, want, "\n{log}");
    for secret in [
        "private-id-token",
        "secret-mode",
        "secret-credential",
        "private-conversation",
    ] {
        assert!(!log.contains(secret), "the log leaked {secret}:\n{log}");
    }
}

async fn read_text(socket: &mut wreq::ws::WebSocket) -> String {
    use futures_util::StreamExt;
    match tokio::time::timeout(Duration::from_secs(10), socket.next()).await {
        Ok(Some(Ok(wreq::ws::message::Message::Text(text)))) => text.as_str().to_owned(),
        other => panic!("no text frame: {other:?}"),
    }
}

/// Turns that are not interrupted keep master's upstream bytes, here on the Claude
/// executor: a first turn and a continuation the handler merges into a full transcript.
/// The pinned bodies were recorded on master before the downstream reader handled
/// `response.interrupt` and before `response.incomplete` ended a turn; `max_tokens` is
/// the model's registered output limit since a881c036 and 0d18d3b3 (#75).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uninterrupted_claude_turns_send_masters_bytes() {
    use futures_util::StreamExt;
    use wreq::ws::message::Message as WsMessage;
    const FIRST: &str = r#"{"model":"claude-opus-5-5","max_tokens":128000,"messages":[{"role":"user","content":[{"type":"text","text":"hi","cache_control":{"type":"ephemeral"}}]}],"metadata":{"user_id":"2b35ba29f7cec38e9220a584fafaa8f9eb2a8d01350e9d1c3653ae728471c5bb"},"stream":true,"system":[{"type":"text","text":"be brief","cache_control":{"type":"ephemeral"}}]}"#;
    const SECOND: &str = r#"{"model":"claude-opus-5-5","max_tokens":128000,"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"hello"},{"role":"user","content":[{"type":"text","text":"again","cache_control":{"type":"ephemeral"}}]}],"metadata":{"user_id":"2b35ba29f7cec38e9220a584fafaa8f9eb2a8d01350e9d1c3653ae728471c5bb"},"stream":true,"system":[{"type":"text","text":"be brief","cache_control":{"type":"ephemeral"}}]}"#;
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let upstream = {
        let seen = seen.clone();
        serve(axum::Router::new().fallback(move |body: axum::body::Bytes| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push(String::from_utf8_lossy(&body).into_owned());
                let events = [
                    r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5-5","content":[],"stop_reason":null,"usage":{"input_tokens":3,"output_tokens":0}}}"#,
                    r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                    r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#,
                    r#"{"type":"content_block_stop","index":0}"#,
                    r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
                    r#"{"type":"message_stop"}"#,
                ];
                let sse: String = events
                    .iter()
                    .map(|e| format!("event: {}\ndata: {e}\n\n", gjson::get(e, "type").str()))
                    .collect();
                ([("content-type", "text/event-stream")], sse)
            }
        }))
        .await
    };
    let dir = std::env::temp_dir().join(format!("cpa-ws-claude-{}", uuid::Uuid::new_v4()));
    let cfg = Config::parse(&format!(
        "auth-dir: {}\nclaude-api-key:\n  - api-key: sk-FAKE\n    base-url: http://{upstream}\n    models:\n      - name: claude-opus-5-5\n",
        dir.join("auths").display()
    ))
    .unwrap();
    let credentials = cpa_core::config::credentials::from_config(&cfg);
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new(format!("http://{upstream}")).unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let rt = Arc::new(crate::testing::runtime(cfg, credentials, executors));
    let proxy = serve(router(rt)).await;
    let mut socket = wreq::Client::new()
        .websocket(format!("ws://{proxy}/v1/responses"))
        .send()
        .await
        .unwrap()
        .into_websocket()
        .await
        .unwrap();
    let turns = [
        r#"{"type":"response.create","model":"claude-opus-5-5","instructions":"be brief","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#,
        r#"{"type":"response.create","model":"claude-opus-5-5","previous_response_id":"{{id}}","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"again"}]}]}"#,
    ];
    let mut id = String::new();
    for turn in turns {
        socket.send(WsMessage::text(turn.replace("{{id}}", &id))).await.unwrap();
        loop {
            let frame = match tokio::time::timeout(Duration::from_secs(10), socket.next()).await {
                Ok(Some(Ok(WsMessage::Text(text)))) => text.as_str().to_owned(),
                other => panic!("no text frame: {other:?}"),
            };
            if gjson::get(&frame, "type").str() == "response.completed" {
                id = gjson::get(&frame, "response.id").str().to_owned();
                break;
            }
        }
    }
    drop(socket);
    let _ = std::fs::remove_dir_all(&dir);
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen, [FIRST, SECOND]);
}

/// Grants one Codex WebSocket credential that only this connection's session knows.
struct HomeGrant(cpa_core::credential::Credential);

impl crate::remote::RemoteDispatch for HomeGrant {
    fn available(&self) -> bool {
        true
    }
    fn dispatch(
        &self,
        _: crate::remote::RemoteRequest,
    ) -> futures_util::future::BoxFuture<'_, Result<crate::remote::RemoteGrant, crate::remote::RemoteError>> {
        let credential = self.0.clone();
        Box::pin(async move {
            Ok(crate::remote::RemoteGrant {
                credential,
                end: Box::new(|| None),
                cancel: None,
                request_retry: None,
                user_api_key: String::new(),
                private: Default::default(),
            })
        })
    }
    fn models(
        &self,
        _: Vec<(String, String)>,
        _: Vec<(String, String)>,
    ) -> futures_util::future::BoxFuture<'_, Result<Vec<u8>, crate::remote::ModelsError>> {
        Box::pin(async { Err(crate::remote::ModelsError::Unavailable) })
    }
}

/// Go `TestResponsesInterruptUsesHomeSessionAuth`: a Home credential lives on the
/// execution session, not in the store, and still passes the interrupt's account check.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_accepts_the_sessions_home_credential() {
    use wreq::ws::message::Message as WsMessage;
    const INTERRUPT: &str =
        r#"{"type":"response.interrupt","response_id":"r1","mode":"discard_partial_items","extension":"keep"}"#;
    let frames: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let upstream = {
        let frames = frames.clone();
        serve(axum::Router::new().fallback(move |ws: WebSocketUpgrade| {
            let frames = frames.clone();
            async move {
                ws.on_upgrade(move |mut socket: AxSocket| async move {
                    while let Some(Ok(AxMessage::Text(text))) = socket.recv().await {
                        frames.lock().unwrap().push(text.as_str().to_owned());
                        // The create starts r1; only the interrupt finishes it.
                        let reply = if gjson::get(text.as_str(), "type").str() == "response.interrupt" {
                            r#"{"type":"response.completed","response":{"id":"r1","status":"completed","output":[]}}"#
                        } else {
                            r#"{"type":"response.created","response":{"id":"r1","output":[]}}"#
                        };
                        if socket.send(AxMessage::Text(reply.into())).await.is_err() {
                            return;
                        }
                    }
                })
            }
        }))
        .await
    };
    let dir = std::env::temp_dir().join(format!("cpa-ws-home-interrupt-{}", uuid::Uuid::new_v4()));
    let cfg = Config::parse(&format!(
        "auth-dir: {}\ncodex-api-key:\n  - api-key: sk-FAKE\n    base-url: http://127.0.0.1:1\n    models:\n      - name: gpt-fixture\n",
        dir.join("auths").display()
    ))
    .unwrap();
    let credentials = cpa_core::config::credentials::from_config(&cfg);
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let rt = Arc::new(crate::testing::runtime(cfg, credentials, executors));
    let mut home = cpa_core::credential::Credential::from_file(
        std::path::Path::new("/fake"),
        std::path::Path::new("/fake/home-interrupt-auth.json"),
        serde_json::json!({"type": "codex"}).as_object().unwrap().clone(),
    )
    .unwrap();
    home.id = "home-interrupt-auth".into();
    for (k, v) in [
        ("api_key", "sk-FAKE-HOME"),
        ("base_url", &format!("http://{upstream}")),
        ("websockets", "true"),
    ] {
        home.attributes.insert(k.into(), v.into());
    }
    rt.set_remote_dispatch(Some(Arc::new(HomeGrant(home))));
    let proxy = serve(router(rt.clone())).await;
    let mut socket = wreq::Client::new()
        .websocket(format!("ws://{proxy}/v1/responses"))
        .send()
        .await
        .unwrap()
        .into_websocket()
        .await
        .unwrap();
    socket
        .send(WsMessage::text(
            r#"{"type":"response.create","model":"gpt-fixture","input":[]}"#,
        ))
        .await
        .unwrap();
    let created = read_text(&mut socket).await;
    assert_eq!(gjson::get(&created, "type").str(), "response.created", "{created}");
    assert!(
        rt.store().get("home-interrupt-auth").is_none(),
        "the Home credential stays out of the store"
    );
    socket.send(WsMessage::text(INTERRUPT)).await.unwrap();
    let completed = read_text(&mut socket).await;
    assert_eq!(
        gjson::get(&completed, "type").str(),
        "response.completed",
        "{completed}"
    );
    let frames = frames.lock().unwrap().clone();
    assert_eq!(frames.len(), 2, "{frames:?}");
    assert_eq!(frames[1], INTERRUPT, "the interrupt reaches upstream unchanged");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The request log once it is complete: the writer streams into the `.log` file, so the
/// file is read until its content stops changing.
async fn written_log(dir: &std::path::Path) -> String {
    let started = Instant::now();
    let mut last: Option<String> = None;
    loop {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "no complete request log written"
        );
        let found = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|e| e == "log"));
        let current = found.map(|path| std::fs::read_to_string(path).unwrap());
        match (&last, &current) {
            (Some(previous), Some(now)) if previous == now && !now.is_empty() => return now.clone(),
            _ => last = current,
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A continuation pinned to its credential whose second turn the upstream fails with
/// `failure` (after `shown`, events the client sees first). The client frames of that
/// turn, the close as `close <code> <reason>` last.
async fn pinned_continuation_failure(shown: &'static [&'static str], failure: &'static str) -> Vec<String> {
    use futures_util::StreamExt;
    use wreq::ws::message::Message as WsMessage;
    const FIRST: [&str; 2] = [
        r#"{"type":"response.created","response":{"id":"r1","status":"in_progress","output":[]}}"#,
        r#"{"type":"response.completed","response":{"id":"r1","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#,
    ];
    let upstream = serve(axum::Router::new().fallback(move |ws: WebSocketUpgrade| async move {
        ws.on_upgrade(move |mut socket: AxSocket| async move {
            let mut turn = 0;
            while let Some(Ok(message)) = socket.recv().await {
                if !matches!(message, AxMessage::Text(_)) {
                    continue;
                }
                turn += 1;
                let events: Vec<&str> = if turn == 1 {
                    FIRST.to_vec()
                } else {
                    shown.iter().copied().chain([failure]).collect()
                };
                for event in events {
                    let _ = socket.send(AxMessage::Text(event.into())).await;
                }
            }
        })
    }))
    .await;
    let dir = std::env::temp_dir().join(format!("cpa-ws-pinned-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = Config::parse(&format!(
        "auth-dir: {}\ncodex-api-key:\n  - api-key: sk-FAKE\n    base-url: http://{upstream}\n    websockets: true\n    models:\n      - name: gpt-fixture\n",
        dir.display()
    ))
    .unwrap();
    let credentials = cpa_core::config::credentials::from_config(&cfg);
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let proxy = serve(router(Arc::new(crate::testing::runtime(cfg, credentials, executors)))).await;
    let mut socket = wreq::Client::new()
        .websocket(format!("ws://{proxy}/v1/responses"))
        .send()
        .await
        .unwrap()
        .into_websocket()
        .await
        .unwrap();
    let read = async |socket: &mut wreq::ws::WebSocket| {
        let mut frames = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(10), socket.next())
                .await
                .unwrap()
            {
                Some(Ok(WsMessage::Text(text))) => {
                    let done = text.as_str().contains("response.completed");
                    frames.push(text.as_str().to_owned());
                    if done {
                        break;
                    }
                }
                Some(Ok(WsMessage::Close(frame))) => {
                    let (code, reason) = frame.map_or((1005, String::new()), |f| {
                        (u16::from(f.code), f.reason.as_str().to_owned())
                    });
                    frames.push(format!("close {code} {reason}"));
                    break;
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => {
                    frames.push("close 1006".into());
                    break;
                }
            }
        }
        frames
    };
    let input = r#""input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]"#;
    socket
        .send(WsMessage::text(format!(
            r#"{{"type":"response.create","model":"gpt-fixture",{input}}}"#
        )))
        .await
        .unwrap();
    let first = read(&mut socket).await;
    assert_eq!(first.len(), 2, "{first:?}");
    socket
        .send(WsMessage::text(format!(
            r#"{{"type":"response.create","model":"gpt-fixture","previous_response_id":"r1",{input}}}"#
        )))
        .await
        .unwrap();
    let frames = read(&mut socket).await;
    let _ = std::fs::remove_dir_all(&dir);
    frames
}

/// `replayPinnedAuthFailure`: a pinned continuation's 401 before anything was shown also
/// lost the session's socket, yet the client is told to replay over HTTP (1012).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pinned_continuation_bootstrap_auth_failure_asks_for_replay() {
    let frames = pinned_continuation_failure(
        &[],
        r#"{"type":"error","status":401,"error":{"type":"invalid_request_error","message":"bad token"}}"#,
    )
    .await;
    assert_eq!(frames, ["close 1012 upstream requires HTTP replay"]);
}

/// The same after the response started: the shown event, then the replay close.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pinned_continuation_later_quota_failure_asks_for_replay() {
    const DELTA: &str = r#"{"type":"response.output_text.delta","output_index":0,"delta":"hello"}"#;
    let frames = pinned_continuation_failure(
        &[DELTA],
        r#"{"type":"error","status":429,"error":{"type":"usage_limit_reached","message":"limit","resets_in_seconds":60}}"#,
    )
    .await;
    assert_eq!(frames, [DELTA, "close 1012 upstream requires HTTP replay"]);
}

/// Go 5dbd92a0 (TestCodexWebsocketsExecutor_UnbufferedUpstreamErrorIsDeliveredBeforeDisconnectNotify
/// and the issue 6503 conductor tests): a usage-limit error after the response started
/// loses the session's upstream socket, which closes the client, yet the error's
/// cooldown is still recorded. Here the error leaves the turn's stream in the same poll
/// that reports the lost socket, so nothing can cancel the turn in between.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_upstream_error_that_loses_the_socket_still_cools_the_credential() {
    use futures_util::StreamExt;
    use wreq::ws::message::Message as WsMessage;
    const CREATED: &str = r#"{"type":"response.created","response":{"id":"r1","status":"in_progress","output":[]}}"#;
    const DELTA: &str = r#"{"type":"response.output_text.delta","output_index":0,"delta":"hello"}"#;
    for failure in [
        r#"{"type":"error","status":429,"error":{"type":"usage_limit_reached","message":"The usage limit has been reached","resets_in_seconds":600}}"#,
        r#"{"type":"response.failed","response":{"error":{"type":"usage_limit_reached","status_code":429,"message":"The usage limit has been reached","resets_in_seconds":600}}}"#,
    ] {
        let upstream = serve(axum::Router::new().fallback(move |ws: WebSocketUpgrade| async move {
            ws.on_upgrade(move |mut socket: AxSocket| async move {
                while let Some(Ok(message)) = socket.recv().await {
                    if !matches!(message, AxMessage::Text(_)) {
                        continue;
                    }
                    for event in [CREATED, DELTA, failure] {
                        let _ = socket.send(AxMessage::Text(event.into())).await;
                    }
                }
            })
        }))
        .await;
        let dir = std::env::temp_dir().join(format!("cpa-ws-6503-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = Config::parse(&format!(
            "auth-dir: {}\ncodex-api-key:\n  - api-key: sk-FAKE\n    base-url: http://{upstream}\n    websockets: true\n    models:\n      - name: gpt-fixture\n",
            dir.display()
        ))
        .unwrap();
        let credentials = cpa_core::config::credentials::from_config(&cfg);
        let executors = Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        };
        let rt = Arc::new(crate::testing::runtime(cfg, credentials, executors));
        let proxy = serve(router(rt.clone())).await;
        let mut socket = wreq::Client::new()
            .websocket(format!("ws://{proxy}/v1/responses"))
            .send()
            .await
            .unwrap()
            .into_websocket()
            .await
            .unwrap();
        socket
            .send(WsMessage::text(
                r#"{"type":"response.create","model":"gpt-fixture","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#,
            ))
            .await
            .unwrap();
        let mut frames = Vec::new();
        while let Ok(Some(Ok(frame))) = tokio::time::timeout(Duration::from_secs(10), socket.next()).await {
            match frame {
                WsMessage::Text(text) => frames.push(text.as_str().to_owned()),
                WsMessage::Close(_) => break,
                _ => {}
            }
        }
        // Go's disconnect notifier closes the client ahead of the turn's own error.
        assert_eq!(frames, [CREATED, DELTA], "{failure}");
        let credential = rt
            .store()
            .snapshot()
            .into_iter()
            .find(|c| c.provider == "codex")
            .unwrap();
        assert!(rt.store().blocked(&credential, "gpt-fixture"), "{failure}: not cooling");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Go 1e37b52b: removing a Codex or xAI credential from the store closes the upstream
/// WebSocket its session holds and no other; a sibling credential's connection keeps its
/// socket for the next turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_a_credential_closes_only_its_upstream_sockets() {
    for section in ["codex-api-key", "xai-api-key"] {
        removal_closes_only_its_sockets(section).await;
    }
}

async fn removal_closes_only_its_sockets(section: &str) {
    use futures_util::StreamExt;
    use wreq::ws::message::Message as WsMessage;
    type Log = Arc<std::sync::Mutex<Vec<String>>>;
    let (dials, ended): (Log, Log) = Default::default();
    let upstream = {
        let (dials, ended) = (dials.clone(), ended.clone());
        serve(
            axum::Router::new().fallback(move |headers: axum::http::HeaderMap, ws: WebSocketUpgrade| {
                let (dials, ended) = (dials.clone(), ended.clone());
                async move {
                    let key = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .trim_start_matches("Bearer ")
                        .to_owned();
                    dials.lock().unwrap().push(key.clone());
                    ws.on_upgrade(move |mut socket| async move {
                        while let Some(Ok(message)) = socket.recv().await {
                            if !matches!(message, AxMessage::Text(_)) {
                                continue;
                            }
                            let completed = format!(
                                r#"{{"type":"response.completed","response":{{"id":"resp-{key}","output":[],"usage":{{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}}}}"#
                            );
                            if socket.send(AxMessage::Text(completed.into())).await.is_err() {
                                break;
                            }
                        }
                        ended.lock().unwrap().push(key);
                    })
                }
            }),
        )
        .await
    };
    let dir = std::env::temp_dir().join(format!("cpa-ws-remove-{}", uuid::Uuid::new_v4()));
    let entry = |key: &str| {
        format!(
            "  - api-key: {key}\n    base-url: http://{upstream}\n    websockets: true\n    models:\n      - name: gpt-{key}\n"
        )
    };
    let cfg = Config::parse(&format!(
        "auth-dir: {}\n{section}:\n{}{}",
        dir.join("auths").display(),
        entry("keep"),
        entry("drop")
    ))
    .unwrap();
    let credentials = cpa_core::config::credentials::from_config(&cfg);
    let keep = credentials
        .iter()
        .find(|c| c.attributes.get("api_key").map(String::as_str) == Some("keep"))
        .cloned()
        .expect("the kept key");
    assert_eq!(credentials.len(), 2, "{section}");
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let rt = Arc::new(crate::testing::runtime(cfg, credentials, executors));
    let proxy = serve(router(rt.clone())).await;
    let connect = || async {
        wreq::Client::new()
            .websocket(format!("ws://{proxy}/v1/responses"))
            .send()
            .await
            .unwrap()
            .into_websocket()
            .await
            .unwrap()
    };
    async fn turn(socket: &mut wreq::ws::WebSocket, key: &str, previous: &str) {
        let create = format!(
            r#"{{"type":"response.create","model":"gpt-{key}","previous_response_id":"{previous}","input":[{{"type":"message","role":"user","content":"hi"}}]}}"#
        );
        socket.send(WsMessage::text(create)).await.unwrap();
        loop {
            let frame = match tokio::time::timeout(Duration::from_secs(10), socket.next()).await {
                Ok(Some(Ok(WsMessage::Text(text)))) => text.as_str().to_owned(),
                other => panic!("{key}: no text frame: {other:?}"),
            };
            match gjson::get(&frame, "type").str() {
                "response.completed" => return,
                "error" => panic!("{key}: {frame}"),
                _ => {}
            }
        }
    }
    let (mut kept, mut dropped) = (connect().await, connect().await);
    turn(&mut kept, "keep", "").await;
    turn(&mut dropped, "drop", "").await;
    assert_eq!(*dials.lock().unwrap(), ["keep", "drop"], "{section}");

    rt.store().replace_config(vec![keep]);
    let started = Instant::now();
    while ended.lock().unwrap().is_empty() && started.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        *ended.lock().unwrap(),
        ["drop"],
        "{section}: only the removed key's socket closed"
    );

    turn(&mut kept, "keep", "resp-keep").await;
    assert_eq!(
        *dials.lock().unwrap(),
        ["keep", "drop"],
        "the kept connection reused its socket"
    );
    assert_eq!(*ended.lock().unwrap(), ["drop"]);
    drop((kept, dropped));
    let _ = std::fs::remove_dir_all(&dir);
}

/// zakstak/cliproxy-rs#5: once a Codex client's context-window failure is committed (the
/// upstream sent it and the executor classified it), a `response.interrupt` or a client
/// that leaves while the proxy writes the event does not replace it. The client gets one
/// terminal event and no interrupted `response.incomplete`, and the scheduler records
/// one failure instead of a cancellation. The event write is held at a test gate, so the
/// client acts during it on every machine (no reliance on socket buffer sizes).
mod committed_context_failure {
    use super::*;
    use crate::websocket::gate;
    use futures_util::StreamExt;
    use wreq::ws::message::Message as WsMessage;

    const CODEX_UA: &str = "codex_cli_rs/0.200.0 (Linux; x86_64) xterm";

    /// A private `auth-dir`, removed even when an assertion fails.
    struct AuthDir(std::path::PathBuf);

    impl AuthDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("cpa-ws-ctx-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for AuthDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum During {
        Interrupt,
        Leave,
    }

    #[derive(Clone, Copy, Debug)]
    enum Shape {
        /// The canonical `response.failed`, first on the stream (bootstrap).
        FailedFirst,
        /// The canonical `response.failed` after streamed output.
        FailedAfterOutput,
        /// The live statusless `error`, first on the stream.
        Statusless,
    }

    /// Upstream events and the marker only the client's context-failure event carries.
    fn events(shape: Shape, id: &str) -> (Vec<(&'static str, String)>, String) {
        let marker = format!("too long for {id}");
        let created = format!(
            r#"{{"type":"response.created","sequence_number":0,"response":{{"id":"{id}","object":"response","status":"in_progress","output":[]}}}}"#
        );
        let delta = r#"{"type":"response.output_text.delta","sequence_number":1,"item_id":"msg_1","output_index":0,"content_index":0,"delta":"Partial"}"#.to_owned();
        let failed = format!(
            r#"{{"type":"response.failed","sequence_number":2,"response":{{"id":"{id}","object":"response","status":"failed","error":{{"code":"context_length_exceeded","message":"{marker}"}}}}}}"#
        );
        let statusless = format!(
            r#"{{"type":"error","error":{{"type":"invalid_request_error","code":"context_length_exceeded","message":"{marker}","param":"input"}},"response_id":"{id}","sequence_number":2}}"#
        );
        let events = match shape {
            Shape::FailedFirst => vec![("response.failed", failed)],
            Shape::FailedAfterOutput => vec![
                ("response.created", created),
                ("response.output_text.delta", delta),
                ("response.failed", failed),
            ],
            Shape::Statusless => vec![("error", statusless)],
        };
        (events, marker)
    }

    /// A Codex upstream: over a WebSocket, or over HTTP SSE with (`named`) or without
    /// `event:` lines.
    async fn upstream(events: Vec<(&'static str, String)>, websocket: bool, named: bool) -> String {
        if websocket {
            return serve(axum::Router::new().fallback(move |ws: WebSocketUpgrade| {
                let events = events.clone();
                async move {
                    ws.on_upgrade(move |mut socket: AxSocket| async move {
                        while let Some(Ok(message)) = socket.recv().await {
                            if !matches!(message, AxMessage::Text(_)) {
                                continue;
                            }
                            for (_, event) in &events {
                                let _ = socket.send(AxMessage::Text(event.clone().into())).await;
                            }
                        }
                    })
                }
            }))
            .await;
        }
        let body: String = events
            .iter()
            .map(|(name, data)| {
                if named {
                    format!("event: {name}\ndata: {data}\n\n")
                } else {
                    format!("data: {data}\n\n")
                }
            })
            .collect();
        serve(axum::Router::new().fallback(move || {
            let body = body.clone();
            async move { ([("content-type", "text/event-stream")], body) }
        }))
        .await
    }

    async fn case(name: &str, shape: Shape, websocket: bool, named: bool, during: During) {
        let id = format!("resp_{}", uuid::Uuid::new_v4().simple());
        let (events, marker) = events(shape, &id);
        let upstream = upstream(events, websocket, named).await;
        let dir = AuthDir::new();
        let cfg = Config::parse(&format!(
            "auth-dir: {}\ncodex-api-key:\n  - api-key: sk-FAKE\n    base-url: http://{upstream}\n    websockets: {websocket}\n    models:\n      - name: gpt-fixture\n",
            dir.0.display()
        ))
        .unwrap();
        let credentials = cpa_core::config::credentials::from_config(&cfg);
        let executors = Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        };
        let rt = Arc::new(crate::testing::runtime(cfg, credentials, executors));
        let proxy = serve(router(rt.clone())).await;
        let mut socket = wreq::Client::new()
            .websocket(format!("ws://{proxy}/v1/responses"))
            .header("user-agent", CODEX_UA)
            .send()
            .await
            .unwrap()
            .into_websocket()
            .await
            .unwrap();
        let write = gate::arm("write", &marker);
        let deliver = gate::arm("deliver", &id);
        let _disarm = Disarm(vec![write.clone(), deliver.clone()]);
        socket
            .send(WsMessage::text(
                r#"{"type":"response.create","model":"gpt-fixture","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"huge"}]}]}"#,
            ))
            .await
            .unwrap();
        let reached = tokio::time::timeout(Duration::from_secs(10), write.reached.acquire()).await;
        reached
            .unwrap_or_else(|_| panic!("{name}: the context failure event was never written"))
            .unwrap()
            .forget();
        let stats = || (rt.store().stats().success, rt.store().stats().failure);
        match during {
            During::Interrupt => {
                socket
                    .send(WsMessage::text(format!(
                        r#"{{"type":"response.interrupt","response_id":"{id}"}}"#
                    )))
                    .await
                    .unwrap();
                let delivered = tokio::time::timeout(Duration::from_secs(10), deliver.reached.acquire()).await;
                delivered
                    .unwrap_or_else(|_| panic!("{name}: the interrupt did not reach the turn"))
                    .unwrap()
                    .forget();
                gate::disarm(&write);
                let mut frames = Vec::new();
                while let Ok(Some(Ok(frame))) = tokio::time::timeout(Duration::from_secs(3), socket.next()).await {
                    match frame {
                        WsMessage::Text(text) => frames.push(text.as_str().to_owned()),
                        WsMessage::Close(_) => break,
                        _ => {}
                    }
                }
                let kinds: Vec<String> = frames
                    .iter()
                    .map(|f| gjson::get(f, "type").str().to_owned())
                    .filter(|k| k.starts_with("response.failed") || k == "response.incomplete" || k == "error")
                    .collect();
                assert_eq!(kinds, ["response.failed"], "{name}: one terminal event\n{frames:#?}");
                assert!(frames.last().unwrap().contains(&marker), "{name}: {frames:#?}");
            }
            During::Leave => {
                drop(socket);
                // The turn is dropped with the connection; its failure was committed first.
                let deadline = Instant::now() + Duration::from_secs(5);
                while stats() == (0, 0) && Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                gate::disarm(&write);
            }
        }
        assert_eq!(stats(), (0, 1), "{name}: one failed attempt, not a cancellation");
    }

    /// Disarms its gates when the test ends, passed or not.
    struct Disarm(Vec<Arc<gate::Gate>>);

    impl Drop for Disarm {
        fn drop(&mut self) {
            for gate in &self.0 {
                gate::disarm(gate);
            }
        }
    }

    /// Runs every case, then fails with the names of those that failed.
    async fn every_shape(during: During) {
        let mut failed = Vec::new();
        for (shape, websocket, named) in [
            (Shape::FailedFirst, true, false),
            (Shape::FailedAfterOutput, true, false),
            (Shape::Statusless, true, false),
            (Shape::FailedFirst, false, true),
            (Shape::FailedFirst, false, false),
            (Shape::FailedAfterOutput, false, true),
            (Shape::Statusless, false, true),
        ] {
            let name = format!("{during:?} {shape:?} websocket={websocket} named={named}");
            if tokio::spawn(async move { case(&name, shape, websocket, named, during).await })
                .await
                .is_err()
            {
                failed.push(format!("{shape:?} websocket={websocket} named={named}"));
            }
        }
        assert!(failed.is_empty(), "{during:?}: failed cases {failed:#?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_interrupt_during_the_event_write_adds_no_second_terminal() {
        every_shape(During::Interrupt).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_client_leaving_during_the_event_write_keeps_the_failure() {
        every_shape(During::Leave).await;
    }
}
