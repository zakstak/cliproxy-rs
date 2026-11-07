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

fn request(body: &str) -> ExecRequest {
    let body = Bytes::from(body.to_owned());
    ExecRequest {
        operation: Operation::Generate,
        source_format: Format::OpenAIResponse,
        response_format: Format::OpenAIResponse,
        stream_payloads: false,
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
    let closed = executor.session_closed("conn-1");
    assert!(
        tokio::time::timeout(Duration::from_millis(200), closed).await.is_err(),
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
    let closed = executor.session_closed("conn-1");
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
    let notified = tokio::time::timeout(Duration::from_secs(2), closed)
        .await
        .expect("session notified");
    assert_eq!(notified.status, 413);
    assert_eq!(
        gjson::get(&String::from_utf8_lossy(&notified.body), "error.message").str(),
        "bye"
    );
    assert_eq!(executor.session_loss("conn-1").map(|e| e.body), Some(notified.body));
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
    assert_eq!(error.scope, FailureScope::Transport, "{error}");
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
    assert_eq!(error.scope, FailureScope::Transport);
    assert!(String::from_utf8_lossy(&error.body).contains("idle timeout"), "{error}");
}

/// A socket whose upstream stopped reading (half-open) fails the write after the write
/// bound instead of holding the turn until the read deadline; the one retry on a fresh
/// socket is bounded the same way.
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
    assert_eq!(error.scope, FailureScope::Transport);
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
        2,
        "one retry on a fresh socket"
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

/// A reader that fails before the first turn activates (binary message right after the
/// handshake) must fail that turn instead of leaving it waiting on a dead socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reader_failure_before_the_turn_activates_fails_the_turn() {
    for _ in 0..20 {
        let (up, url) = upstream(vec![]).await;
        *up.binary_first.lock().unwrap() = true;
        let executor = CodexExecutor::new().unwrap();
        let (cred, cfg, sess) = (credential(&url), Config::default(), session(false));
        let turn = executor.execute_in_session(&cred, request(BODY), &cfg, &sess);
        let outcome = tokio::time::timeout(Duration::from_secs(2), async {
            match turn.await {
                Ok(response) => collect(response).await.into_iter().find_map(Result::err),
                Err(error) => Some(error),
            }
        })
        .await
        .expect("the turn must not hang on a dead socket");
        let error = outcome.expect("the turn fails");
        assert_eq!(error.scope, FailureScope::Transport, "{error}");
    }
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
    let closed = executor.session_closed("conn-1");
    let response = executor
        .execute_in_session(&credential(&url), request(BODY), &Config::default(), &session(false))
        .await
        .unwrap();
    // Nothing reads the turn until the reader has filled the channel and seen the close.
    tokio::time::timeout(Duration::from_secs(5), closed)
        .await
        .expect("session notified");
    let events = collect(response).await;
    assert_eq!(events.len(), TURN_BUFFER + 1, "every queued event, then the error");
    let error = events.last().unwrap().as_ref().expect_err("turn ends with the close");
    assert_eq!(error.status, 413);
    assert_eq!(
        gjson::get(&String::from_utf8_lossy(&error.body), "error.message").str(),
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
        gjson::get(&String::from_utf8_lossy(&created), "type").str(),
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
        assert_eq!(gjson::get(body, "type").str(), "response.create", "the frame as sent");
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
    assert_eq!(error.scope, FailureScope::Transport);
    assert_eq!(text, "websocket: close 1011 (internal server error): bye");
    assert_eq!(cause, "remote_close: code 1011");

    // gorilla's text for a peer that vanished is close 1006, which no peer can send:
    // the cause says it was an end of stream, not a close frame.
    let (error, text, cause) = read_failure(vec![Act::SendDrop(vec![CREATED])], None).await;
    assert_eq!(error.scope, FailureScope::Transport);
    assert_eq!(text, "websocket: close 1006 (abnormal closure): unexpected EOF");
    assert!(cause.starts_with("eof"), "{cause}");

    let (error, text, cause) = read_failure(
        vec![Act::Pings(Duration::from_millis(50), 40)],
        Some(Duration::from_millis(300)),
    )
    .await;
    assert_eq!(error.scope, FailureScope::Transport);
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
/// while the account check runs. The interrupt is then done, not forwarded: the socket's
/// state is read after the check.
#[tokio::test]
async fn interrupt_for_a_response_finished_during_the_auth_check_is_done() {
    let (up, url) = upstream(vec![Act::Send(vec![CREATED, COMPLETED])]).await;
    let executor = CodexExecutor::new().unwrap();
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
    executor
        .interrupt_session("conn-1", br#"{"type":"response.interrupt","response_id":"r1"}"#, |_| {
            finish()
        })
        .await
        .unwrap();
    conn.send("barrier".into()).await.unwrap();
    assert_eq!(received(&up, 2).await, [r#"{"type":"response.create"}"#, "barrier"]);
}

/// Go `TestInterruptExecutionSessionAfterTerminal`: a late interrupt for a finished
/// response is done, also while the next response runs, and never reaches upstream; an
/// unknown ID on an idle socket falls through to the HTTP fallback; after the socket is
/// lost there is no active upstream.
#[tokio::test]
async fn late_interrupts_for_finished_responses_are_done() {
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
            Act::Send(vec![r#"{"type":"response.incomplete","response":{"id":"r2"}}"#]),
            Act::SendClose(vec![], 1000),
        ])
        .await;
        let executor = CodexExecutor::new().unwrap();
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
            assert_eq!(send("r1").await, Ok(()), "{terminal}: finished r1");
        }
        assert_eq!(
            send("unknown").await,
            Err(InterruptError::NoActive),
            "{terminal}: unknown"
        );

        let mut reads = conn.activate(false);
        assert_eq!(send("r1").await, Ok(()), "{terminal}: r1 while r2 runs");
        assert_eq!(send("r2").await, Ok(()), "{terminal}: active r2");
        read_text(&mut reads).await;
        conn.deactivate();
        for id in ["r1", "r2"] {
            assert_eq!(send(id).await, Ok(()), "{terminal}: finished {id}");
        }
        assert_eq!(
            received(&up, 2).await,
            [r#"{"type":"response.create"}"#.to_owned(), interrupt("r2")],
            "{terminal}: only the active response's interrupt reached upstream"
        );
        conn.send("bye".into()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), executor.ws.closed("conn-1"))
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
        let reported = tokio::time::timeout(Duration::from_millis(200), executor.ws.closed("conn-1"))
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
/// closes the socket right after the event, the same holds. Other clients get only
/// Go's classified error.
#[tokio::test]
async fn live_context_window_error_reaches_a_codex_client_as_a_failure() {
    let live = LIVE_CONTEXT_ERROR.trim();
    let normalized = cpa_common::codex_client::context_window_failure_event(live)
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
            gjson::get(&String::from_utf8_lossy(&e.body), "error.code")
                .str()
                .to_owned()
        };
        assert_eq!(code(error), "context_too_large", "{name}");
        // A Codex client's turn ends its connection after the event itself: no loss is
        // reported, not even a close right after the event. For other clients the
        // classified error, or such a close, is the loss, as in Go.
        if codex_client {
            assert!(
                executor.ws.loss("conn-1").is_none(),
                "{name}: a loss was reported: {:?}",
                executor.ws.loss("conn-1").map(|l| code(&l))
            );
        } else {
            tokio::time::timeout(Duration::from_secs(5), executor.session_closed("conn-1"))
                .await
                .unwrap_or_else(|_| panic!("{name}: the session's loss was not reported"));
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
        let loss = tokio::time::timeout(Duration::from_secs(5), executor.session_closed("conn-1")).await;
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
