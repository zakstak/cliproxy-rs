//! Upstream WebSocket executor tests. Error-frame expectations come from Go
//! (`tests/fixtures/codex_ws_errors.json`, `tests/reference/codex/zz_rsfix_ws_errors_test.go`);
//! the lifecycle tests run the executor against a loopback axum WebSocket upstream with
//! fake credentials. Whole-handler behaviour is covered against Go fixtures in
//! cpa-server's `tests/ws_e2e.rs`.

use super::*;
use std::collections::VecDeque;
use std::path::Path;

use axum::extract::State;
use axum::extract::ws::{Message as AxMessage, WebSocket as AxSocket, WebSocketUpgrade};
use axum::response::{IntoResponse, Response};
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_core::exec::{Caller, Operation};
use cpa_core::format::Format;
use serde_json::Value;

/// Frame observation reads through `cpa_common::rawjson` (the third-party gjson crate is
/// gone): the first of duplicate keys decides, the classified body copies the frame's
/// `error` raw, and quota and frame headers keep the frame's number spelling.
#[test]
fn observers_read_first_duplicates_and_keep_raw_numbers() {
    let frame = r#"{"type":"error","status":429,"status":503,"error":{"type":"rate_limit_error","n":1e400,"big":123456789012345678901234567890},"headers":{"x-codex-primary-used-percent":12.50}}"#;
    let error = ws_error(frame, false).expect("classified");
    assert_eq!(error.status, 429);
    assert_eq!(
        String::from_utf8_lossy(&error.body),
        r#"{"status":429,"error":{"type":"rate_limit_error","n":1e400,"big":123456789012345678901234567890}}"#
    );
    assert_eq!(error.headers.get("x-codex-primary-used-percent").unwrap(), "12.50");
    let ack = r#"{"type":"response.interrupt.accepted","response_id":"r1","response_id":"r2"}"#;
    assert_eq!(acknowledged_id(ack).as_deref(), Some("r1"));
    let done = r#"{"type":"response.completed","response":{"id":"r1"},"response":{"id":"r2"}}"#;
    assert_eq!(terminal_response_id(done).as_deref(), Some("r1"));
    let limits = r#"{"type":"codex.rate_limits","rate_limits":{"allowed":true,"primary":{"used_percent":12.50,"window_minutes":3e2,"reset_after_seconds":60}}}"#;
    let headers = crate::codex_quota::event_headers(limits).expect("quota headers");
    for (name, value) in [
        ("X-Codex-Primary-Used-Percent", "12.50"),
        ("X-Codex-Primary-Window-Minutes", "3e2"),
    ] {
        assert!(
            headers.iter().any(|(k, v)| k == name && v == value),
            "{name}: {headers:?}"
        );
    }
}

#[test]
fn error_frames_match_go() {
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("../tests/fixtures/codex_ws_errors.json")).expect("fixture");
    assert_eq!(cases.len(), 15);
    for case in &cases {
        let payload = case["payload"].as_str().unwrap();
        let cooling = case["model_level_cooling"].as_bool().unwrap_or(false);
        let got = ws_error(payload, cooling);
        if !case["matched"].as_bool().unwrap() {
            assert!(got.is_none(), "{payload}");
            continue;
        }
        let got = got.unwrap_or_else(|| panic!("{payload}: not classified"));
        assert_eq!(u64::from(got.status), case["status"].as_u64().unwrap(), "{payload}");
        assert_eq!(
            String::from_utf8_lossy(&got.body),
            case["message"].as_str().unwrap(),
            "{payload}"
        );
        assert_eq!(
            got.retry_after.map(|d| d.as_millis() as i64),
            case["retry_after_ms"].as_i64(),
            "{payload}"
        );
        let credential_scoped = case["credential_scoped"].as_bool().unwrap_or(false);
        assert_eq!(
            got.scope == FailureScope::Credential && response::is_usage_limit(&String::from_utf8_lossy(&got.body)),
            credential_scoped,
            "{payload}: credential-wide usage limit"
        );
        let mut headers: Vec<(String, String)> = got
            .headers
            .iter()
            .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap().to_owned()))
            .collect();
        headers.sort();
        let mut want: Vec<(String, String)> = case["headers"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.as_str().unwrap().to_owned()))
            .collect();
        want.sort();
        assert_eq!(headers, want, "{payload}");
    }
}

/// What the scripted upstream does with each received frame.
#[derive(Clone)]
enum Act {
    Send(Vec<&'static str>),
    /// Send, then close the socket with this code.
    SendClose(Vec<&'static str>, u16),
    /// Send, then drop the connection without a close frame.
    SendDrop(Vec<&'static str>),
    /// Send one owned text frame (large payloads).
    SendOwned(String),
    /// Ping `count` times, `every` apart, without any application message.
    Pings(Duration, usize),
    /// Send, then close the socket with code 1000 once notified.
    SendCloseOn(Vec<&'static str>, Arc<tokio::sync::Notify>),
    /// Answer with a binary message, which the reader takes as the socket's loss.
    SendBinary,
}

#[derive(Default)]
struct Upstream {
    script: Mutex<VecDeque<Act>>,
    frames: Mutex<Vec<String>>,
    dials: Mutex<Vec<HeaderMap>>,
    /// Handshake rejection (status, body) for the next dial.
    reject: Mutex<Option<(u16, &'static str)>>,
    /// Sockets that ended (closed by either side).
    ended: Mutex<usize>,
    /// Send a binary message right after the handshake, before any request.
    binary_first: Mutex<bool>,
    /// Close the socket right after the handshake, before any request.
    close_first: Mutex<bool>,
}

async fn handler(State(up): State<Arc<Upstream>>, ws: WebSocketUpgrade, headers: axum::http::HeaderMap) -> Response {
    let mut converted = HeaderMap::new();
    for (k, v) in &headers {
        converted.insert(
            http::HeaderName::from_bytes(k.as_str().as_bytes()).unwrap(),
            http::HeaderValue::from_bytes(v.as_bytes()).unwrap(),
        );
    }
    up.dials.lock().unwrap().push(converted);
    if let Some((status, body)) = up.reject.lock().unwrap().take() {
        return (
            axum::http::StatusCode::from_u16(status).unwrap(),
            [
                ("content-type", "application/json"),
                ("x-codex-primary-used-percent", "97"),
            ],
            body,
        )
            .into_response();
    }
    ws.on_upgrade(move |socket| serve_socket(up, socket))
}

async fn serve_socket(up: Arc<Upstream>, mut socket: AxSocket) {
    if *up.binary_first.lock().unwrap() {
        let _ = socket.send(AxMessage::Binary(vec![1u8, 2, 3].into())).await;
    }
    if *up.close_first.lock().unwrap() {
        let _ = socket.send(AxMessage::Close(None)).await;
        *up.ended.lock().unwrap() += 1;
        return;
    }
    while let Some(Ok(message)) = socket.recv().await {
        let AxMessage::Text(text) = message else {
            continue;
        };
        up.frames.lock().unwrap().push(text.as_str().to_owned());
        let act = up.script.lock().unwrap().pop_front();
        let (events, close) = match act {
            Some(Act::Send(events)) => (events, None),
            Some(Act::SendDrop(events)) => {
                for event in events {
                    let _ = socket.send(AxMessage::Text(event.into())).await;
                }
                break;
            }
            Some(Act::SendClose(events, code)) => (events, Some(code)),
            Some(Act::SendCloseOn(events, close)) => {
                for event in events {
                    let _ = socket.send(AxMessage::Text(event.into())).await;
                }
                close.notified().await;
                (Vec::new(), Some(1000))
            }
            Some(Act::SendOwned(text)) => {
                let _ = socket.send(AxMessage::Text(text.into())).await;
                continue;
            }
            Some(Act::SendBinary) => {
                let _ = socket.send(AxMessage::Binary(vec![1u8, 2, 3].into())).await;
                continue;
            }
            Some(Act::Pings(every, count)) => {
                for _ in 0..count {
                    tokio::time::sleep(every).await;
                    if socket.send(AxMessage::Ping(Bytes::new())).await.is_err() {
                        break;
                    }
                }
                continue;
            }
            None => continue,
        };
        for event in events {
            let _ = socket.send(AxMessage::Text(event.into())).await;
        }
        if let Some(code) = close {
            let _ = socket
                .send(AxMessage::Close(Some(axum::extract::ws::CloseFrame {
                    code,
                    reason: "bye".into(),
                })))
                .await;
            break;
        }
    }
    *up.ended.lock().unwrap() += 1;
}

async fn upstream(script: Vec<Act>) -> (Arc<Upstream>, String) {
    let up = Arc::new(Upstream::default());
    up.script.lock().unwrap().extend(script);
    let app = axum::Router::new().fallback(handler).with_state(up.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (up, url)
}

fn credential(base_url: &str) -> Credential {
    let mut credential = Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/codex-ws.json"),
        serde_json::json!({"type": "codex"}).as_object().unwrap().clone(),
    )
    .unwrap();
    for (k, v) in [("api_key", "sk-FAKE"), ("base_url", base_url), ("websockets", "true")] {
        credential.attributes.insert(k.into(), v.into());
    }
    credential
}

/// A turn as the server hands it over: the client's frame, which is a
/// `response.create` (the tag is added when a test body leaves it out).
fn request(body: &str) -> ExecRequest {
    let body = match cpa_common::codex_body::CodexBody::parse(body.as_bytes()) {
        Ok(parsed) if parsed.get("type").is_none() => Bytes::from(parsed.with_member("type", r#""response.create""#)),
        _ => Bytes::from(body.to_owned()),
    };
    ExecRequest {
        operation: Operation::Generate,
        source_format: Format::OpenAIResponse,
        response_format: Format::OpenAIResponse,
        requested_model: "gpt-fixture".into(),
        model: "gpt-fixture".into(),
        original_body: body.clone(),
        body,
        stream: true,
        alt: None,
        session: None,
        execution_session: Some("conn-1".into()),
        derived_session: None,
        resolved_model: None,
        usage: Default::default(),
        request_path: String::new(),
        headers: HeaderMap::new(),
        caller: Caller {
            principal: "client-key-FAKE".into(),
            source: "authorization",
            provider: String::new(),
        },
    }
}

fn session(continuation: bool) -> ExecSession {
    ExecSession {
        id: "conn-1".into(),
        continuation,
        lease: None,
    }
}

async fn collect(response: ExecResponse) -> Vec<Result<String, ExecError>> {
    let ResponseBody::Stream(stream) = response.body else {
        panic!("websocket turns stream");
    };
    stream
        .map(|item| item.map(|b| String::from_utf8_lossy(&b).into_owned()))
        .collect()
        .await
}

const CREATED: &str = r#"{"type":"response.created","response":{"id":"r1","output":[]}}"#;
const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"r1","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#;
const BODY: &str = r#"{"model":"gpt-fixture","input":[{"type":"message","role":"user","content":"hi"}]}"#;

/// Go `TestWebsocketActivationAfterUpstreamDisconnect` (d31b61cb): a turn that activates
/// after the socket's reader stopped, on a close or a binary message, gets the reader's
/// error at once instead of waiting forever. The xAI executor shares this reader.
#[tokio::test]
async fn a_turn_activating_after_the_reader_stopped_gets_its_error() {
    for (name, binary, close) in [("close", false, true), ("binary", true, false)] {
        let (up, url) = upstream(vec![]).await;
        *up.binary_first.lock().unwrap() = binary;
        *up.close_first.lock().unwrap() = close;
        let executor = CodexExecutor::new().unwrap();
        let session = executor.ws.session("conn-1");
        let target = Target::new(
            &credential(&url),
            ws_url(&format!("{url}/responses")).unwrap(),
            &Config::default(),
        );
        let (conn, _) = executor
            .ensure(&session, &target, &HeaderMap::new(), "gpt-fixture", false, None)
            .await
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        tokio::time::timeout(Duration::from_secs(5), async {
            while conn.link.lock().unwrap().lost.is_none() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{name}: the disconnect was not processed"));
        let mut reads = conn.activate(false);
        let read = tokio::time::timeout(Duration::from_secs(1), reads.recv())
            .await
            .unwrap_or_else(|_| panic!("{name}: activation waited"));
        assert!(matches!(read, Some(Read::Failed(_))), "{name}");
    }
}

#[tokio::test]
async fn continuation_without_a_socket_requires_replay() {
    let (up, url) = upstream(vec![]).await;
    let executor = CodexExecutor::new().unwrap();
    let error = executor
        .execute_in_session(&credential(&url), request(BODY), &Config::default(), &session(true))
        .await
        .err()
        .expect("no socket to continue on");
    assert!(error.is_replay_required(), "{error}");
    assert!(up.dials.lock().unwrap().is_empty(), "a continuation never dials");
}

/// Two turns share one dial; the continuation turn reuses it, and closing the session
/// closes the upstream socket and drops the pooled session.
#[tokio::test]
async fn turns_reuse_the_socket_and_close_session_releases_it() {
    let (up, url) = upstream(vec![
        Act::Send(vec![CREATED, COMPLETED]),
        Act::Send(vec![CREATED, COMPLETED]),
    ])
    .await;
    let executor = CodexExecutor::new().unwrap();
    let cfg = Config::default();
    let cred = credential(&url);
    for continuation in [false, true] {
        let response = executor
            .execute_in_session(&cred, request(BODY), &cfg, &session(continuation))
            .await
            .unwrap();
        let events = collect(response).await;
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(events.iter().all(Result::is_ok));
    }
    assert_eq!(up.dials.lock().unwrap().len(), 1);
    assert_eq!(up.frames.lock().unwrap().len(), 2);
    assert_eq!(executor.ws.len(), 1);
    executor.close_session("conn-1");
    assert_eq!(executor.ws.len(), 0);
    for _ in 0..50 {
        if *up.ended.lock().unwrap() == 1 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("upstream socket still open after close_session");
}

/// A Home pick as the pooled socket sees it: records retain/end and keeps the closer.
#[derive(Default)]
struct FakePick {
    retained: Mutex<usize>,
    ended: Mutex<usize>,
    close: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl cpa_core::exec::Retainable for FakePick {
    fn retain(&self, close: Box<dyn FnOnce() + Send>) -> bool {
        *self.retained.lock().unwrap() += 1;
        *self.close.lock().unwrap() = Some(close);
        true
    }

    fn end(&self) {
        *self.ended.lock().unwrap() += 1;
    }
}

fn session_with(continuation: bool, pick: &Arc<FakePick>) -> ExecSession {
    ExecSession {
        lease: Some(cpa_core::exec::SessionLease(pick.clone())),
        ..session(continuation)
    }
}

async fn ended(up: &Upstream, count: usize) {
    for _ in 0..50 {
        if *up.ended.lock().unwrap() == count {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("upstream socket still open");
}

/// Go `bindExecutionLifecycle`: the pooled socket keeps the turn's Home pick past the
/// response; a later turn's pick replaces it (`target_replaced`), and draining that
/// pick closes the socket, which then ends the pick (`connection_closed`).
#[tokio::test]
async fn pooled_socket_keeps_the_home_pick_until_it_closes() {
    let (up, url) = upstream(vec![
        Act::Send(vec![CREATED, COMPLETED]),
        Act::Send(vec![CREATED, COMPLETED]),
    ])
    .await;
    let executor = CodexExecutor::new().unwrap();
    let (cfg, cred) = (Config::default(), credential(&url));
    let first = Arc::new(FakePick::default());
    let response = executor
        .execute_in_session(&cred, request(BODY), &cfg, &session_with(false, &first))
        .await
        .unwrap();
    assert_eq!(collect(response).await.len(), 2);
    assert_eq!(*first.retained.lock().unwrap(), 1);
    assert_eq!(
        *first.ended.lock().unwrap(),
        0,
        "the response ended, the socket did not"
    );

    let second = Arc::new(FakePick::default());
    let response = executor
        .execute_in_session(&cred, request(BODY), &cfg, &session_with(true, &second))
        .await
        .unwrap();
    assert_eq!(collect(response).await.len(), 2);
    assert_eq!(*first.ended.lock().unwrap(), 1, "replaced on the same socket");
    assert_eq!(*second.ended.lock().unwrap(), 0);
    assert_eq!(up.dials.lock().unwrap().len(), 1);

    let close = second.close.lock().unwrap().take().expect("closer bound");
    close();
    ended(&up, 1).await;
    assert_eq!(*second.ended.lock().unwrap(), 1);
    assert_eq!(executor.ws.len(), 1, "the downstream session stays");
    let error = executor
        .execute_in_session(&cred, request(BODY), &cfg, &session(true))
        .await
        .err()
        .expect("the drained socket is gone");
    assert!(error.is_replay_required(), "{error}");
}

/// Closing the downstream session ends the pick its socket kept.
#[tokio::test]
async fn closing_the_session_ends_the_kept_pick() {
    let (up, url) = upstream(vec![Act::Send(vec![CREATED, COMPLETED])]).await;
    let executor = CodexExecutor::new().unwrap();
    let pick = Arc::new(FakePick::default());
    let response = executor
        .execute_in_session(
            &credential(&url),
            request(BODY),
            &Config::default(),
            &session_with(false, &pick),
        )
        .await
        .unwrap();
    collect(response).await;
    executor.close_session("conn-1");
    assert_eq!(*pick.ended.lock().unwrap(), 1);
    ended(&up, 1).await;
}

/// Go 1e37b52b: after a reload that changes only `requests.proxy-url`, the session's
/// open socket keeps serving its credential (continuation and new turn alike), while a
/// new session dials through the new proxy and a credential's own proxy still re-targets.
#[tokio::test]
async fn a_global_proxy_reload_keeps_the_open_socket() {
    let (up, url) = upstream(vec![Act::Send(vec![CREATED, COMPLETED]); 3]).await;
    let executor = CodexExecutor::new().unwrap();
    let cred = credential(&url);
    // Nothing listens on port 1: a dial through this proxy fails.
    let reloaded = Config::parse("requests:\n  proxy-url: http://127.0.0.1:1\n").unwrap();
    for (cfg, continuation) in [(&Config::default(), false), (&reloaded, true), (&reloaded, false)] {
        let response = executor
            .execute_in_session(&cred, request(BODY), cfg, &session(continuation))
            .await
            .unwrap_or_else(|e| panic!("continuation={continuation}: {e:?}"));
        let events = collect(response).await;
        assert!(events.len() == 2 && events.iter().all(Result::is_ok), "{events:?}");
    }
    assert_eq!(up.dials.lock().unwrap().len(), 1, "every turn kept the first socket");

    let mut other = request(BODY);
    other.execution_session = Some("conn-2".into());
    let fresh = ExecSession {
        id: "conn-2".into(),
        ..session(false)
    };
    let error = executor
        .execute_in_session(&cred, other, &reloaded, &fresh)
        .await
        .err()
        .expect("a new socket dials through the reloaded proxy");
    assert_eq!(error.scope, FailureScope::Transport, "{error}");

    let mut own = cred.clone();
    own.attributes.insert("proxy_url".into(), "http://127.0.0.1:1".into());
    let error = executor
        .execute_in_session(&own, request(BODY), &reloaded, &session(true))
        .await
        .err()
        .expect("the credential's own proxy no longer matches the socket");
    assert!(error.is_replay_required(), "{error}");
    assert_eq!(up.dials.lock().unwrap().len(), 1);
}

/// Go `websocketSessionTargetMatches` compares the trimmed proxy settings, not parsed
/// proxies: `direct` and `none`, or two unparseable settings, are different targets. A
/// continuation that changes `(global, own)` after the first turn keeps the socket or
/// needs a replay as Go decides.
#[tokio::test]
async fn proxy_settings_match_as_go_compares_them() {
    type Row = (
        &'static str,
        Option<&'static str>,
        &'static str,
        Option<&'static str>,
        bool,
    );
    let rows: [Row; 6] = [
        ("", None, "http://127.0.0.1:1", None, true),
        ("direct", Some("none"), "http://127.0.0.1:1", None, false),
        ("", Some("direct"), "", Some("none"), false),
        ("", Some("bogus://a"), "", Some("bogus://b"), false),
        ("", Some("direct"), "", Some(" direct "), true),
        ("none", None, "NONE", None, true),
    ];
    for (dial_global, dial_own, global, own, reuse) in rows {
        let row = format!("{dial_global:?} {dial_own:?} -> {global:?} {own:?}");
        let (up, url) = upstream(vec![Act::Send(vec![CREATED, COMPLETED]); 2]).await;
        let executor = CodexExecutor::new().unwrap();
        let turn = |global: &str, own: Option<&str>, continuation: bool| {
            let cfg = Config::parse(&format!("requests:\n  proxy-url: {global:?}\n")).unwrap();
            let mut cred = credential(&url);
            if let Some(own) = own {
                cred.attributes.insert("proxy_url".into(), own.into());
            }
            let executor = &executor;
            async move {
                let response = executor
                    .execute_in_session(&cred, request(BODY), &cfg, &session(continuation))
                    .await?;
                let events = collect(response).await;
                assert!(events.len() == 2 && events.iter().all(Result::is_ok), "{events:?}");
                Ok::<(), ExecError>(())
            }
        };
        turn(dial_global, dial_own, false)
            .await
            .unwrap_or_else(|e| panic!("{row}: first turn {e:?}"));
        match turn(global, own, true).await {
            Ok(()) => assert!(reuse, "{row}: kept the socket"),
            Err(e) => assert!(!reuse && e.is_replay_required(), "{row}: {e:?}"),
        }
        assert_eq!(up.dials.lock().unwrap().len(), 1, "{row}");
    }
}

/// A rejected handshake reports the upstream status with Go's classification and the
/// quota headers are observed.
#[tokio::test]
async fn handshake_rejection_is_classified() {
    let (up, url) = upstream(vec![]).await;
    *up.reject.lock().unwrap() = Some((401, r#"{"error":{"message":"bad key","type":"invalid_request_error"}}"#));
    let executor = CodexExecutor::new().unwrap();
    let error = executor
        .execute_in_session(&credential(&url), request(BODY), &Config::default(), &session(false))
        .await
        .err()
        .expect("401 handshake");
    assert_eq!(error.status, 401);
    assert_eq!(error.scope, FailureScope::Credential, "{error}");
    assert!(String::from_utf8_lossy(&error.body).contains("bad key"));
    assert_eq!(
        error
            .headers
            .get("x-codex-primary-used-percent")
            .map(|v| v.to_str().unwrap()),
        Some("97")
    );
}

/// With bootstrap buffering, an overload rejection before any output fails the attempt
/// over without telling the downstream session its socket was lost.
#[tokio::test]
async fn bootstrap_overload_fails_over_without_disconnect_notice() {
    const FAILED: &str = r#"{"type":"response.failed","response":{"id":"r1","error":{"code":"server_is_overloaded","message":"overloaded"}}}"#;
    let (_up, url) = upstream(vec![Act::Send(vec![CREATED, FAILED])]).await;
    let executor = CodexExecutor::new().unwrap();
    // Legacy top-level spelling: global, so it applies to this API key too.
    let cfg = Config::parse("codex:\n  stream-bootstrap-buffering: true\n").unwrap();
    let error = executor
        .execute_in_session(&credential(&url), request(BODY), &cfg, &session(false))
        .await
        .err()
        .expect("overload fails the attempt");
    assert_ne!(error.scope, FailureScope::Request, "another credential may serve it");
    let mut closed = executor.session_losses("conn-1");
    assert!(
        tokio::time::timeout(Duration::from_millis(200), closed.next())
            .await
            .is_err(),
        "overload must not close the downstream session"
    );
}

/// An upstream close with 1009 reaches the turn as Go's fixed request-scoped 413
/// (`mapCodexWebsocketReadError`), while the session's loss keeps the upstream reason
/// (gorilla's close error, which the downstream handler turns into close 1009).
#[tokio::test]
async fn close_1009_is_a_request_scoped_413_and_notifies() {
    let (_up, url) = upstream(vec![Act::SendClose(vec![CREATED], 1009)]).await;
    let executor = CodexExecutor::new().unwrap();
    let mut closed = executor.session_losses("conn-1");
    let response = executor
        .execute_in_session(&credential(&url), request(BODY), &Config::default(), &session(false))
        .await
        .unwrap();
    let events = collect(response).await;
    let error = events.last().unwrap().as_ref().expect_err("turn ends with the close");
    assert_eq!((error.status, error.scope), (413, FailureScope::Request));
    assert_eq!(
        String::from_utf8_lossy(&error.body),
        r#"{"error":{"message":"upstream websocket message too big","type":"invalid_request_error","code":"message_too_big"}}"#
    );
    let notified = tokio::time::timeout(Duration::from_secs(2), closed.next())
        .await
        .expect("session notified");
    assert!(notified.acquired, "the turn's own socket");
    let notified = notified.error;
    assert_eq!(notified.status, 413);
    assert_eq!(
        crate::rawjson::get(&String::from_utf8_lossy(&notified.body), "error.message").str(),
        "bye"
    );
    assert_eq!(
        executor.session_losses("conn-1").pending().map(|e| e.body),
        Some(notified.body)
    );
}

/// gorilla/websocket reads a peer that vanishes without a close frame as close 1006,
/// and Go keeps that text, which Home dispatch classifies as a lifecycle failure.
#[tokio::test]
async fn a_socket_dropped_without_a_close_frame_reads_as_close_1006() {
    let (_up, url) = upstream(vec![Act::SendDrop(vec![CREATED])]).await;
    let executor = CodexExecutor::new().unwrap();
    let response = executor
        .execute_in_session(&credential(&url), request(BODY), &Config::default(), &session(false))
        .await
        .unwrap();
    let events = tokio::time::timeout(Duration::from_secs(5), collect(response))
        .await
        .expect("the turn ends with the dropped socket");
    let error = events.last().unwrap().as_ref().expect_err("the turn fails");
    // N1 #111 W1: the request was written, so the loss is uncertain: request-scoped,
    // never failed over.
    assert_eq!(error.scope, FailureScope::Request, "{error}");
    assert!(
        String::from_utf8_lossy(&error.body).contains("websocket: close 1006 (abnormal closure): unexpected EOF"),
        "{error}"
    );
}

/// Go keeps one read deadline per application message: pings answered while waiting do
/// not extend it, so a stalled upstream that only pings still times out.
#[tokio::test]
async fn pings_do_not_extend_the_idle_deadline() {
    let (_up, url) = upstream(vec![Act::Pings(Duration::from_millis(100), 30)]).await;
    let mut executor = CodexExecutor::new().unwrap();
    executor.ws = Arc::new(Pool::with_idle(Duration::from_millis(400)));
    let response = executor
        .execute_in_session(&credential(&url), request(BODY), &Config::default(), &session(false))
        .await
        .unwrap();
    let events = tokio::time::timeout(Duration::from_secs(2), collect(response))
        .await
        .expect("idle deadline fires despite pings");
    let error = events
        .last()
        .unwrap()
        .as_ref()
        .expect_err("the turn ends with the timeout");
    // The request was written: an idle timeout is an uncertain loss (N1 #111 W1).
    assert_eq!(error.scope, FailureScope::Request);
    assert!(String::from_utf8_lossy(&error.body).contains("idle timeout"), "{error}");
}

/// A socket whose upstream stopped reading (half-open) fails the write after the write
/// bound instead of holding the turn until the read deadline. Part of the frame may
/// have reached the upstream, so the request is never sent again: no fresh socket, and a
/// request-scoped failure that does not fail over (N1 #111 W1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_write_the_upstream_never_reads_times_out() {
    let dials = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = dials.clone();
    let app = axum::Router::new().fallback(move |ws: WebSocketUpgrade| {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        async move {
            ws.on_upgrade(|socket| async move {
                // Hold the socket open without reading from it.
                tokio::time::sleep(Duration::from_secs(120)).await;
                drop(socket);
            })
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });

    let mut executor = CodexExecutor::new().unwrap();
    executor.ws = Arc::new(Pool::with_write(WriteLimit {
        floor: Duration::from_millis(300),
        rate: usize::MAX,
    }));
    // Far larger than loopback socket buffers, so the write cannot finish.
    let body = format!(
        r#"{{"model":"gpt-fixture","input":[{{"type":"message","role":"user","content":"{}"}}]}}"#,
        "x".repeat(32 << 20)
    );
    // Preparing a 32 MiB body takes seconds in a debug build; without the bound the
    // turn would wait for the upstream to drop the socket (120 s).
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        executor.execute_in_session(&credential(&url), request(&body), &Config::default(), &session(false)),
    )
    .await
    .expect("the write bound ends the turn");
    let Err(error) = result else {
        panic!("a write that never completes fails the turn");
    };
    assert_eq!(error.scope, FailureScope::Request);
    assert_eq!(error.body, "codex websockets executor: write timed out");
    // The request log learns it was the proxy's own bound, and for how large a frame
    // (whole seconds at this rate round up to 1 s, past the 300 ms floor).
    let cause = error
        .diagnostic
        .as_ref()
        .map(|d| d.as_str().to_owned())
        .unwrap_or_default();
    assert!(
        cause.starts_with("local_timeout: writing a ") && cause.ends_with("-byte frame took longer than 1s"),
        "{cause}"
    );
    assert_eq!(
        dials.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a partial write is never retried"
    );
    assert_eq!(
        WRITE_LIMIT.deadline(1 << 20),
        Duration::from_secs(10),
        "small frames get the floor"
    );
    assert_eq!(
        WRITE_LIMIT.deadline(5 << 20),
        Duration::from_secs(40),
        "5 MiB at 128 KiB/s"
    );
    // Partial seconds round up: just under 11 x 128 KiB needs almost 11 s.
    assert_eq!(WRITE_LIMIT.deadline(11 * (128 << 10) - 1), Duration::from_secs(11));
}

/// A timed-out write maps like a failed one: after the upstream closed with 1009 it is
/// the request-scoped 413 (no retry on a fresh socket), else a transport error.
#[test]
fn write_errors_keep_the_message_too_big_413() {
    let too_big = message_too_big();
    for message in [
        "codex websockets executor: write timed out",
        "codex websockets executor: write failed",
    ] {
        let error = write_error(Some(&too_big), message);
        assert_eq!((error.status, error.scope), (413, FailureScope::Request), "{message}");
        let other = transport("codex websockets executor: read idle timeout");
        for lost in [None, Some(&other)] {
            let error = write_error(lost, message);
            assert_eq!(error.scope, FailureScope::Transport, "{message}");
            assert!(String::from_utf8_lossy(&error.body).contains(message), "{message}");
        }
    }
}

/// The turn's failure, whether the executor returns it or the turn's stream ends with it.
async fn turn_failure(turn: impl Future<Output = Result<ExecResponse, ExecError>>) -> Option<ExecError> {
    match turn.await {
        Ok(response) => collect(response).await.into_iter().find_map(Result::err),
        Err(error) => Some(error),
    }
}

/// Waits until `done` holds; panics with `what` if it does not within 5 s.
async fn wait_until(what: &str, done: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}"));
}

/// A reader that fails before the turn activates (a binary message right after the
/// handshake) fails the turn instead of leaving it waiting on a dead socket. The test
/// gate holds each dial until the test has taken that socket's loss, so the reader
/// records it before the turn activates and writes, on the first socket and on the one
/// fresh socket of the retry. Each write then provably sent nothing (N1 #111 W1), so
/// the turn ends with the write's transport failure, which may fail over: two dials and
/// no frame on either socket. This is the order FP2 S2 observed under load; the order in
/// which a write wins is `a_reader_failure_after_the_write_is_never_replayed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reader_failure_before_the_turn_activates_fails_the_turn() {
    let (up, url) = upstream(vec![]).await;
    *up.binary_first.lock().unwrap() = true;
    let executor = CodexExecutor::new().unwrap();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    *executor.ws.dial_gate.lock().unwrap() = Some(gate.clone());
    let mut losses = executor.ws.losses("conn-1");
    let (cred, cfg, sess) = (credential(&url), Config::default(), session(false));
    let turn = turn_failure(executor.execute_in_session(&cred, request(BODY), &cfg, &sess));
    let order = async {
        for socket in ["first", "fresh"] {
            let loss = tokio::time::timeout(Duration::from_secs(5), losses.next())
                .await
                .unwrap_or_else(|_| panic!("the {socket} socket's reader recorded no loss"));
            assert_eq!(loss.error.body.as_ref(), UNEXPECTED_BINARY.as_bytes(), "{socket}");
            // The turn activates and writes only now.
            gate.add_permits(1);
        }
    };
    let (error, ()) = tokio::time::timeout(Duration::from_secs(10), futures_util::future::join(turn, order))
        .await
        .expect("the turn must not hang on a dead socket");
    let error = error.expect("the turn fails");
    assert_eq!((error.status, error.scope), (502, FailureScope::Transport), "{error}");
    assert_eq!(
        String::from_utf8_lossy(&error.body),
        "codex websockets executor: write failed"
    );
    assert!(!format!("{error:?}").contains("sk-FAKE"), "{error:?}");
    assert_eq!(up.dials.lock().unwrap().len(), 2, "one retry on a fresh socket");
    // Once both upstream sockets ended, every frame either one received is recorded.
    wait_until("both upstream sockets end", || *up.ended.lock().unwrap() == 2).await;
    assert!(up.frames.lock().unwrap().is_empty(), "no frame left either socket");
}

/// The other order of [`reader_failure_before_the_turn_activates_fails_the_turn`]: the
/// write wins and the reader fails after it. The upstream answers the frame it received
/// with a binary message, so the reader's loss follows a frame that provably left: the
/// failure is request-scoped and the frame is never sent again, here or on another
/// socket (N1 #111 W1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reader_failure_after_the_write_is_never_replayed() {
    let (up, url) = upstream(vec![Act::SendBinary]).await;
    let executor = CodexExecutor::new().unwrap();
    let (cred, cfg, sess) = (credential(&url), Config::default(), session(false));
    let error = tokio::time::timeout(
        Duration::from_secs(10),
        turn_failure(executor.execute_in_session(&cred, request(BODY), &cfg, &sess)),
    )
    .await
    .expect("the turn must not hang on a dead socket")
    .expect("the turn fails");
    assert_eq!((error.status, error.scope), (502, FailureScope::Request), "{error}");
    assert_eq!(error.body.as_ref(), UNEXPECTED_BINARY.as_bytes(), "{error}");
    assert!(!format!("{error:?}").contains("sk-FAKE"), "{error:?}");
    assert_eq!(up.dials.lock().unwrap().len(), 1, "never sent again on a fresh socket");
    wait_until("the upstream socket ends", || *up.ended.lock().unwrap() == 1).await;
    assert_eq!(up.frames.lock().unwrap().len(), 1, "the frame left exactly once");
}

/// With model-level cooling, a usage-limit handshake rejection cools only the model
/// (`newCodexStatusErrWithCooling`); without it, the whole credential. The setting is
/// shared (`upstream.codex`, Go 52d5507d), so every spelling applies to API keys.
#[tokio::test]
async fn handshake_usage_limit_honours_model_level_cooling() {
    const LIMIT: &str = r#"{"error":{"type":"usage_limit_reached","message":"limit","resets_in_seconds":3600}}"#;
    for (yaml, scope) in [
        ("{}\n", FailureScope::Credential),
        ("codex:\n  model-level-cooling: true\n", FailureScope::Model),
        (
            "oauth:\n  providers:\n    codex:\n      model-level-cooling: true\n",
            FailureScope::Model,
        ),
        (
            "upstream:\n  codex:\n    model-level-cooling: true\n",
            FailureScope::Model,
        ),
    ] {
        let (up, url) = upstream(vec![]).await;
        *up.reject.lock().unwrap() = Some((429, LIMIT));
        let cfg = Config::parse(yaml).unwrap();
        let error = CodexExecutor::new()
            .unwrap()
            .execute_in_session(&credential(&url), request(BODY), &cfg, &session(false))
            .await
            .err()
            .expect("429 handshake");
        assert_eq!((error.status, error.scope), (429, scope), "{yaml}");
        assert_eq!(error.retry_after, Some(Duration::from_secs(3600)));
    }
}

/// Upstream frames above tungstenite's 16 MiB default reach the client (Go sets no read
/// limit; the executor allows 64 MiB).
#[tokio::test]
async fn large_upstream_frames_are_delivered() {
    let delta = format!(
        r#"{{"type":"response.output_text.delta","delta":"{}"}}"#,
        "a".repeat(17 * 1024 * 1024)
    );
    let len = delta.len();
    let (up, url) = upstream(vec![Act::SendOwned(delta)]).await;
    up.script.lock().unwrap().push_back(Act::Send(vec![]));
    let executor = CodexExecutor::new().unwrap();
    let response = executor
        .execute_in_session(&credential(&url), request(BODY), &Config::default(), &session(false))
        .await
        .unwrap();
    let ResponseBody::Stream(mut stream) = response.body else {
        panic!("websocket turns stream");
    };
    let first = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("frame arrives")
        .expect("stream item")
        .expect("not an error");
    assert_eq!(first.len(), len);
}

/// One final server text frame compressed like gorilla's writer (sync flush, the
/// `00 00 ff ff` tail removed).
fn compressed_frame(text: &str) -> Vec<u8> {
    let mut deflate = flate2::Compress::new(flate2::Compression::fast(), false);
    let mut payload = Vec::with_capacity(text.len() + 64);
    deflate
        .compress_vec(text.as_bytes(), &mut payload, flate2::FlushCompress::Sync)
        .unwrap();
    assert!(payload.ends_with(&[0, 0, 0xff, 0xff]));
    payload.truncate(payload.len() - 4);
    let mut frame = vec![0xc1];
    match payload.len() {
        n if n < 126 => frame.push(n as u8),
        n => {
            frame.push(126);
            frame.extend_from_slice(&(n as u16).to_be_bytes());
        }
    }
    frame.extend_from_slice(&payload);
    frame
}

/// permessage-deflate end to end: the dial offers gorilla's extension, an upstream that
/// agrees compresses its events, and the turn yields exactly what the same events give
/// uncompressed.
#[tokio::test]
async fn compressed_upstream_events_read_like_plain_ones() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut tcp, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buf = [0u8; 4096];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = tcp.read(&mut buf).await.unwrap();
            request.extend_from_slice(&buf[..n]);
        }
        let request = String::from_utf8(request).unwrap();
        let header = |name: &str| {
            request.lines().find_map(|line| {
                let (k, v) = line.split_once(':')?;
                k.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
            })
        };
        let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(
            header("sec-websocket-key").unwrap().as_bytes(),
        );
        let response = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\nSec-WebSocket-Extensions: {}\r\n\r\n",
            deflate::OFFER
        );
        tcp.write_all(response.as_bytes()).await.unwrap();
        // The turn's response.create, then the compressed events.
        let _ = tcp.read(&mut buf).await.unwrap();
        for event in [CREATED, COMPLETED] {
            tcp.write_all(&compressed_frame(event)).await.unwrap();
        }
        let _ = tcp.read(&mut buf).await;
        header("sec-websocket-extensions")
    });
    let executor = CodexExecutor::new().unwrap();
    let response = executor
        .execute_in_session(&credential(&url), request(BODY), &Config::default(), &session(false))
        .await
        .unwrap();
    let compressed: Vec<String> = collect(response).await.into_iter().map(Result::unwrap).collect();
    executor.close_session("conn-1");
    assert_eq!(server.await.unwrap().as_deref(), Some(deflate::OFFER));

    let (_up, plain_url) = upstream(vec![Act::Send(vec![CREATED, COMPLETED])]).await;
    let executor = CodexExecutor::new().unwrap();
    let response = executor
        .execute_in_session(
            &credential(&plain_url),
            request(BODY),
            &Config::default(),
            &session(false),
        )
        .await
        .unwrap();
    let plain: Vec<String> = collect(response).await.into_iter().map(Result::unwrap).collect();
    assert_eq!(compressed.len(), 2, "{compressed:?}");
    assert_eq!(compressed, plain);
}

/// A close 1009 that arrives while the turn's channel is full cannot be queued; the turn
/// still ends with Go's 413 from the session's recorded loss, not a generic channel error.
#[tokio::test]
async fn close_1009_on_a_full_turn_channel_keeps_the_413() {
    const DELTA: &str = r#"{"type":"response.output_text.delta","output_index":0,"delta":"x"}"#;
    let mut events = vec![CREATED];
    events.extend(std::iter::repeat_n(DELTA, TURN_BUFFER - 1));
    let (_up, url) = upstream(vec![Act::SendClose(events, 1009)]).await;
    let executor = CodexExecutor::new().unwrap();
    let mut closed = executor.session_losses("conn-1");
    let response = executor
        .execute_in_session(&credential(&url), request(BODY), &Config::default(), &session(false))
        .await
        .unwrap();
    // Nothing reads the turn until the reader has filled the channel and seen the close.
    tokio::time::timeout(Duration::from_secs(5), closed.next())
        .await
        .expect("session notified");
    let events = collect(response).await;
    assert_eq!(events.len(), TURN_BUFFER + 1, "every queued event, then the error");
    let error = events.last().unwrap().as_ref().expect_err("turn ends with the close");
    assert_eq!(error.status, 413);
    assert_eq!(
        crate::rawjson::get(&String::from_utf8_lossy(&error.body), "error.message").str(),
        "upstream websocket message too big"
    );
}

/// Go's duplex writes upstream from its own goroutine: a steer stuck in the network (the
/// upstream stopped reading) must not hold back the upstream's next event.
#[tokio::test]
async fn duplex_delivers_upstream_events_while_a_write_is_blocked() {
    const DELTA: &str = r#"{"type":"response.output_text.delta","output_index":0,"delta":"x"}"#;
    let app = axum::Router::new().fallback(|ws: WebSocketUpgrade| async move {
        ws.on_upgrade(|mut socket| async move {
            // The bootstrap create, then no more reads for the socket's life.
            let _ = socket.recv().await;
            let _ = socket.send(AxMessage::Text(CREATED.into())).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
            let _ = socket.send(AxMessage::Text(DELTA.into())).await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        })
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });

    let executor = CodexExecutor::new().unwrap();
    let (frames, input) = tokio::sync::mpsc::channel(4);
    let input = Arc::new(tokio::sync::Mutex::new(input));
    executor.attach_steering("conn-1", SteeringInput::new(input, |_| true));
    let cfg = Config::parse("codex:\n  response-steering: true\n").unwrap();
    let response = executor
        .execute_in_session(&credential(&url), request(BODY), &cfg, &session(false))
        .await
        .unwrap();
    let ResponseBody::Stream(mut stream) = response.body else {
        panic!("websocket turns stream");
    };
    let created = stream.next().await.unwrap().unwrap();
    assert_eq!(
        crate::rawjson::get(&String::from_utf8_lossy(&created), "type").str(),
        "response.created"
    );
    // Far larger than loopback socket buffers: the write cannot finish while upstream
    // does not read.
    let steer = format!(
        r#"{{"type":"response.steer","previous_response_id":"r1","input":"{}"}}"#,
        "x".repeat(32 << 20)
    );
    frames.send(Bytes::from(steer)).await.unwrap();
    let next = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("the delta arrives while the steer is still being written");
    assert_eq!(String::from_utf8_lossy(&next.unwrap().unwrap()), DELTA);
}

/// A request whose capture goes to `tap`.
fn tapped(body: &str, tap: &Arc<crate::codex_testkit::Wiretap>) -> ExecRequest {
    let mut req = request(body);
    req.usage = cpa_core::exec::UsageSink::default().with_capture(cpa_core::exec::CaptureSink::new(tap.clone()));
    req
}

/// Upstream capture of a WebSocket turn (`codex_websockets_stream.go`): the request
/// frame with the dial's URL and headers before dialing, the handshake after a new dial
/// only, then every upstream message as received.
#[tokio::test]
async fn capture_records_websocket_turns_as_go_does() {
    use crate::codex_testkit::{Tap, Wiretap};
    let (_up, url) = upstream(vec![
        Act::Send(vec![CREATED, COMPLETED]),
        Act::Send(vec![CREATED, COMPLETED]),
    ])
    .await;
    let executor = CodexExecutor::new().unwrap();
    let cred = credential(&url);
    let ws_url = format!("{}/responses", url.replacen("http://", "ws://", 1));
    for continuation in [false, true] {
        let tap = Arc::new(Wiretap::default());
        let response = executor
            .execute_in_session(&cred, tapped(BODY, &tap), &Config::default(), &session(continuation))
            .await
            .unwrap();
        collect(response).await;
        let taps = tap.taps();
        let Tap::WsRequest {
            url,
            headers,
            body,
            account,
        } = &taps[0]
        else {
            panic!("{taps:?}");
        };
        assert_eq!(url, &ws_url);
        assert_eq!(
            crate::rawjson::get(body, "type").str(),
            "response.create",
            "the frame as sent"
        );
        assert!(
            headers.contains(&("Authorization".into(), "Bearer sk-FAKE".into())),
            "{headers:?}"
        );
        assert_eq!(
            account,
            &["codex", &cred.id, &cred.label, "api_key", "sk-FAKE"].map(str::to_owned)
        );
        let mut want = vec![];
        if !continuation {
            want.push(Tap::WsHandshake(101));
        }
        want.extend([Tap::WsResponse(CREATED.into()), Tap::WsResponse(COMPLETED.into())]);
        assert_eq!(taps[1..], want[..], "continuation={continuation}");
    }
}

/// A refused upgrade is recorded as an HTTP attempt (`RecordAPIWebsocketUpgradeRejection`):
/// a GET to the HTTP form of the URL with Connection and Upgrade, the status and the body.
#[tokio::test]
async fn capture_records_a_refused_upgrade_as_an_http_attempt() {
    use crate::codex_testkit::{Tap, Wiretap};
    const BODY_401: &str = r#"{"error":{"message":"bad key","type":"invalid_request_error"}}"#;
    let (up, url) = upstream(vec![]).await;
    *up.reject.lock().unwrap() = Some((401, BODY_401));
    let tap = Arc::new(Wiretap::default());
    let _ = CodexExecutor::new()
        .unwrap()
        .execute_in_session(
            &credential(&url),
            tapped(BODY, &tap),
            &Config::default(),
            &session(false),
        )
        .await
        .err()
        .expect("401 handshake");
    let taps = tap.taps();
    assert!(matches!(&taps[0], Tap::WsRequest { .. }), "{taps:?}");
    let Tap::Request {
        url: upgrade,
        method,
        headers,
        body,
        ..
    } = &taps[1]
    else {
        panic!("{taps:?}");
    };
    assert_eq!(
        (upgrade.as_str(), method.as_str(), body.as_str()),
        (format!("{url}/responses").as_str(), "GET", "")
    );
    for pair in [("Connection", "Upgrade"), ("Upgrade", "websocket")] {
        assert!(headers.contains(&(pair.0.into(), pair.1.into())), "{headers:?}");
    }
    assert!(matches!(&taps[2], Tap::Metadata(401, _)), "{taps:?}");
    assert_eq!(
        taps[3..],
        [Tap::Chunk(BODY_401.into())],
        "no dial error after a rejection"
    );
}

/// A dial that never reaches an HTTP response is `api.websocket.error` stage `dial`.
#[tokio::test]
async fn capture_records_a_dial_failure() {
    use crate::codex_testkit::{Tap, Wiretap};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let tap = Arc::new(Wiretap::default());
    let error = CodexExecutor::new()
        .unwrap()
        .execute_in_session(
            &credential(&url),
            tapped(BODY, &tap),
            &Config::default(),
            &session(false),
        )
        .await
        .err()
        .expect("nothing listens");
    let taps = tap.taps();
    assert!(matches!(&taps[0], Tap::WsRequest { .. }), "{taps:?}");
    // The client-facing text stays fixed; the request log also gets the evidence.
    assert_eq!(error.body, "codex websockets executor: dial failed");
    let [Tap::WsError(stage, text), Tap::WsCause(cause)] = &taps[1..] else {
        panic!("{taps:?}");
    };
    assert_eq!(
        (stage.as_str(), text.as_str()),
        ("dial", "codex websockets executor: dial failed")
    );
    assert!(cause.starts_with("connect (ConnectionRefused): "), "{cause}");
}

/// The errors whose public text is fixed keep their causes apart in the request log:
/// a close frame's code, a stream that ends without one, and the proxy's own idle
/// deadline. Public texts and scopes are unchanged.
#[tokio::test]
async fn capture_keeps_why_a_socket_ended() {
    use crate::codex_testkit::{Tap, Wiretap};
    async fn read_failure(script: Vec<Act>, idle: Option<Duration>) -> (ExecError, String, String) {
        let (_up, url) = upstream(script).await;
        let tap = Arc::new(Wiretap::default());
        let mut executor = CodexExecutor::new().unwrap();
        if let Some(idle) = idle {
            executor.ws = Arc::new(Pool::with_idle(idle));
        }
        let response = executor
            .execute_in_session(
                &credential(&url),
                tapped(BODY, &tap),
                &Config::default(),
                &session(false),
            )
            .await
            .unwrap();
        let events = tokio::time::timeout(Duration::from_secs(5), collect(response))
            .await
            .expect("the turn ends");
        let error = events.last().unwrap().as_ref().expect_err("the turn fails").clone();
        let taps = tap.taps();
        let at = taps
            .iter()
            .position(|t| matches!(t, Tap::WsError(stage, _) if stage == "read"))
            .unwrap_or_else(|| panic!("no read error: {taps:?}"));
        let (Tap::WsError(_, text), Some(Tap::WsCause(cause))) = (&taps[at], taps.get(at + 1)) else {
            panic!("no cause: {taps:?}");
        };
        assert_eq!(
            text.as_bytes(),
            error.body.as_ref(),
            "the Error line is the turn's error"
        );
        (error, text.clone(), cause.clone())
    }

    let (error, text, cause) = read_failure(vec![Act::SendClose(vec![CREATED], 1011)], None).await;
    assert_eq!(error.scope, FailureScope::Request, "an uncertain loss (N1 #111 W1)");
    assert_eq!(text, "websocket: close 1011 (internal server error): bye");
    assert_eq!(cause, "remote_close: code 1011");

    // gorilla's text for a peer that vanished is close 1006, which no peer can send:
    // the cause says it was an end of stream, not a close frame.
    let (error, text, cause) = read_failure(vec![Act::SendDrop(vec![CREATED])], None).await;
    assert_eq!(error.scope, FailureScope::Request);
    assert_eq!(text, "websocket: close 1006 (abnormal closure): unexpected EOF");
    assert!(cause.starts_with("eof"), "{cause}");

    let (error, text, cause) = read_failure(
        vec![Act::Pings(Duration::from_millis(50), 40)],
        Some(Duration::from_millis(300)),
    )
    .await;
    assert_eq!(error.scope, FailureScope::Request);
    assert_eq!(text, "codex websockets executor: read idle timeout");
    assert_eq!(cause, "local_timeout: no application message within 300ms");
}

/// A TLS failure is labelled from BoringSSL's error type: the upstream answers plain
/// HTTP on a `wss` URL.
#[tokio::test]
async fn capture_labels_a_tls_dial_failure() {
    use crate::codex_testkit::{Tap, Wiretap};
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("https://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let _ = socket.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\nnot tls").await;
        }
    });
    let tap = Arc::new(Wiretap::default());
    let error = CodexExecutor::new()
        .unwrap()
        .execute_in_session(
            &credential(&url),
            tapped(BODY, &tap),
            &Config::default(),
            &session(false),
        )
        .await
        .err()
        .expect("no TLS server");
    assert_eq!(error.body, "codex websockets executor: dial failed");
    assert_eq!(error.scope, FailureScope::Transport);
    let taps = tap.taps();
    let Some(Tap::WsCause(cause)) = taps.last() else {
        panic!("{taps:?}");
    };
    assert!(cause.starts_with("tls: "), "{cause}");
}

/// A dial error's library text can carry the URL with its userinfo and query. It
/// reaches only the request log's diagnostic (which the log redacts), never the error's
/// body, `Display` or `Debug`.
#[tokio::test]
async fn a_dial_diagnostic_stays_out_of_the_public_error() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let url = format!("http://alice:pw-h1dden@{addr}/backend?api_key=sk-h1dden");
    let error = CodexExecutor::new()
        .unwrap()
        .execute_in_session(&credential(&url), request(BODY), &Config::default(), &session(false))
        .await
        .err()
        .expect("nothing listens");
    assert_eq!(error.body, "codex websockets executor: dial failed");
    for shown in [format!("{error}"), format!("{error:?}")] {
        assert!(!shown.contains("h1dden"), "{shown}");
    }
    assert!(error.diagnostic.is_some());
}

/// An upstream `error` event is recorded as received, then as stage `upstream_error`
/// with the turn's error.
#[tokio::test]
async fn capture_records_an_upstream_error_event() {
    use crate::codex_testkit::{Tap, Wiretap};
    const ERROR: &str =
        r#"{"type":"error","status":400,"error":{"type":"invalid_request_error","message":"bad input"}}"#;
    let (_up, url) = upstream(vec![Act::Send(vec![ERROR])]).await;
    let tap = Arc::new(Wiretap::default());
    let response = CodexExecutor::new()
        .unwrap()
        .execute_in_session(
            &credential(&url),
            tapped(BODY, &tap),
            &Config::default(),
            &session(false),
        )
        .await
        .unwrap();
    let error = collect(response)
        .await
        .pop()
        .expect("the turn ends")
        .expect_err("with the upstream error");
    let taps = tap.taps();
    assert_eq!(
        taps[2..],
        [
            Tap::WsResponse(ERROR.into()),
            Tap::WsError(
                "upstream_error".into(),
                String::from_utf8_lossy(&error.body).into_owned()
            ),
        ]
    );
}

/// Go's executor holds the session lock through its synchronous observer: a turn
/// abandoned while an observation runs keeps the session until the observer returns, so
/// the next turn can neither write upstream nor be observed before that.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abandoned_turn_keeps_its_session_until_its_observation_returns() {
    /// Holds every message until `release`.
    struct Holding {
        calls: std::sync::atomic::AtomicUsize,
        release: tokio::sync::Semaphore,
    }
    impl cpa_core::exec::WebSocketObserver for Holding {
        fn response<'a>(
            &'a self,
            _: &'a str,
            _: &'a Credential,
            _: &'a str,
            _: &'a [u8],
        ) -> futures_util::future::BoxFuture<'a, ()> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                let _ = self.release.acquire().await.unwrap();
            })
        }
    }
    let (up, url) = upstream(vec![
        Act::Send(vec![CREATED, COMPLETED]),
        Act::Send(vec![CREATED, COMPLETED]),
    ])
    .await;
    let executor = Arc::new(CodexExecutor::new().unwrap());
    let observer = Arc::new(Holding {
        calls: Default::default(),
        release: tokio::sync::Semaphore::new(0),
    });
    let turn = |executor: Arc<CodexExecutor>| {
        let (url, observer) = (url.clone(), observer.clone());
        tokio::spawn(async move {
            let mut req = request(BODY);
            req.usage = cpa_core::exec::UsageSink::default().with_websocket(Some(observer));
            if let Ok(response) = executor
                .execute_in_session(&credential(&url), req, &Config::default(), &session(false))
                .await
            {
                collect(response).await;
            }
        })
    };
    let a = turn(executor.clone());
    let calls = || observer.calls.load(std::sync::atomic::Ordering::SeqCst);
    for _ in 0..250 {
        if calls() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(calls(), 1, "turn A's first message was not observed");
    a.abort();
    let _ = a.await;
    let lock = executor.ws.session("conn-1").turn.clone();
    assert!(
        lock.try_lock().is_err(),
        "dropping A released the session during its observation"
    );
    let b = turn(executor.clone());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        up.frames.lock().unwrap().len(),
        1,
        "turn B wrote while A's observation ran"
    );
    assert_eq!(calls(), 1, "turn B was observed while A's observation ran");
    observer.release.add_permits(100);
    tokio::time::timeout(Duration::from_secs(10), b).await.unwrap().unwrap();
    assert!(lock.try_lock().is_ok(), "the session stayed locked after both turns");
    assert_eq!(
        up.frames.lock().unwrap().len(),
        2,
        "turn B did not run after the release"
    );
}

/// A pooled socket on session "conn-1" to the mock at `url`, its reader running.
async fn pooled(executor: &CodexExecutor, url: &str) -> Arc<super::Upstream> {
    let session = executor.ws.session("conn-1");
    let target = Target::new(
        &credential(url),
        ws_url(&format!("{url}/responses")).unwrap(),
        &Config::default(),
    );
    executor
        .ensure(&session, &target, &HeaderMap::new(), "gpt-fixture", false, None)
        .await
        .unwrap()
        .0
}

async fn read_text(reads: &mut mpsc::Receiver<Read>) -> String {
    match tokio::time::timeout(Duration::from_secs(5), reads.recv()).await {
        Ok(Some(Read::Text(text))) => text,
        Ok(Some(Read::Failed(error))) => panic!("read failed: {error}"),
        other => panic!("no upstream event: {}", other.is_ok()),
    }
}

/// What the mock upstream received, once `count` frames arrived.
async fn received(up: &Upstream, count: usize) -> Vec<String> {
    for _ in 0..250 {
        if up.frames.lock().unwrap().len() >= count {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    up.frames.lock().unwrap().clone()
}

const INTERRUPT_R1: &str = r#"{"type":"response.interrupt","response_id":"r1","mode":"discard_partial_items"}"#;

/// Go `TestInterruptExecutionSessionRequiresActiveRead`: a socket kept between turns is
/// not the current upstream (the HTTP fallback may cancel locally); once a turn reads,
/// the interrupt reaches the socket byte for byte.
#[tokio::test]
async fn interrupt_needs_a_turn_reading_on_the_socket() {
    let (up, url) = upstream(vec![]).await;
    let executor = CodexExecutor::new().unwrap();
    assert_eq!(
        executor
            .interrupt_session("conn-1", INTERRUPT_R1.as_bytes(), |_| true)
            .await,
        Err(InterruptError::NoActive),
        "no session"
    );
    let conn = pooled(&executor, &url).await;
    assert_eq!(
        executor
            .interrupt_session("conn-1", INTERRUPT_R1.as_bytes(), |_| true)
            .await,
        Err(InterruptError::NoActive),
        "idle socket"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(up.frames.lock().unwrap().is_empty(), "the idle socket was written");

    let _reads = conn.activate(false);
    let mut checked = Vec::new();
    let check = |id: &str| {
        checked.push(id.to_owned());
        false
    };
    assert_eq!(
        executor
            .interrupt_session("conn-1", INTERRUPT_R1.as_bytes(), check)
            .await,
        Err(InterruptError::Rejected(
            "websocket credential is no longer enabled".into()
        )),
        "disabled credential"
    );
    assert_eq!(checked, ["codex-ws.json"], "the check gets the socket's credential");
    executor
        .interrupt_session("conn-1", INTERRUPT_R1.as_bytes(), |_| true)
        .await
        .unwrap();
    assert_eq!(received(&up, 1).await, [INTERRUPT_R1]);
}

/// Go `TestInterruptExecutionSessionCompletesDuringAuthCheck`: the response finishes
/// while the account check runs, and the socket's state is read after the check. Go
/// then answers success without forwarding; current Codex waits for upstream's
/// acknowledgement of that interrupt, so it goes upstream (docs/DIFFERENCES-FROM-GO.md)
/// and, unacknowledged here, the call returns after the bounded wait.
#[tokio::test]
async fn interrupt_for_a_response_finished_during_the_auth_check_goes_upstream() {
    let (up, url) = upstream(vec![Act::Send(vec![CREATED, COMPLETED])]).await;
    let mut executor = CodexExecutor::new().unwrap();
    executor.ws = Arc::new(Pool::with_ack_wait(Duration::from_millis(100)));
    let conn = pooled(&executor, &url).await;
    let mut reads = conn.activate(false);
    conn.send(r#"{"type":"response.create"}"#.into()).await.unwrap();
    read_text(&mut reads).await;
    read_text(&mut reads).await;
    // The turn is still reading when the check starts and done when it returns.
    let finish = || {
        conn.deactivate();
        true
    };
    let late = br#"{"type":"response.interrupt","response_id":"r1"}"#;
    let outcome = executor.interrupt_session("conn-1", late, |_| finish()).await;
    assert_eq!(outcome, Ok(None), "no acknowledgement arrived");
    conn.send("barrier".into()).await.unwrap();
    assert_eq!(
        received(&up, 3).await,
        [
            r#"{"type":"response.create"}"#,
            r#"{"type":"response.interrupt","response_id":"r1"}"#,
            "barrier"
        ]
    );
}

/// Go `TestInterruptExecutionSessionAfterTerminal`, with current Codex's acknowledgement
/// handshake (docs/DIFFERENCES-FROM-GO.md): an interrupt for a response finished before
/// the turn reading now is done and never reaches upstream, as in Go; one for a finished
/// response while no turn reads goes upstream (Go: done, not forwarded) and returns
/// after the bounded wait without an acknowledgement here; an unknown ID on an idle
/// socket falls through to the HTTP fallback; after the socket is lost there is no
/// active upstream.
#[tokio::test]
async fn late_interrupts_for_finished_responses() {
    for terminal in [
        "response.completed",
        "response.done",
        "response.incomplete",
        "response.failed",
    ] {
        let finished: &'static str =
            Box::leak(format!(r#"{{"type":"{terminal}","response":{{"id":"r1","output":[]}}}}"#).into_boxed_str());
        let (up, url) = upstream(vec![
            Act::Send(vec![CREATED, finished]),
            Act::Send(vec![]),
            Act::Send(vec![]),
            Act::Send(vec![r#"{"type":"response.incomplete","response":{"id":"r2"}}"#]),
            Act::Send(vec![]),
            Act::Send(vec![]),
            Act::SendClose(vec![], 1000),
        ])
        .await;
        let mut executor = CodexExecutor::new().unwrap();
        executor.ws = Arc::new(Pool::with_ack_wait(Duration::from_millis(100)));
        let conn = pooled(&executor, &url).await;
        let interrupt = |id: &str| format!(r#"{{"type":"response.interrupt","response_id":"{id}"}}"#);
        let send = |id: &'static str| {
            let executor = &executor;
            async move {
                executor
                    .interrupt_session("conn-1", interrupt(id).as_bytes(), |_| true)
                    .await
            }
        };
        let mut reads = conn.activate(false);
        conn.send(r#"{"type":"response.create"}"#.into()).await.unwrap();
        read_text(&mut reads).await;
        read_text(&mut reads).await;
        conn.deactivate();
        for _ in 0..2 {
            assert_eq!(send("r1").await, Ok(None), "{terminal}: finished r1, idle socket");
        }
        assert_eq!(
            send("unknown").await,
            Err(InterruptError::NoActive),
            "{terminal}: unknown"
        );

        let mut reads = conn.activate(false);
        assert_eq!(send("r1").await, Ok(None), "{terminal}: r1 while r2 runs");
        assert_eq!(send("r2").await, Ok(None), "{terminal}: active r2");
        read_text(&mut reads).await;
        conn.deactivate();
        for id in ["r1", "r2"] {
            assert_eq!(send(id).await, Ok(None), "{terminal}: finished {id}, idle socket");
        }
        assert_eq!(
            received(&up, 6).await,
            [
                r#"{"type":"response.create"}"#.to_owned(),
                interrupt("r1"),
                interrupt("r1"),
                interrupt("r2"),
                interrupt("r1"),
                interrupt("r2"),
            ],
            "{terminal}: r1 never reached upstream while r2 ran"
        );
        conn.send("bye".into()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), executor.ws.losses("conn-1").next())
            .await
            .unwrap_or_else(|_| panic!("{terminal}: the disconnect was not reported"));
        assert_eq!(
            send("r2").await,
            Err(InterruptError::NoActive),
            "{terminal}: lost socket"
        );
    }
}

/// A socket that ends right after a context-window failure, while the turn has yet to
/// give it to a Codex client: the reader drops the socket but leaves the loss to the
/// turn, whose own error after the event replaces it. After any other event, or for a
/// turn that does not preserve the failure, the reader reports the loss at once.
#[tokio::test]
async fn reader_leaves_the_loss_after_a_preserved_context_failure_to_the_turn() {
    const CONTEXT: &str = r#"{"type":"response.failed","response":{"id":"r1","status":"failed","error":{"code":"context_length_exceeded","message":"too long"}}}"#;
    for (preserves, last, deferred) in [
        (true, CONTEXT, true),
        (true, LIVE_CONTEXT_ERROR.trim(), true),
        (false, CONTEXT, false),
        (false, LIVE_CONTEXT_ERROR.trim(), false),
        (true, CREATED, false),
    ] {
        let (_up, url) = upstream(vec![Act::SendClose(vec![last], 1000)]).await;
        let executor = CodexExecutor::new().unwrap();
        let conn = pooled(&executor, &url).await;
        let mut reads = conn.activate(preserves);
        conn.send(r#"{"type":"response.create"}"#.into()).await.unwrap();
        assert_eq!(read_text(&mut reads).await, last);
        // The reader's own error follows the event and comes after its decision.
        let after = tokio::time::timeout(Duration::from_secs(5), reads.recv()).await;
        assert!(
            matches!(after, Ok(Some(Read::Failed(_)))),
            "{preserves} {last}: no read error"
        );
        let reported = tokio::time::timeout(Duration::from_millis(200), executor.ws.losses("conn-1").next())
            .await
            .is_ok();
        assert_eq!(reported, !deferred, "{preserves} {last}: loss reported by the reader");
        assert_eq!(
            conn.link.lock().expect("link").deferred.is_some(),
            deferred,
            "{preserves} {last}"
        );
        assert!(
            executor.ws.session("conn-1").current().is_none(),
            "{preserves} {last}: socket kept"
        );
    }
}

/// The ChatGPT Codex backend's rejection of an input larger than the window, recorded
/// from a live subscription request: an `error` event without a status, before
/// `response.created`.
const LIVE_CONTEXT_ERROR: &str = include_str!("../../cpa-common/tests/fixtures/codex_context_window_error_live.json");

fn codex_client_request(body: &str) -> ExecRequest {
    let mut req = request(body);
    req.headers.insert(
        http::header::USER_AGENT,
        http::HeaderValue::from_static("codex_cli_rs/0.167.0"),
    );
    req
}

/// The live context-window rejection reaches a Codex client as the `response.failed`
/// Codex compacts on, followed by Go's classified 400 `context_too_large`, which is also
/// the one loss the session reports. With bootstrap buffering, and when the upstream
/// closes the socket right after the event, the same holds. Other clients get the
/// classified error carrying the upstream's frame as sent, for delivery once failover is
/// decided (the native route, #24, #28).
#[tokio::test]
async fn live_context_window_error_reaches_a_codex_client_as_a_failure() {
    let live = LIVE_CONTEXT_ERROR.trim();
    let normalized = cpa_common::codex_client::context_window_failure_event(live, live)
        .unwrap()
        .into_owned();
    let buffered = Config::parse("codex:\n  stream-bootstrap-buffering: true\n").unwrap();
    for (name, codex_client, cfg, act) in [
        ("codex", true, Config::default(), Act::Send(vec![live])),
        ("codex buffered", true, buffered.clone(), Act::Send(vec![live])),
        (
            "codex then close",
            true,
            Config::default(),
            Act::SendClose(vec![live], 1000),
        ),
        (
            "codex buffered then close",
            true,
            buffered.clone(),
            Act::SendClose(vec![live], 1000),
        ),
        ("other", false, Config::default(), Act::Send(vec![live])),
        (
            "other buffered then close",
            false,
            buffered.clone(),
            Act::SendClose(vec![live], 1000),
        ),
    ] {
        let (_up, url) = upstream(vec![act]).await;
        let executor = CodexExecutor::new().unwrap();
        let req = if codex_client {
            codex_client_request(BODY)
        } else {
            request(BODY)
        };
        let items = match executor
            .execute_in_session(&credential(&url), req, &cfg, &session(false))
            .await
        {
            Ok(response) => collect(response).await,
            Err(error) => vec![Err(error)],
        };
        let (events, errors): (Vec<_>, Vec<_>) = items.into_iter().partition(Result::is_ok);
        let events: Vec<String> = events.into_iter().map(Result::unwrap).collect();
        if codex_client {
            assert_eq!(events, std::slice::from_ref(&normalized), "{name}");
        } else {
            assert!(events.is_empty(), "{name}: {events:?}");
        }
        let [Err(error)] = errors.as_slice() else {
            panic!("{name}: one error, got {errors:?}");
        };
        assert_eq!(error.status, 400, "{name}");
        assert_eq!(error.scope, FailureScope::Request, "{name}");
        let code = |e: &ExecError| {
            crate::rawjson::get(&String::from_utf8_lossy(&e.body), "error.code")
                .str()
                .to_owned()
        };
        assert_eq!(code(error), "context_too_large", "{name}");
        // A Codex client's turn ends its connection after the event itself: no loss is
        // reported, not even a close right after the event.
        if codex_client {
            assert!(
                executor.ws.losses("conn-1").pending().is_none(),
                "{name}: a loss was reported: {:?}",
                executor.ws.losses("conn-1").pending().map(|l| code(&l))
            );
        } else {
            assert_eq!(
                error.delivered_event().map(|b| String::from_utf8_lossy(b).into_owned()),
                Some(live.to_owned()),
                "{name}: the frame as sent"
            );
        }
    }
}

/// A Codex client's turn abandoned while an observer holds the context-window rejection
/// keeps its read channel until the observation returns. A reader that loses the socket
/// in that window reports its own loss: the gone turn cannot report a deferred one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abandoned_preserving_turn_leaves_no_loss_unreported() {
    /// Holds every message until `release`.
    struct Holding {
        calls: std::sync::atomic::AtomicUsize,
        release: tokio::sync::Semaphore,
    }
    impl cpa_core::exec::WebSocketObserver for Holding {
        fn response<'a>(
            &'a self,
            _: &'a str,
            _: &'a Credential,
            _: &'a str,
            _: &'a [u8],
        ) -> futures_util::future::BoxFuture<'a, ()> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                let _ = self.release.acquire().await.unwrap();
            })
        }
    }
    const FAILED: &str = r#"{"type":"response.failed","response":{"id":"r1","status":"failed","error":{"code":"context_length_exceeded","message":"too long"}}}"#;
    for rejection in [FAILED, LIVE_CONTEXT_ERROR.trim()] {
        let close = Arc::new(tokio::sync::Notify::new());
        let (_up, url) = upstream(vec![Act::SendCloseOn(vec![rejection], close.clone())]).await;
        let executor = Arc::new(CodexExecutor::new().unwrap());
        let observer = Arc::new(Holding {
            calls: Default::default(),
            release: tokio::sync::Semaphore::new(0),
        });
        let turn = {
            let (executor, url, observer) = (executor.clone(), url.clone(), observer.clone());
            tokio::spawn(async move {
                let mut req = codex_client_request(BODY);
                req.usage = cpa_core::exec::UsageSink::default().with_websocket(Some(observer));
                if let Ok(response) = executor
                    .execute_in_session(&credential(&url), req, &Config::default(), &session(false))
                    .await
                {
                    collect(response).await;
                }
            })
        };
        let calls = || observer.calls.load(std::sync::atomic::Ordering::SeqCst);
        for _ in 0..250 {
            if calls() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(calls(), 1, "{rejection}: the rejection was not observed");
        let conn = executor.ws.session("conn-1").current().expect("socket");
        turn.abort();
        let _ = turn.await;
        assert!(
            conn.link.lock().expect("link").active.is_some(),
            "{rejection}: the abandoned turn released its read channel during the observation"
        );
        close.notify_one();
        let loss = tokio::time::timeout(Duration::from_secs(5), executor.session_losses("conn-1").next()).await;
        let deferred = conn.link.lock().expect("link").deferred.is_some();
        observer.release.add_permits(100);
        assert!(
            loss.is_ok(),
            "{rejection}: the reader's loss was left unreported (deferred: {deferred})"
        );
        assert!(!deferred, "{rejection}: a loss was deferred to the abandoned turn");
    }
}

#[path = "codex_duplex_tests.rs"]
mod duplex;

#[path = "codex_ws_interrupt_tests.rs"]
mod interrupt;

/// `buildCodexResponsesWebsocketURL` answers, recorded from Go v8.0.35 with the inputs of `url.Parse` and
/// `URL.String()` (tests/reference/go_url).
#[test]
fn ws_url_follows_go() {
    let cases: Value = serde_json::from_str(include_str!("../tests/fixtures/go_url_string.json")).unwrap();
    for case in cases.as_array().unwrap() {
        let input = case["in"].as_str().unwrap();
        let want = &case["codex_ws"];
        match ws_url(input) {
            Ok(got) => assert_eq!(Some(got.as_str()), want["out"].as_str(), "{input}"),
            Err(e) => {
                assert_eq!(
                    String::from_utf8_lossy(&e.body),
                    want["err"].as_str().unwrap(),
                    "{input}"
                );
                // A plain Go error before dialing: no status, no retry round, and the
                // model cools for the credential.
                assert_eq!((e.status, e.scope), (0, cpa_core::exec::FailureScope::Model), "{input}");
            }
        }
    }
}

fn header_map(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        headers.insert(
            http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            http::HeaderValue::from_str(value).unwrap(),
        );
    }
    headers
}

/// Only `x-codex-turn-state` and `openai-model` leave the handshake, in an event Codex
/// reads; cookies, credentials and other upstream headers never do.
#[test]
fn handshake_metadata_event_carries_only_the_allowlist() {
    let handshake = header_map(&[
        ("x-codex-turn-state", "ts-upstream"),
        ("openai-model", "gpt-served"),
        ("x-reasoning-included", ""),
        ("set-cookie", "session=SECRET"),
        ("authorization", "Bearer SECRET"),
        ("x-request-id", "req-1"),
        ("x-openai-model", "other"),
    ]);
    let event = handshake_metadata_event(&handshake).expect("an event");
    assert_eq!(
        event,
        r#"{"type":"response.metadata","headers":{"x-codex-turn-state":"ts-upstream","openai-model":"gpt-served"}}"#
    );

    let model_only = header_map(&[("openai-model", "gpt-served"), ("x-codex-turn-state", "  ")]);
    assert_eq!(
        handshake_metadata_event(&model_only).as_deref(),
        Some(r#"{"type":"response.metadata","headers":{"openai-model":"gpt-served"}}"#)
    );
    // Codex reads `x-reasoning-included` only from a handshake; an event cannot carry it.
    let reasoning_only = header_map(&[("x-reasoning-included", ""), ("set-cookie", "a=b")]);
    assert_eq!(handshake_metadata_event(&reasoning_only), None);
    assert_eq!(handshake_metadata_event(&HeaderMap::new()), None);
}

/// Upstream's `openai-model` reaches the server only when the proxy sent the client's
/// model unchanged; otherwise Codex would report the proxy's own rewrite as a reroute.
#[test]
fn returned_handshake_drops_the_model_the_proxy_rewrote() {
    assert!(model_kept(br#"{"model":"gpt-5.5"}"#, r#"{"model":"gpt-5.5"}"#));
    assert!(model_kept(br#"{"model":"GPT-5.5"}"#, r#"{"model":"gpt-5.5"}"#));
    assert!(!model_kept(br#"{"model":"gpt-alias"}"#, r#"{"model":"gpt-5.5"}"#));
    assert!(!model_kept(br#"{"model":"gpt-5.5(high)"}"#, r#"{"model":"gpt-5.5"}"#));
    assert!(!model_kept(br#"{"input":[]}"#, r#"{"model":"gpt-5.5"}"#));
    // ASCII case only, as Codex compares: the Kelvin sign is not `k`.
    assert!(!model_kept("{\"model\":\"\u{212A}\"}".as_bytes(), r#"{"model":"k"}"#));

    let handshake = header_map(&[("openai-model", "gpt-5.5"), ("x-codex-turn-state", "ts")]);
    let kept = returned_handshake(handshake.clone(), true);
    assert_eq!(kept, handshake);
    let rewritten = returned_handshake(handshake, false);
    assert!(rewritten.get("openai-model").is_none());
    assert_eq!(rewritten.get("x-codex-turn-state").unwrap(), "ts");
}

/// #3: a send that provably never left the socket the turn got (its reader already
/// recorded the loss) is retried once on a fresh socket, and the turn returns the fresh
/// socket's handshake. The client's `response.metadata` comes from the socket that
/// serves the turn, never from the one whose write failed. A write that may have sent
/// part of the frame is never retried (N1 #111 W1,
/// `a_write_the_upstream_never_reads_times_out`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_retried_on_a_fresh_socket_returns_that_sockets_handshake() {
    let dials = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = dials.clone();
    let app = axum::Router::new().fallback(move |ws: WebSocketUpgrade| {
        let dial = counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        async move {
            let (turn_state, model) = if dial == 0 {
                ("ts-failed", "gpt-failed")
            } else {
                ("ts-fresh", "gpt-fixture")
            };
            let ws = ws.max_message_size(64 << 20).max_frame_size(64 << 20);
            let mut response = ws.on_upgrade(move |mut socket| async move {
                if dial == 0 {
                    // Half-open: hold the socket without reading, so the write times out.
                    tokio::time::sleep(Duration::from_secs(120)).await;
                    return;
                }
                while let Some(Ok(message)) = socket.recv().await {
                    if matches!(message, AxMessage::Text(_)) {
                        for event in [CREATED, COMPLETED] {
                            let _ = socket.send(AxMessage::Text(event.into())).await;
                        }
                    }
                }
            });
            for (name, value) in [("x-codex-turn-state", turn_state), ("openai-model", model)] {
                response.headers_mut().insert(
                    axum::http::HeaderName::from_static(name),
                    axum::http::HeaderValue::from_static(value),
                );
            }
            response
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });

    let executor = CodexExecutor::new().unwrap();
    // The session holds the first socket, whose reader recorded a loss the session has
    // not dropped yet: a write there provably sends nothing.
    let stale = pooled(&executor, &url).await;
    stale.link.lock().unwrap().lost = Some(transport("codex websockets executor: stale socket"));
    let response = tokio::time::timeout(
        Duration::from_secs(60),
        executor.execute_in_session(&credential(&url), request(BODY), &Config::default(), &session(false)),
    )
    .await
    .expect("the retry ends the turn")
    .expect("the fresh socket serves the turn");
    assert_eq!(
        dials.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "one retry on a fresh socket"
    );
    assert_eq!(
        handshake_metadata_event(&response.headers).as_deref(),
        Some(
            r#"{"type":"response.metadata","headers":{"x-codex-turn-state":"ts-fresh","openai-model":"gpt-fixture"}}"#
        )
    );
    let events: Vec<String> = collect(response)
        .await
        .into_iter()
        .map(|event| event.expect("an upstream event"))
        .collect();
    assert_eq!(events, [CREATED, COMPLETED]);
}

/// A loopback upstream that gives dial `n` the handshake headers `handshakes[n]` and
/// answers every text frame with CREATED and COMPLETED. With `stall_first`, the first
/// socket answers one frame and then stops reading, so a large later write times out.
/// Returns the URL, the dial count and every frame received, in order.
async fn handshake_upstream(
    handshakes: Vec<Vec<(&'static str, &'static str)>>,
    stall_first: bool,
) -> (String, Arc<std::sync::atomic::AtomicUsize>, Arc<Mutex<Vec<String>>>) {
    let dials = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let frames: Arc<Mutex<Vec<String>>> = Arc::default();
    let (counted, received) = (dials.clone(), frames.clone());
    let app = axum::Router::new().fallback(move |ws: WebSocketUpgrade| {
        let dial = counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let received = received.clone();
        let handshake = handshakes[dial].clone();
        async move {
            let ws = ws.max_message_size(64 << 20).max_frame_size(64 << 20);
            let mut response = ws.on_upgrade(move |mut socket| async move {
                while let Some(Ok(message)) = socket.recv().await {
                    let AxMessage::Text(text) = message else {
                        continue;
                    };
                    received.lock().unwrap().push(text.as_str().to_owned());
                    for event in [CREATED, COMPLETED] {
                        let _ = socket.send(AxMessage::Text(event.into())).await;
                    }
                    if stall_first && dial == 0 {
                        // Half-open: hold the socket without reading again.
                        tokio::time::sleep(Duration::from_secs(120)).await;
                        return;
                    }
                }
            });
            for (name, value) in handshake {
                response.headers_mut().insert(
                    axum::http::HeaderName::from_static(name),
                    axum::http::HeaderValue::from_static(value),
                );
            }
            response
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (url, dials, frames)
}

/// A turn of `model` on session `conn-1`; returns the metadata event the server would
/// deliver, the returned headers and the events.
async fn metadata_turn(
    executor: &CodexExecutor,
    url: &str,
    body: &str,
    model: &str,
    continuation: bool,
) -> (Option<String>, HeaderMap, Vec<String>) {
    let mut req = request(body);
    req.model = model.into();
    req.requested_model = model.into();
    let response = tokio::time::timeout(
        Duration::from_secs(60),
        executor.execute_in_session(&credential(url), req, &Config::default(), &session(continuation)),
    )
    .await
    .expect("the turn starts")
    .expect("the turn is served");
    let headers = response.headers.clone();
    let events = collect(response)
        .await
        .into_iter()
        .map(|event| event.expect("an upstream event"))
        .collect();
    (handshake_metadata_event(&headers), headers, events)
}

/// #24 (refs #3): a later request on a reused socket gets only `openai-model` from that
/// socket's own upgrade. The pinned client re-emits its socket's upgrade model on every
/// request (`responses_websocket.rs:302-303`); turn state belongs to the request that
/// opened the socket and never repeats. The upgrade answered a request for one upstream
/// model, so a request for another model on the same socket gets nothing: the header
/// does not describe it. No other upgrade header travels on any request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reused_socket_repeats_only_its_own_upgrade_model() {
    let (url, dials, frames) = handshake_upstream(
        vec![vec![
            ("x-codex-turn-state", "ts-open"),
            ("openai-model", "gpt-served"),
            ("x-reasoning-included", "true"),
            ("set-cookie", "session=SECRET"),
        ]],
        false,
    )
    .await;
    let executor = CodexExecutor::new().unwrap();
    let body = |model: &str, previous: bool| {
        let previous = if previous {
            r#","previous_response_id":"r1""#
        } else {
            ""
        };
        format!(r#"{{"type":"response.create","model":"{model}"{previous},"input":[]}}"#)
    };
    const LATER: &str = r#"{"type":"response.metadata","headers":{"openai-model":"gpt-served"}}"#;
    let turns = [
        // (client model, continuation, the delivered metadata)
        (
            "gpt-fixture",
            false,
            Some(
                r#"{"type":"response.metadata","headers":{"x-codex-turn-state":"ts-open","openai-model":"gpt-served"}}"#,
            ),
        ),
        ("gpt-fixture", true, Some(LATER)),
        ("gpt-other", false, None),
        ("gpt-other", true, None),
        ("gpt-fixture", false, Some(LATER)),
        ("gpt-fixture", true, Some(LATER)),
    ];
    let mut sent = Vec::new();
    for (i, (model, continuation, want)) in turns.into_iter().enumerate() {
        let frame = body(model, continuation);
        let (metadata, headers, events) = metadata_turn(&executor, &url, &frame, model, continuation).await;
        assert_eq!(metadata.as_deref(), want, "turn {i}");
        if i > 0 {
            assert!(
                headers.keys().all(|name| name == "openai-model"),
                "turn {i}: a later request carries no other upgrade header: {headers:?}"
            );
        }
        assert_eq!(events, [CREATED, COMPLETED], "turn {i}: upstream events unchanged");
        sent.push(frame);
    }
    assert_eq!(dials.load(std::sync::atomic::Ordering::SeqCst), 1, "one socket");
    assert_eq!(*frames.lock().unwrap(), sent, "only the client's frames, as sent");
}

/// #24 (refs #3): a later request whose send provably never left the reused socket (its
/// loss was already recorded) is retried on a fresh socket, which reports its own opening
/// metadata: its new turn state, and no model when its upgrade had none (here a blank
/// value). The replaced socket's model is never donated, then or on the next request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replacement_socket_never_inherits_the_old_sockets_model() {
    let (url, dials, frames) = handshake_upstream(
        vec![
            vec![("x-codex-turn-state", "ts-old"), ("openai-model", "gpt-old")],
            vec![("x-codex-turn-state", "ts-new"), ("openai-model", "   ")],
        ],
        true,
    )
    .await;
    let executor = CodexExecutor::new().unwrap();
    let (opening, _, _) = metadata_turn(&executor, &url, BODY, "gpt-fixture", false).await;
    assert_eq!(
        opening.as_deref(),
        Some(r#"{"type":"response.metadata","headers":{"x-codex-turn-state":"ts-old","openai-model":"gpt-old"}}"#)
    );
    // The reused socket's reader recorded a loss the session has not dropped yet.
    let reused = executor.ws.session("conn-1").current().expect("the reused socket");
    reused.link.lock().unwrap().lost = Some(transport("codex websockets executor: stale socket"));
    let next = r#"{"model":"gpt-fixture","input":[{"type":"message","role":"user","content":"again"}]}"#;
    let (retried, _, events) = metadata_turn(&executor, &url, next, "gpt-fixture", false).await;
    assert_eq!(dials.load(std::sync::atomic::Ordering::SeqCst), 2, "one fresh socket");
    assert_eq!(
        retried.as_deref(),
        Some(r#"{"type":"response.metadata","headers":{"x-codex-turn-state":"ts-new"}}"#),
        "the fresh socket's opening metadata only"
    );
    assert_eq!(events, [CREATED, COMPLETED]);
    let continuation = r#"{"model":"gpt-fixture","previous_response_id":"r1","input":[]}"#;
    let (later, headers, _) = metadata_turn(&executor, &url, continuation, "gpt-fixture", true).await;
    assert_eq!(later, None, "the new socket has no model to repeat");
    assert!(headers.is_empty(), "{headers:?}");
    assert_eq!(dials.load(std::sync::atomic::Ordering::SeqCst), 2);
    // The stale socket read only the first frame; the fresh one read the retried frame
    // once and then the continuation.
    let received = frames.lock().unwrap();
    assert_eq!(received.len(), 3);
    assert!(received[1].contains("again"));
    assert!(received[2].contains(r#""previous_response_id":"r1""#));
}

/// #25 and #6: the failure readings of a frame written as a JSON array read the members
/// the pinned client decodes it to (`codex_event::classification_view`). A positional
/// `response.failed` is the rate limit its `response.error` says; a positional wrapped
/// status error fails by its status; a positional event has no status, so its status-frame
/// reading finds none. The frame as sent reads as neither.
#[test]
fn positional_frames_read_by_their_decoded_members() {
    let failed = r#"["response.failed",null,null,{"id":"r1","error":{"type":"rate_limit_error","code":"rate_limit_exceeded","message":"limited"}},null,null,null,null,null,null,null,null,null,null]"#;
    let event = crate::codex_event::decode(failed).unwrap();
    let view = crate::codex_event::classification_view(failed);
    let (error, _) = response::terminal_failure(&view, false).unwrap();
    assert_eq!(error.status, 429);
    assert!(ws_error(&view, false).is_none());
    assert!(response::terminal_failure(failed, false).is_none());
    assert!(matches!(reading(failed, Some(&event), false), Reading::Failure { ref error, .. } if error.status == 429));
    assert!(account_shaped(failed, true));
    let wrapped = r#"["error",429,{"code":"rate_limit_exceeded","message":"limited"}]"#;
    assert!(crate::codex_event::decode(wrapped).is_none());
    assert_eq!(
        ws_error(&crate::codex_event::classification_view(wrapped), false).map(|e| e.status),
        Some(429)
    );
    assert!(ws_error(wrapped, false).is_none());
    assert!(matches!(reading(wrapped, None, false), Reading::Failure { ref error, body: None } if error.status == 429));
    assert!(account_shaped(wrapped, false));
}

/// The pinned first-party readers' outcome for each frame of
/// `tests/fixtures/codex_pinned_failure_readings.json` (its `provenance` names the pinned
/// source, the serde_json build and the evidence): nested `response.failed` errors written
/// as the `Error` struct by position, private values, malformed arrays, and `error`
/// frames with every status representation.
fn pinned_readings() -> Vec<Value> {
    let fixture: Value =
        serde_json::from_str(include_str!("../tests/fixtures/codex_pinned_failure_readings.json")).unwrap();
    fixture["cases"].as_array().unwrap().clone()
}

/// The pinned first-party readers' outcome for each frame of
/// `tests/fixtures/codex_pinned_notice_readings.json`, recorded by the same oracle as
/// [`pinned_readings`] (its `provenance` names the evidence): status spellings the first
/// fixture does not hold (group `status`), and failure slots a pinned struct refuses
/// (group `refused`).
fn notice_readings() -> Vec<Value> {
    let fixture: Value =
        serde_json::from_str(include_str!("../tests/fixtures/codex_pinned_notice_readings.json")).unwrap();
    fixture["cases"].as_array().unwrap().clone()
}

/// Whether the frame `raw` has no status at all: each `status` and `status_code` member
/// of its root object, every occurrence, is missing or `null` (a private raw value
/// holding `null` is `null` to this build's `Value` too). The proxy's one documented
/// reading beyond the pinned WebSocket reader keys on it: a decoded `error` without a
/// status keeps the status-less failure reading (`reading`).
fn statusless(raw: &str) -> bool {
    struct Members;
    impl<'de> serde::de::Visitor<'de> for Members {
        type Value = Vec<(String, Value)>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("an object")
        }
        fn visit_map<M: serde::de::MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
            let mut out = Vec::new();
            while let Some(entry) = map.next_entry::<String, Value>()? {
                out.push(entry);
            }
            Ok(out)
        }
    }
    use serde::Deserializer as _;
    serde_json::Deserializer::from_str(raw)
        .deserialize_map(Members)
        .unwrap_or_default()
        .iter()
        .filter(|(key, _)| key == "status" || key == "status_code")
        .all(|(_, value)| value.is_null())
}

/// The pinned WebSocket reader's decision on `case` against the proxy's [`reading`]: a
/// frame it fails fails the turn; a frame it reads on or skips fails only by the one
/// documented reading beyond it, a decoded `error` without any status ([`statusless`]).
/// Go's integer reading of a status (`gjson` `Int`, which also takes `"429"` or `429.0`)
/// decides nothing: a status the wrapped struct does not read is no wrapped status, and
/// the pinned `error` branch reads no status at all.
///
/// FP2 (#116) is the one deliberate departure from the recorded pinned decisions: research
/// main `5c3ff1dabf64` added `context_length_exceeded` to the codes
/// `parse_terminal_provider_error` raises without a status (`error.rs:110-112`), and its
/// WebSocket reader reads every frame the wrapped mapping leaves unmapped that way
/// (`responses_websocket.rs:906-925`). So every decoded frame of these fixtures named
/// for that code (`-context_length_exceeded-`, plain or private), which the pinned
/// reader read on, fails ([`MAIN_CONTEXT_FAILURES`] counts them per fixture).
fn assert_pinned_reading(case: &Value) -> bool {
    let name = case["name"].as_str().unwrap();
    let raw = case["raw"].as_str().unwrap();
    let event = crate::codex_event::decode(raw);
    assert_eq!(event.is_some(), case["accepted"].as_bool().unwrap(), "{name}");
    let failed = matches!(reading(raw, event.as_ref(), false), Reading::Failure { .. });
    if case["ws"] == "fail" {
        assert!(failed, "{name}: the pinned reader fails it ({})", case["pinned"]);
        return true;
    }
    if main_context_failure(case) {
        assert!(
            failed,
            "{name}: research main fails it (FP2), the pinned reader went on"
        );
        return true;
    }
    let decoded_error = event.as_ref().is_some_and(|e| e.kind == "error");
    assert_eq!(
        failed,
        decoded_error && statusless(raw),
        "{name}: the pinned reader goes on ({})",
        case["pinned"]
    );
    false
}

/// Whether research main fails a fixture frame the pinned reader read on (FP2, see
/// [`assert_pinned_reading`]): a decoded frame named for `context_length_exceeded`.
fn main_context_failure(case: &Value) -> bool {
    case["ws"] != "fail"
        && case["accepted"] == true
        && case["name"].as_str().unwrap().contains("-context_length_exceeded-")
}

/// The frames of each pinned fixture that [`main_context_failure`] turns into failures:
/// the failure readings, then the `status` group of the notice readings.
const MAIN_CONTEXT_FAILURES: [usize; 2] = [62, 81];

/// #25, #6 and #7 against the pinned readers ([`assert_pinned_reading`]): every status
/// that is present is a notice unless the wrapped struct reads it as an error status,
/// zero, negative, too large and type-wrong ones included, whether or not a private
/// value keeps the frame from reading as the pinned wrapped event.
#[test]
fn readings_follow_the_pinned_readers() {
    let mut checked = [0usize; 2];
    let mut main = 0;
    for case in pinned_readings() {
        main += usize::from(main_context_failure(&case));
        checked[usize::from(!assert_pinned_reading(&case))] += 1;
    }
    assert_eq!(main, MAIN_CONTEXT_FAILURES[0]);
    assert!(checked[0] > 150 && checked[1] > 150, "{checked:?}");
}

/// #25 (review finding 3): the status spellings Go's integer reading takes as an error
/// status and the pinned wrapped struct does not read as a `u16` (`"429"`, `" 429"`,
/// `"+429"`, `429.0`, `4.29e2`, `429e0`, a private number or raw value, `status_code`
/// spelled so) and the frames the wrapped struct refuses as a duplicate field (`status`
/// beside `status_code`, a repeated `status`) are notices when the pinned `error` branch
/// reads on: the turn goes on to the real completion and nothing cools. A terminal
/// provider code still fails; an escaped `status` key is the wrapped status it unescapes
/// to. Only a frame with no status at all (`null` in every occurrence) keeps the
/// status-less failure reading.
#[test]
fn notice_readings_follow_the_pinned_readers() {
    let mut checked = [0usize; 3];
    let mut main = 0;
    for case in notice_readings().iter().filter(|c| c["group"] == "status") {
        let raw = case["raw"].as_str().unwrap();
        main += usize::from(main_context_failure(case));
        if assert_pinned_reading(case) {
            checked[0] += 1;
        } else if statusless(raw) {
            checked[2] += 1;
        } else {
            let event = crate::codex_event::decode(raw).unwrap();
            assert!(
                matches!(reading(raw, Some(&event), false), Reading::Passed { decoded: true }),
                "{}: a notice",
                case["name"]
            );
            checked[1] += 1;
        }
    }
    assert_eq!(main, MAIN_CONTEXT_FAILURES[1]);
    assert!(checked[0] > 150 && checked[1] > 200 && checked[2] > 5, "{checked:?}");
}

/// #25 and #6: a decoded `response.failed`'s `response.error` is read as
/// `parse_failed_response` reads it into its `Error` struct
/// (`sse/responses_error.rs:14-24`), so the struct written as a JSON array of five or six
/// members is the object of those members, private values and escapes resolved. Its
/// failure then classifies, and is withheld or delivered, exactly as that object does. An
/// array the struct refuses (too short, too long, a member of the wrong type) stays one:
/// the pinned reader has only its generic stream error for it, which names no code.
#[test]
fn failed_errors_read_as_the_pinned_error_struct() {
    let mut read = [0usize; 2];
    for case in pinned_readings() {
        if case["error_array"] != true {
            continue;
        }
        let name = case["name"].as_str().unwrap();
        let raw = case["raw"].as_str().unwrap();
        let view: Value = serde_json::from_str(&crate::codex_event::classification_view(raw)).unwrap();
        let error = &view["response"]["error"];
        let event = crate::codex_event::decode(raw).unwrap();
        let Reading::Failure { error: got, .. } = reading(raw, Some(&event), false) else {
            panic!("{name}: a response.failed fails");
        };
        if case["error_struct"].is_null() {
            assert!(error.is_array(), "{name}: {error}");
            read[1] += 1;
            continue;
        }
        // Numbers by value: the view keeps `-0` as sent, which the struct's `i64` reads as 0.
        let fields = error.as_object().unwrap();
        let want = case["error_struct"].as_object().unwrap();
        assert_eq!(fields.len(), want.len(), "{name}: {error}");
        for (key, value) in want {
            match (value.as_f64(), fields.get(key)) {
                (Some(n), Some(got)) => assert_eq!(got.as_f64(), Some(n), "{name}: {key}"),
                (None, got) => assert_eq!(got, Some(value), "{name}: {key}"),
                (Some(_), None) => panic!("{name}: {key} missing"),
            }
        }
        // The same failure written with the object the struct reads.
        let object =
            serde_json::json!({"type": "response.failed", "response": {"id": "r1", "error": case["error_struct"]}})
                .to_string();
        let object_event = crate::codex_event::decode(&object).unwrap();
        let Reading::Failure { error: want, .. } = reading(&object, Some(&object_event), false) else {
            panic!("{name}: the object form fails");
        };
        assert_eq!(got.status, want.status, "{name}");
        // The same delivery decision; a withheld failure keeps its own frame's bytes.
        if want.withheld() {
            assert_eq!(got.delivery, withheld(raw), "{name}");
            assert_eq!(want.delivery, withheld(&object), "{name}");
        } else {
            assert_eq!(got.delivery, want.delivery, "{name}");
        }
        read[0] += 1;
    }
    assert!(read[0] >= 30 && read[1] >= 15, "{read:?}");
}

/// What [`withhold_sensitive`] makes of a failure `frame`'s delivery: withheld, with the
/// frame's original bytes kept for the credential guard.
fn withheld(frame: &str) -> Delivery {
    Delivery::Withheld(cpa_core::exec::WithheldFrame::new(Bytes::copy_from_slice(
        frame.trim().as_bytes(),
    )))
}

/// #6 (review finding 1): the nested positional rate limit, with the credential's own
/// token in its message, is the 429 its object form is, and never reaches the client:
/// withheld, as an account failure. The #5 relay and the session reader read the
/// positional context-window code as the pinned parser does too.
#[test]
fn nested_positional_errors_are_withheld_and_relayed_as_their_struct() {
    let nested = r#"{"type":"response.failed","response":{"id":"r1","error":["rate_limit_error","rate_limit_exceeded","limited at-FAKE acct-PRIVATE",null,null,null]}}"#;
    let event = crate::codex_event::decode(nested).unwrap();
    let Reading::Failure { mut error, .. } = reading(nested, Some(&event), false) else {
        panic!("a failure");
    };
    assert_eq!(error.status, 429);
    assert_eq!(error.delivery, withheld(nested));
    deliver_as_sent(&mut error, nested);
    assert_eq!(error.delivery, withheld(nested), "never as sent");
    assert!(account_shaped(nested, true));
    let context = r#"{"type":"response.failed","response":{"id":"r1","error":[null,"context_length_exceeded","too large",null,null]}}"#;
    let event = crate::codex_event::decode(context).unwrap();
    assert!(event.is_context_window_failure());
    assert!(preserved_rejection(context));
    assert!(matches!(
        crate::codex_event::context_window_event(context),
        Some(std::borrow::Cow::Borrowed(sent)) if sent == context
    ));
    let Reading::Failure { error, .. } = reading(context, Some(&event), false) else {
        panic!("a failure");
    };
    assert_eq!(error.status, 400);
    // A four-member array is no `Error`: no context window, and no classification. It is
    // still read by position for account safety (`codex_event::safety_views`).
    let short = r#"{"type":"response.failed","response":{"id":"r1","error":[null,"context_length_exceeded","too large",null]}}"#;
    assert!(!crate::codex_event::decode(short).unwrap().is_context_window_failure());
    assert!(account_shaped(
        r#"{"type":"response.failed","response":{"id":"r1","error":["rate_limit_error","rate_limit_exceeded","limited"]}}"#,
        true
    ));
}

/// #25 criterion 7 against #6 (hosted review on PR #63, `codex_ws.rs` original line
/// 1545): withholding an account failure from the client must not discard the bytes the
/// credential guard reads. The frame's `echo` member is one no reading keeps, so the
/// classified body never carries the token; the guard bound to the values the socket
/// sent still finds it in the original frame, as written and escaped, and makes the
/// failure Confidential with its classification unchanged. A value the socket never
/// sent leaves it Withheld.
#[test]
fn withheld_failures_keep_their_original_frame_for_the_credential_guard() {
    use crate::codex_secret::Guard;
    let sent = |token: &str| {
        let mut headers = http::HeaderMap::new();
        let value = http::HeaderValue::from_str(&format!("Bearer {token}")).unwrap();
        headers.insert("authorization", value);
        crate::codex_secret::from_request(&headers, "wss://chatgpt.example/backend-api/codex/responses")
    };
    let limited = |echo: &str| {
        format!(
            r#"{{"type":"error","status":429,"error":{{"type":"usage_limit_reached","message":"The usage limit has been reached."}},"echo":"{echo}"}}"#
        )
    };
    for (echo, confidential) in [
        ("at-FAKE", true),
        (r"at\u002dFAKE", true),
        (r"\u0061\u0074\u002d\u0046\u0041\u004b\u0045", true),
        ("at-OTHER", false),
    ] {
        let frame = limited(echo);
        let event = crate::codex_event::decode(&frame);
        let Reading::Failure { mut error, .. } = reading(&frame, event.as_ref(), false) else {
            panic!("{echo}: a failure");
        };
        deliver_as_sent(&mut error, &frame);
        assert_eq!(error.status, 429, "{echo}");
        assert!(error.withheld() && error.delivered_event().is_none(), "{echo}");
        assert!(!sent("at-FAKE").exposes(&error.body), "{echo}: the body drops `echo`");
        let guarded = sent("at-FAKE").guarded(error.clone());
        assert_eq!(guarded.confidential(), confidential, "{echo}");
        assert!(guarded.withheld(), "{echo}: never as sent");
        assert_eq!((guarded.status, guarded.scope), (error.status, error.scope), "{echo}");
        assert_eq!(guarded.body, error.body, "{echo}: the classified body stays");
        assert_eq!(guarded.retry_after, error.retry_after, "{echo}");
        // A value this socket never sent is no secret of it.
        assert!(!sent("at-NEVER").guarded(error).confidential(), "{echo}");
    }
}

/// #25 criterion 7 and #6 (review findings 1 and 2): a failure slot a pinned reader reads
/// by position keeps what the upstream wrote in it when the typed decode refuses it.
/// Processing and safety are separate facts. A `response.failed` whose `response.error`
/// array the `Error` struct refuses stays the pinned generic stream error, classified 502
/// as before, and is withheld when that array reads as an account failure by position. A
/// wrapped array neither pinned struct accepts stays skipped: it is passed, never a
/// failure, and withheld when it reads as one. Safe refused arrays and opaque arrays reach
/// the client as sent, and their classification does not change.
#[test]
fn refused_failure_slots_are_read_for_account_safety() {
    let account = |name: &str| !name.contains("safe") && !name.contains("empty") && !name.starts_with("opaque");
    let mut checked = [0usize; 4];
    for case in notice_readings().iter().filter(|c| c["group"] == "refused") {
        let name = case["name"].as_str().unwrap();
        let raw = case["raw"].as_str().unwrap();
        let event = crate::codex_event::decode(raw);
        assert_eq!(event.is_some(), case["accepted"].as_bool().unwrap(), "{name}");
        let ws = case["ws"].as_str().unwrap();
        // The group's read-on frames are decoded `error` events without a status, which
        // the proxy's documented status-less reading fails (`assert_pinned_reading`).
        let decoded_error = event.as_ref().is_some_and(|e| e.kind == "error");
        let fails = ws == "fail" || (ws == "read-on" && decoded_error && statusless(raw));
        match (fails, ws, reading(raw, event.as_ref(), false)) {
            (true, _, Reading::Failure { mut error, .. }) => {
                if ws == "fail" {
                    assert_eq!(case["pinned"], "fail Stream", "{name}");
                }
                assert_eq!(error.status, 502, "{name}: the generic failure keeps its status");
                if account(name) {
                    assert_eq!(error.delivery, withheld(raw), "{name}");
                    deliver_as_sent(&mut error, raw);
                    assert_eq!(error.delivery, withheld(raw), "{name}: never as sent");
                    checked[0] += 1;
                } else {
                    assert_eq!(error.delivery, Delivery::Classified, "{name}");
                    deliver_as_sent(&mut error, raw);
                    assert_eq!(
                        error.delivered_event().map(|b| b.as_ref()),
                        Some(raw.as_bytes()),
                        "{name}"
                    );
                    checked[1] += 1;
                }
            }
            (false, "skip", Reading::Passed { decoded: false }) => {
                assert_eq!(account_shaped(raw, false), account(name), "{name}");
                checked[if account(name) { 2 } else { 3 }] += 1;
            }
            (_, ws, _) => panic!("{name}: pinned {ws}, read otherwise"),
        }
    }
    assert_eq!(checked, [9, 4, 5, 3]);
    // The projections are read, never delivered: the frame itself is unchanged, and an
    // accepted slot keeps the classification view's reading alone.
    let accepted = r#"{"type":"response.failed","response":{"id":"r1","error":["rate_limit_error","rate_limit_exceeded","limited",null,null,null]}}"#;
    assert!(crate::codex_event::safety_views(accepted).is_empty());
    assert!(crate::codex_event::safety_views(r#"{"type":"response.completed","response":{"id":"r"}}"#).is_empty());
    let views = crate::codex_event::safety_views(
        r#"{"type":"response.failed","response":{"id":"r1","error":["rate_limit_error",{"$serde_json::private::RawValue":"\"rate_limit_exceeded\""},"m"]}}"#,
    );
    assert_eq!(
        views,
        [
            r#"{"type":"response.failed","response":{"id":"r1","error":{"type":"rate_limit_error","code":"rate_limit_exceeded","message":"m"}}}"#
        ]
    );
}

/// #6 (review finding 2): the pinned wrapped mapping keeps an explicit `status: 400` for
/// the scheduler, while the frame's own error is a rate limit carrying the credential's
/// token. The failure stays 400 and the frame is withheld, in every representation that
/// reads that way; a 400 whose error reads as no account failure still reaches the client
/// as sent.
#[test]
fn explicit_status_account_errors_keep_their_status_and_are_withheld() {
    for case in pinned_readings() {
        let name = case["name"].as_str().unwrap();
        if !name.starts_with("conflict-") {
            continue;
        }
        let raw = case["raw"].as_str().unwrap();
        let event = crate::codex_event::decode(raw);
        if name == "conflict-raw-code-array" {
            // Neither pinned struct reads it (its `error.code` is a private raw value), so
            // the pinned reader skips it (review finding 2): it stays skipped, and its
            // wrapped slots, read by position, withhold it.
            assert_eq!(case["ws"], "skip");
            assert!(
                matches!(reading(raw, None, false), Reading::Passed { decoded: false }),
                "{name}"
            );
            assert!(account_shaped(raw, false), "{name}");
            continue;
        }
        match reading(raw, event.as_ref(), false) {
            Reading::Failure { mut error, .. } => {
                assert_eq!(error.status, 400, "{name}");
                assert_eq!(error.delivery, withheld(raw), "{name}");
                deliver_as_sent(&mut error, raw);
                assert_eq!(error.delivery, withheld(raw), "{name}");
            }
            Reading::Passed { .. } | Reading::Event { .. } => panic!("{name}"),
        }
    }
    // A decoded failure keeps the pinned last-key reading: a rate-limit `type` the
    // `Value` replaces with a later one is no account failure.
    let first = r#"{"type":"response.failed","response":{"id":"r1","error":{"type":"rate_limit_error","type":"invalid_request_error","message":"bad input"}}}"#;
    let event = crate::codex_event::decode(first).unwrap();
    let Reading::Failure { error, .. } = reading(first, Some(&event), false) else {
        panic!("a failure");
    };
    assert_eq!((error.status, error.delivery), (400, Delivery::Classified));
    let plain = r#"{"type":"error","status":400,"error":{"type":"invalid_request_error","code":"invalid_value","message":"bad"}}"#;
    let event = crate::codex_event::decode(plain).unwrap();
    let Reading::Failure { mut error, .. } = reading(plain, Some(&event), false) else {
        panic!("a failure");
    };
    assert_eq!(error.delivery, Delivery::Classified);
    deliver_as_sent(&mut error, plain);
    assert_eq!(error.delivered_event().map(|b| b.as_ref()), Some(plain.as_bytes()));
}

/// #25 (review finding 3): an explicit zero status is a status. With a private raw value
/// as `error.code` the frame is no pinned wrapped event, the pinned `error` branch reads
/// its logical code `x` and goes on; so does the proxy, as for the same frame with the
/// code resolved. A logical rate limit is withheld without failing; a logical terminal
/// provider code still fails; only a missing or `null` status keeps the status-less
/// failure reading.
#[test]
fn explicit_zero_status_is_a_notice() {
    let frame = |status: &str, code: &str| {
        let status = if status.is_empty() {
            String::new()
        } else {
            format!(r#""status":{status},"#)
        };
        format!(
            r#"{{"type":"error",{status}"error":{{"code":{{"$serde_json::private::RawValue":"\"{code}\""}},"message":"notice"}}}}"#
        )
    };
    let read = |raw: &str| {
        let event = crate::codex_event::decode(raw).unwrap();
        match reading(raw, Some(&event), false) {
            Reading::Failure { error, .. } => Some(error.status),
            Reading::Passed { decoded: true } => None,
            _ => panic!("{raw}"),
        }
    };
    for status in [
        "0",
        "-0",
        "-1",
        "70000",
        "\"0\"",
        "0.0",
        "[]",
        r#"{"$serde_json::private::Number":"0"}"#,
    ] {
        assert_eq!(read(&frame(status, "x")), None, "{status}");
        let limited = frame(status, "rate_limit_exceeded");
        assert_eq!(read(&limited), None, "{status}");
        assert!(account_shaped(&limited, true), "{status}");
        assert!(read(&frame(status, "budget_reached")).is_some(), "{status}");
    }
    assert!(crate::codex_event::status_present(r#"{"status_code":0}"#));
    assert!(!crate::codex_event::status_present(
        r#"{"status":null,"status_code":null}"#
    ));
    for status in ["", "null", r#"{"$serde_json::private::RawValue":"null"}"#] {
        assert_eq!(read(&frame(status, "x")), Some(502), "{status}: status-less");
    }
}

/// #6 by the pinned client's failure kinds (`sse/responses_error.rs:48-103`,
/// `api_bridge.rs:136-140,212-255` at 44ccf394573c): each account code or type is read in
/// the error objects a reading of the frame's type reads, and nowhere else. Generated
/// frames.
#[test]
fn pinned_account_kinds_follow_the_slots_each_frame_type_reads() {
    for code in super::PINNED_ACCOUNT_CODES {
        for frame in [
            format!(r#"{{"type":"response.failed","response":{{"error":{{"code":"{code}"}}}}}}"#),
            format!(r#"{{"type":"response.failed","error":{{"code":"{code}"}}}}"#),
            format!(r#"{{"type":"error","error":{{"code":"{code}"}}}}"#),
            format!(r#"{{"type":"error","status":503,"body":{{"error":{{"code":"{code}"}}}}}}"#),
        ] {
            assert!(super::pinned_account_kind(&frame), "{frame}");
            assert!(super::account_sensitive(502, &frame), "{frame}");
        }
        for frame in [
            // A slot no reading of that type reads, or no failure frame at all.
            format!(r#"{{"type":"error","response":{{"error":{{"code":"{code}"}}}}}}"#),
            format!(r#"{{"type":"response.output_text.delta","error":{{"code":"{code}"}}}}"#),
            format!(r#"{{"type":"response.failed","response":{{"error":{{"message":"{code}"}}}}}}"#),
        ] {
            assert!(!super::pinned_account_kind(&frame), "{frame}");
        }
    }
    for kind in super::PINNED_ACCOUNT_TYPES {
        let frame = format!(r#"{{"type":"error","status":429,"error":{{"type":"{kind}"}}}}"#);
        assert!(super::pinned_account_kind(&frame), "{frame}");
    }
    for code in [
        "invalid_prompt",
        "context_length_exceeded",
        "server_is_overloaded",
        "x",
        "budget_reached",
    ] {
        let frame = format!(r#"{{"type":"response.failed","response":{{"error":{{"code":"{code}"}}}}}}"#);
        assert!(!super::pinned_account_kind(&frame), "{frame}");
    }
}

/// #25 R3: a loss belongs to one socket, and the downstream handler takes each socket's
/// loss once. A loss it took never fires again, nor counts as pending; a replacement
/// socket starts without a loss and reports its own; a late notice from a replaced
/// socket's reader reports nothing; and a dial that starts supersedes the earlier loss
/// even when that dial fails. A subscription made later still finds the latest
/// socket's loss.
#[tokio::test]
async fn each_socket_reports_its_own_loss_once() {
    async fn quiet(losses: &mut SessionLosses) -> bool {
        tokio::time::timeout(Duration::from_millis(50), losses.next())
            .await
            .is_err()
    }
    async fn fires(losses: &mut SessionLosses, what: &str) -> String {
        let loss = tokio::time::timeout(Duration::from_secs(2), losses.next())
            .await
            .unwrap_or_else(|_| panic!("{what}: no loss fired"));
        String::from_utf8_lossy(&loss.error.body).into_owned()
    }
    let lost = |text: &str| transport(text);
    // The attempt serves on `id` ([`Upstream::bind`]).
    let acquire = |session: &Session, id: u64| session.acquired.store(id, std::sync::atomic::Ordering::Release);
    let pool = Pool::default();
    let session = pool.session("s");
    let mut losses = pool.losses("s");
    let shadow = losses.clone();
    let a = session.begin_socket();
    acquire(&session, a);
    assert!(shadow.pending().is_none());
    session.notify(a, &lost("a lost"));
    assert_eq!(&shadow.pending().unwrap().body[..], b"a lost", "pending until taken");
    assert_eq!(fires(&mut losses, "a").await, "a lost");
    assert!(shadow.pending().is_none(), "a taken loss is not pending");
    session.notify(a, &lost("a again"));
    assert!(quiet(&mut losses).await, "a socket reports one loss");
    // A replacement socket: a replaced socket's late notice is nobody's loss.
    let b = session.begin_socket();
    acquire(&session, b);
    session.notify(a, &lost("a late"));
    assert!(quiet(&mut losses).await, "a replaced socket reports nothing");
    assert!(shadow.pending().is_none());
    session.notify(b, &lost("b lost"));
    assert_eq!(&shadow.pending().unwrap().body[..], b"b lost");
    assert_eq!(fires(&mut losses, "the replacement's loss").await, "b lost");
    // A loss nobody took is superseded by the next dial, whether or not it succeeds.
    let c = session.begin_socket();
    session.notify(c, &lost("c lost"));
    let _d = session.begin_socket();
    assert!(shadow.pending().is_none(), "the dial superseded the earlier loss");
    assert!(quiet(&mut losses).await);
    // A new subscription finds the latest socket's loss, taken or not by another one.
    let e = session.begin_socket();
    session.notify(e, &lost("e lost"));
    let mut later = pool.losses("s");
    assert_eq!(fires(&mut later, "a later subscription").await, "e lost");
    assert_eq!(fires(&mut losses, "e").await, "e lost");
}

/// #25 R1: a loss stands in for an attempt's failure only when it is the loss of the
/// socket that attempt acquired. A new attempt owns no socket until it serves on one; a
/// socket an earlier attempt lost reports nothing more once a new attempt starts, a late
/// notice from its reader included. A live socket that served the last turn keeps its
/// own loss across a new attempt: when it closes, the connection's watcher still gets
/// it. Starting an attempt on a session that never existed creates none.
#[tokio::test]
async fn an_attempt_owns_only_the_socket_it_acquired() {
    // A live socket: the turn completed on it and keeps it.
    let close = Arc::new(tokio::sync::Notify::new());
    let (_up, url) = upstream(vec![Act::SendCloseOn(vec![CREATED, COMPLETED], close.clone())]).await;
    let executor = CodexExecutor::new().unwrap();
    let response = executor
        .execute_in_session(&credential(&url), request(BODY), &Config::default(), &session(false))
        .await
        .unwrap();
    assert!(collect(response).await.iter().all(Result::is_ok), "the turn completed");
    let mut losses = executor.session_losses("conn-1");
    executor.begin_session_attempt("conn-1");
    assert!(losses.pending().is_none());
    close.notify_one();
    let loss = tokio::time::timeout(Duration::from_secs(5), losses.next()).await;
    let loss = loss.expect("a live socket's later loss still reaches the watcher");
    assert!(!loss.acquired, "not the new attempt's socket: no failure of its turn");
    assert!(
        losses.pending().is_none(),
        "a socket the new attempt did not acquire is not its failure"
    );
    // An attempt that served on a socket owns its loss.
    let pool = Pool::default();
    let session = pool.session("s");
    let mut watch = pool.losses("s");
    let acquire = |id: u64| session.acquired.store(id, std::sync::atomic::Ordering::Release);
    let a = session.begin_socket();
    acquire(a);
    session.notify(a, &transport("a lost"));
    assert_eq!(
        watch.pending().map(|l| l.body),
        Some(Bytes::from_static(b"a lost")),
        "the attempt's own socket"
    );
    // The next attempt fails before it acquires anything: the lost socket is not its
    // failure, and its reader's late notice reports nothing.
    pool.begin_attempt("s");
    assert!(watch.pending().is_none(), "a new attempt owns no earlier loss");
    session.notify(a, &transport("a late"));
    assert!(watch.pending().is_none());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), watch.next())
            .await
            .is_err(),
        "a lost socket's late notice never fires once a new attempt started"
    );
    // A socket dialed but not yet served on: its loss reaches the watcher, not the
    // attempt's failure.
    let b = session.begin_socket();
    session.notify(b, &transport("b lost"));
    assert!(watch.pending().is_none(), "not acquired");
    let b_loss = tokio::time::timeout(Duration::from_secs(2), watch.next())
        .await
        .expect("the latest socket's loss still fires");
    assert!(!b_loss.acquired, "a socket not served on is no attempt's failure");
    pool.begin_attempt("never-made");
    assert!(
        pool.get("never-made").is_none(),
        "starting an attempt creates no session"
    );
}

/// N1 #111 W1: only the tungstenite refusals returned before the frame is queued prove
/// that nothing was written. A reset, an I/O error or a protocol error can follow a
/// partial write (`ConnectionClosed` included: tungstenite maps a reset to it).
#[test]
fn only_refusals_before_queueing_prove_a_write_unsent() {
    use tokio_tungstenite::tungstenite::{Error, Message, error::ProtocolError};
    for unsent in [
        Error::AlreadyClosed,
        Error::Protocol(ProtocolError::SendAfterClosing),
        Error::WriteBufferFull(Box::new(Message::text("x"))),
    ] {
        assert!(super::refused_before_queueing(&unsent), "{unsent}");
    }
    for ambiguous in [
        Error::ConnectionClosed,
        Error::Io(std::io::Error::from(std::io::ErrorKind::BrokenPipe)),
        Error::Io(std::io::Error::from(std::io::ErrorKind::ConnectionReset)),
        Error::Protocol(ProtocolError::ResetWithoutClosingHandshake),
    ] {
        assert!(!super::refused_before_queueing(&ambiguous), "{ambiguous}");
    }
}

/// N1 #111 W1: a socket whose loss the reader already recorded is not written at all,
/// so the write provably sent nothing; the failure keeps the loss's classification.
#[tokio::test]
async fn a_socket_whose_loss_was_recorded_is_not_written() {
    let (up, url) = upstream(vec![]).await;
    let executor = CodexExecutor::new().unwrap();
    let conn = pooled(&executor, &url).await;
    conn.link.lock().unwrap().lost = Some(transport("codex websockets executor: stale socket"));
    let failure = conn
        .send_create(Bytes::from_static(br#"{"type":"response.create"}"#), None)
        .await
        .expect_err("a lost socket takes no write");
    assert!(failure.unsent);
    assert_eq!(failure.error.scope, FailureScope::Transport);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(up.frames.lock().unwrap().is_empty(), "nothing reached the upstream");
}

/// N1 #111 W1: a failure the upstream may have caused after reading the request is
/// request-scoped (no failover, no cooldown) and keeps its status, body and diagnostic;
/// one that is request-scoped already stays as it is.
#[test]
fn an_uncertain_loss_is_request_scoped_and_otherwise_unchanged() {
    let lost = transport("websocket: close 1011 (internal server error): bye");
    let error = super::uncertain(lost.clone());
    assert_eq!((error.status, error.scope), (lost.status, FailureScope::Request));
    assert_eq!(error.body, lost.body);
    assert!(error.headers.is_empty());
    let too_big = message_too_big();
    assert_eq!(super::uncertain(too_big.clone()).body, too_big.body);
}

// ------------------------------------------------------------------ N1 #111 review: write admission

fn test_refusal() -> Refusal {
    Refusal {
        message: "Invalid API key".into(),
        event: Bytes::from_static(br#"{"type":"error","status":401,"error":{"message":"Invalid API key"}}"#),
    }
}

/// One scripted step of [`MockSink`] (ready when the script is empty): an error, pending
/// and woken at once (after running `on_pending`), or pending forever.
enum Step {
    Fail(tokio_tungstenite::tungstenite::Error),
    Pending,
    Stall,
}

/// A sink whose readiness, hand-over and flush follow a script, for the phases of
/// [`super::write_phases`] that a loopback socket cannot be made to show on demand.
#[derive(Default)]
struct MockSink {
    ready: VecDeque<Step>,
    start: Option<tokio_tungstenite::tungstenite::Error>,
    flush: VecDeque<Step>,
    started: usize,
    on_pending: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl MockSink {
    fn step(
        steps: &mut VecDeque<Step>,
        on_pending: &Option<Arc<std::sync::atomic::AtomicBool>>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), tokio_tungstenite::tungstenite::Error>> {
        use std::task::Poll;
        match steps.pop_front() {
            None => Poll::Ready(Ok(())),
            Some(Step::Fail(error)) => Poll::Ready(Err(error)),
            Some(Step::Pending) => {
                if let Some(flag) = on_pending {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Some(Step::Stall) => {
                steps.push_front(Step::Stall);
                Poll::Pending
            }
        }
    }
}

impl futures_util::Sink<tokio_tungstenite::tungstenite::Message> for MockSink {
    type Error = tokio_tungstenite::tungstenite::Error;

    fn poll_ready(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        let this = self.get_mut();
        Self::step(&mut this.ready, &this.on_pending, cx)
    }

    fn start_send(
        self: std::pin::Pin<&mut Self>,
        _: tokio_tungstenite::tungstenite::Message,
    ) -> Result<(), Self::Error> {
        let this = self.get_mut();
        this.started += 1;
        this.start.take().map_or(Ok(()), Err)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        let this = self.get_mut();
        Self::step(&mut this.flush, &this.on_pending, cx)
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }
}

/// N1 #111 review: each phase of a write, injected. A failure before the hand-over
/// (readiness, or a timeout while waiting for it) wrote nothing of the frame; a refusal
/// of the hand-over itself before queueing too; a failed or stalled flush after the
/// hand-over may have written part of it. The client authority is asked only once the
/// sink is ready: a key revoked while the write waited is refused and nothing is handed
/// over, while a revocation after the hand-over cannot recall the frame.
#[tokio::test]
async fn each_write_phase_reports_whether_the_frame_left() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio_tungstenite::tungstenite::{Error, Message, error::ProtocolError};
    let io = || Error::Io(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
    let frame = || Message::text("frame");

    // Readiness fails: nothing handed over, the authority never asked.
    let asked = Arc::new(AtomicUsize::new(0));
    let counting = {
        let asked = asked.clone();
        move || {
            asked.fetch_add(1, Ordering::SeqCst);
            None
        }
    };
    let mut sink = MockSink {
        ready: VecDeque::from([Step::Fail(io())]),
        ..MockSink::default()
    };
    let mut queued = false;
    let failed = super::write_phases(&mut sink, frame(), Some(&counting), &mut queued).await;
    assert!(matches!(failed, Err((_, true))), "{failed:?}");
    assert_eq!((sink.started, queued, asked.load(Ordering::SeqCst)), (0, false, 0));

    // Revoked while waiting for readiness: refused, nothing handed over.
    let revoked = Arc::new(AtomicBool::new(false));
    let authority = {
        let revoked = revoked.clone();
        move || revoked.load(Ordering::SeqCst).then(test_refusal)
    };
    let mut sink = MockSink {
        ready: VecDeque::from([Step::Pending]),
        on_pending: Some(revoked.clone()),
        ..MockSink::default()
    };
    let mut queued = false;
    let refused = super::write_phases(&mut sink, frame(), Some(&authority), &mut queued).await;
    assert_eq!(refused.unwrap(), Some(test_refusal()));
    assert_eq!((sink.started, queued), (0, false));

    // The hand-over refused before queueing, or failing after it may have written.
    for (error, unsent) in [
        (Error::AlreadyClosed, true),
        (Error::Protocol(ProtocolError::SendAfterClosing), true),
        (Error::WriteBufferFull(Box::new(Message::text("x"))), true),
        (Error::ConnectionClosed, false),
        (io(), false),
    ] {
        let name = error.to_string();
        let mut sink = MockSink {
            start: Some(error),
            ..MockSink::default()
        };
        let mut queued = false;
        let failed = super::write_phases(&mut sink, frame(), None, &mut queued).await;
        assert!(matches!(failed, Err((_, u)) if u == unsent), "{name}: {failed:?}");
        assert_eq!((sink.started, queued), (1, true), "{name}");
    }

    // The flush fails after the hand-over: ambiguous.
    let mut sink = MockSink {
        flush: VecDeque::from([Step::Fail(io())]),
        ..MockSink::default()
    };
    let mut queued = false;
    let failed = super::write_phases(&mut sink, frame(), None, &mut queued).await;
    assert!(matches!(failed, Err((_, false))), "{failed:?}");

    // Revoked while the handed-over frame flushes: the frame is committed and sent.
    let revoked = Arc::new(AtomicBool::new(false));
    let authority = {
        let revoked = revoked.clone();
        move || revoked.load(Ordering::SeqCst).then(test_refusal)
    };
    let mut sink = MockSink {
        flush: VecDeque::from([Step::Pending]),
        on_pending: Some(revoked.clone()),
        ..MockSink::default()
    };
    let mut queued = false;
    let sent = super::write_phases(&mut sink, frame(), Some(&authority), &mut queued).await;
    assert_eq!(sent.unwrap(), None);
    assert!(revoked.load(Ordering::SeqCst) && sink.started == 1);

    // A timeout while waiting for readiness wrote nothing; one during the flush may have.
    for (stalled_ready, queued_at_timeout) in [(true, false), (false, true)] {
        let mut sink = MockSink::default();
        if stalled_ready {
            sink.ready.push_back(Step::Stall);
        } else {
            sink.flush.push_back(Step::Stall);
        }
        let mut queued = false;
        let timed_out = tokio::time::timeout(
            Duration::from_millis(20),
            super::write_phases(&mut sink, frame(), None, &mut queued),
        )
        .await;
        assert!(timed_out.is_err());
        assert_eq!(queued, queued_at_timeout, "stalled_ready={stalled_ready}");
    }
}

/// N1 #111 review: a turn's create that waits for the socket (here: another holder of
/// its sink) is admitted only once it can be written. A key revoked meanwhile stops it:
/// nothing is written, the turn fails with the refusal, and the socket serves the next
/// turn once the key is accepted again. Deterministic: the test holds the sink until the
/// write is parked on it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_create_waiting_for_the_socket_is_refused_after_revocation() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let (up, url) = upstream(vec![
        Act::Send(vec![CREATED, COMPLETED]),
        Act::Send(vec![CREATED, COMPLETED]),
    ])
    .await;
    let executor = Arc::new(CodexExecutor::new().unwrap());
    let first = executor
        .execute_in_session(&credential(&url), request(BODY), &Config::default(), &session(false))
        .await
        .unwrap();
    assert!(collect(first).await.iter().all(Result::is_ok));
    let conn = executor.ws.session("conn-1").current().unwrap();
    let revoked = Arc::new(AtomicBool::new(false));
    executor.attach_authority(
        "conn-1",
        Arc::new({
            let revoked = revoked.clone();
            move || revoked.load(Ordering::SeqCst).then(test_refusal)
        }),
    );
    let mut waits = conn.sink_waits.subscribe();
    let before = *waits.borrow_and_update();
    let sink = conn.sink.lock().await;
    let second = tokio::spawn({
        let (executor, url) = (executor.clone(), url.clone());
        async move {
            executor
                .execute_in_session(&credential(&url), request(BODY), &Config::default(), &session(false))
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(5), waits.wait_for(|n| *n > before))
        .await
        .expect("the create parks on the sink")
        .unwrap();
    revoked.store(true, Ordering::SeqCst);
    drop(sink);
    let refused = tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .unwrap()
        .unwrap()
        .err()
        .expect("the create is refused");
    assert_eq!((refused.status, refused.scope), (401, FailureScope::Request));
    assert_eq!(refused.delivered_event(), Some(&test_refusal().event));

    revoked.store(false, Ordering::SeqCst);
    let third = executor
        .execute_in_session(&credential(&url), request(BODY), &Config::default(), &session(false))
        .await
        .unwrap();
    assert!(collect(third).await.iter().all(Result::is_ok));
    assert!(
        Arc::ptr_eq(&executor.ws.session("conn-1").current().unwrap(), &conn),
        "the same socket"
    );
    assert_eq!(
        up.frames.lock().unwrap().len(),
        2,
        "only the two accepted creates were written"
    );
}

/// N1 #111 review: an interrupt that waits for the socket is admitted only once it can be
/// written. A key revoked meanwhile stops it: nothing is written, the caller gets the
/// refusal, and the acknowledgement it would have waited for is not left registered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_interrupt_waiting_for_the_socket_is_refused_after_revocation() {
    use std::sync::atomic::{AtomicBool, Ordering};
    const REFUSED: &str = r#"{"type":"response.interrupt","response_id":"r-refused","mode":"discard_partial_items"}"#;
    let (up, url) = upstream(vec![Act::Send(vec![CREATED])]).await;
    let executor = Arc::new(CodexExecutor::new().unwrap());
    let response = executor
        .execute_in_session(&credential(&url), request(BODY), &Config::default(), &session(false))
        .await
        .unwrap();
    let _ = received(&up, 1).await;
    let conn = executor.ws.session("conn-1").current().unwrap();
    let revoked = Arc::new(AtomicBool::new(false));
    executor.attach_authority(
        "conn-1",
        Arc::new({
            let revoked = revoked.clone();
            move || revoked.load(Ordering::SeqCst).then(test_refusal)
        }),
    );
    let mut waits = conn.sink_waits.subscribe();
    let before = *waits.borrow_and_update();
    let sink = conn.sink.lock().await;
    let interrupt = tokio::spawn({
        let executor = executor.clone();
        async move { executor.interrupt_session("conn-1", REFUSED.as_bytes(), |_| true).await }
    });
    tokio::time::timeout(Duration::from_secs(5), waits.wait_for(|n| *n > before))
        .await
        .expect("the interrupt parks on the sink")
        .unwrap();
    revoked.store(true, Ordering::SeqCst);
    drop(sink);
    let outcome = tokio::time::timeout(Duration::from_secs(5), interrupt)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome, Err(InterruptError::Refused(test_refusal())));
    assert!(!conn.link.lock().unwrap().awaited.contains("r-refused"));

    revoked.store(false, Ordering::SeqCst);
    executor
        .interrupt_session("conn-1", INTERRUPT_R1.as_bytes(), |_| true)
        .await
        .unwrap();
    let frames = received(&up, 2).await;
    assert_eq!(frames.len(), 2, "{frames:?}");
    assert_eq!(frames[1], INTERRUPT_R1, "only the accepted interrupt was written");
    drop(response);
}

/// N1 #111 review, W2 on the WebSocket HTTP fallback: an attempt is checked before it
/// dials and again when the HTTP client takes its body, which the client does only once
/// it holds a connection. A key revoked between the two (here: during the dial, as the
/// authority itself records) stops the attempt: the connection was made, no request byte
/// reached it, and the turn fails with the refusal instead of failing over.
/// Deterministic: the authority allows its first check and refuses every later one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_revoked_while_an_http_turn_dials_sends_no_request() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let accepted = Arc::new(AtomicUsize::new(0));
    let received = Arc::new(Mutex::new(Vec::<u8>::new()));
    let closed = Arc::new(tokio::sync::Notify::new());
    tokio::spawn({
        let (accepted, received, closed) = (accepted.clone(), received.clone(), closed.clone());
        async move {
            while let Ok((mut conn, _)) = listener.accept().await {
                accepted.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0; 4096];
                loop {
                    match conn.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => received.lock().unwrap().extend_from_slice(&buf[..n]),
                    }
                }
                closed.notify_one();
            }
        }
    });
    let executor = CodexExecutor::new().unwrap();
    let checks = Arc::new(AtomicUsize::new(0));
    executor.attach_authority(
        "conn-1",
        Arc::new({
            let checks = checks.clone();
            move || (checks.fetch_add(1, Ordering::SeqCst) >= 1).then(test_refusal)
        }),
    );
    let mut cred = credential(&url);
    cred.attributes.remove("websockets");
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        executor.execute_in_session(&cred, request(BODY), &Config::default(), &session(false)),
    )
    .await
    .expect("the attempt ends");
    let refused = outcome.err().expect("the attempt is refused");
    assert_eq!(
        (refused.status, refused.scope),
        (401, FailureScope::Request),
        "{refused}"
    );
    assert_eq!(refused.delivered_event(), Some(&test_refusal().event));
    assert_eq!(checks.load(Ordering::SeqCst), 2, "before the dial, and at the body");
    tokio::time::timeout(Duration::from_secs(5), closed.notified())
        .await
        .expect("the abandoned connection closes");
    assert_eq!(accepted.load(Ordering::SeqCst), 1, "the attempt dialled");
    let received = received.lock().unwrap().clone();
    eprintln!(
        "dial-window evidence: {} bytes reached the upstream: {:?}",
        received.len(),
        String::from_utf8_lossy(&received)
    );
    // The client fails the request before it flushes anything: not even the head
    // leaves, so the upstream has no request at all.
    assert!(received.is_empty(), "no request bytes reached the upstream");
}
