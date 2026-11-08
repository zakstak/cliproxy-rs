//! The native Codex WebSocket route through the real router (#24, the WebSocket half of
//! #25): a Responses client's `response.create` frames reach the upstream socket byte for
//! byte, with only `model` routed, and the upstream's frames reach the client byte for
//! byte (`docs/codex/CONTRACT.md` "Native WebSocket route").
//!
//! The frames come from the #21 contract fixtures (`cpa_exec::codex_contract`). Two
//! request frames are derived from HTTP fixtures by adding the `"type":"response.create"`
//! tag at the front, the WebSocket envelope of `codex-api/src/common.rs:397-403`; the
//! derivation is the only difference. Upstream events reuse the SSE fixtures' `data`
//! payloads as WebSocket text frames: the pinned client decodes both transports with the
//! same `ResponsesStreamEvent` (`codex-api/src/endpoint/responses_websocket.rs:919-975`).
//!
//! Loopback capture upstreams, fake tokens and a private `auth-dir` only.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_exec::Executors;
use cpa_exec::codex_contract::fixture;
use cpa_server::testing::codex_capture::{CaptureUpstream, Script};
use futures_util::StreamExt;
use wreq::ws::WebSocket;
use wreq::ws::message::Message;

const CODEX_UA: &str = "codex_cli_rs/0.160.0 (Linux; x86_64) xterm-256color";
const BETA: &str = "responses_websockets=2026-02-06";

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("127.0.0.1:{}", addr.port())
}

/// A private, empty `auth-dir`, removed when dropped: credential loading never falls back
/// to `~/.cli-proxy-api`.
struct AuthDir(std::path::PathBuf);

impl AuthDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("cpa-it-auth-{}", uuid::Uuid::new_v4()));
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
enum Profile {
    /// A ChatGPT OAuth token.
    Chatgpt,
    /// An API key on a base URL other than `api.openai.com` (`compatible`), the profile
    /// a loopback upstream can stand for.
    Compatible,
}

/// A Codex auth file pointing at `base_url`. `websockets` is the credential's own
/// attribute: without it the WebSocket route runs the turn over HTTP.
fn credential(profile: Profile, base_url: &str, websockets: bool) -> Credential {
    let metadata = serde_json::json!({
        "type": "codex",
        "access_token": "at-FAKE",
        "account_id": "acct-FAKE",
        "expired": "2099-01-01T00:00:00Z",
    });
    let mut credential = Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/codex.json"),
        metadata.as_object().unwrap().clone(),
    )
    .unwrap();
    for (key, value) in [("base_url", base_url), ("plan_type", "plus")] {
        credential.attributes.insert(key.into(), value.into());
    }
    if profile == Profile::Compatible {
        credential.attributes.insert("api_key".into(), "key-FAKE".into());
    }
    if websockets {
        credential.attributes.insert("websockets".into(), "true".into());
    }
    credential
}

struct Proxy {
    addr: String,
    upstream: CaptureUpstream,
    _auth_dir: AuthDir,
}

/// A proxy whose Codex credentials (from `credentials`, given the upstream URL) point at a
/// capture upstream answering `script`. `extra_config` is appended to the config.
async fn proxy_with(script: Script, extra_config: &str, credentials: impl FnOnce(&str) -> Vec<Credential>) -> Proxy {
    let upstream = CaptureUpstream::start(script).await;
    let auth_dir = AuthDir::new();
    let cfg = Config::parse(&format!(
        "auth-dir: {}\naccess:\n  api-keys: [client-key]\n{extra_config}",
        auth_dir.0.display()
    ))
    .unwrap();
    let mut all = cpa_core::config::credentials::from_config(&cfg);
    all.extend(credentials(&upstream.base_url));
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        meta: Default::default(),
        xai: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(cfg, all, executors));
    Proxy {
        addr: serve(cpa_server::router(rt)).await,
        upstream,
        _auth_dir: auth_dir,
    }
}

async fn proxy(profile: Profile, script: Script, extra_config: &str) -> Proxy {
    proxy_with(script, extra_config, |url| vec![credential(profile, url, true)]).await
}

/// A Codex client's WebSocket to the proxy.
async fn connect(proxy: &Proxy) -> WebSocket {
    wreq::Client::new()
        .websocket(format!("ws://{}/v1/responses", proxy.addr))
        .header("authorization", "Bearer client-key")
        .header("user-agent", CODEX_UA)
        .header("originator", "codex_cli_rs")
        .send()
        .await
        .unwrap()
        .into_websocket()
        .await
        .unwrap()
}

/// What the client read: text frames, then the close code if the connection ended.
#[derive(Debug, Default)]
struct Read {
    frames: Vec<String>,
    closed: Option<u16>,
}

/// Reads until a frame of a type in `until`, a close, or a quiet `idle`.
async fn read(socket: &mut WebSocket, until: &[&str]) -> Read {
    let mut read = Read::default();
    loop {
        let next = match tokio::time::timeout(Duration::from_secs(5), socket.next()).await {
            Ok(next) => next,
            Err(_) => panic!("read timed out after {read:?}"),
        };
        match next {
            Some(Ok(Message::Text(text))) => {
                let text = text.as_str().to_owned();
                let kind = gjson::get(&text, "type").str().to_owned();
                read.frames.push(text);
                if until.contains(&kind.as_str()) {
                    return read;
                }
            }
            Some(Ok(Message::Close(frame))) => {
                read.closed = Some(frame.map_or(1005, |f| u16::from(f.code)));
                return read;
            }
            Some(Ok(_)) => {}
            Some(Err(_)) | None => {
                read.closed = Some(1006);
                return read;
            }
        }
    }
}

const TERMINAL: [&str; 3] = ["response.completed", "response.incomplete", "response.failed"];

fn text(bytes: Bytes) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// A fixture frame with the test model in place of `gpt-test`: the client's frame.
fn client_frame(name: &str, model: &str) -> String {
    text(fixture(name).bytes()).replacen(r#""model":"gpt-test""#, &format!(r#""model":"{model}""#), 1)
}

/// An HTTP fixture body as a WebSocket `response.create`: the type tag in front.
fn as_create(name: &str, model: &str) -> String {
    let body = client_frame(name, model);
    format!(r#"{{"type":"response.create",{}"#, &body[1..])
}

fn events(name: &str) -> Vec<String> {
    fixture(name).frames().into_iter().map(text).collect()
}

fn replies(names: &[&str]) -> Vec<Vec<Bytes>> {
    names.iter().map(|name| fixture(name).frames()).collect()
}

/// The upstream socket's frames as text.
fn upstream_frames(proxy: &Proxy, socket: usize) -> Vec<String> {
    proxy.upstream.sockets()[socket]
        .frames
        .iter()
        .cloned()
        .map(text)
        .collect()
}

/// First turn, continuation, encrypted reasoning with a namespaced call, and unknown
/// fields with raw number spellings: each frame reaches the one upstream socket as the
/// client sent it, and each upstream frame reaches the client as it arrived, missing
/// usage and an incomplete answer included. Both native profiles a loopback can stand for.
#[tokio::test]
async fn native_creates_and_events_pass_through_unchanged() {
    for profile in [Profile::Chatgpt, Profile::Compatible] {
        let answers = [
            "sse-completed",
            "sse-completed",
            "sse-completed-no-usage",
            "sse-incomplete-partial",
        ];
        let proxy = proxy(
            profile,
            Script {
                ws: replies(&answers),
                ..Script::default()
            },
            "",
        )
        .await;
        let mut socket = connect(&proxy).await;
        let sent = [
            client_frame("ws-create-first", "gpt-5.5"),
            client_frame("ws-create-continuation", "gpt-5.5"),
            as_create("http-request-full", "gpt-5.5"),
            as_create("probe-request-unknown-fields", "gpt-5.5"),
        ];
        for (frame, answer) in sent.iter().zip(answers) {
            socket.send(Message::text(frame.clone())).await.unwrap();
            let read = read(&mut socket, &TERMINAL).await;
            assert_eq!(
                read.frames,
                events(answer),
                "{profile:?}: the client gets {answer} as sent"
            );
            assert_eq!(read.closed, None);
        }
        let sockets = proxy.upstream.sockets();
        assert_eq!(sockets.len(), 1, "{profile:?}: one upstream socket for the connection");
        assert_eq!(upstream_frames(&proxy, 0), sent, "{profile:?}: the client's frames");
        let handshake = &sockets[0].headers;
        let token = match profile {
            Profile::Chatgpt => "Bearer at-FAKE",
            Profile::Compatible => "Bearer key-FAKE",
        };
        assert_eq!(
            handshake["authorization"], token,
            "the client key never travels upstream"
        );
        assert_eq!(handshake["openai-beta"], BETA);
        assert!(proxy.upstream.http().is_empty(), "no HTTP request");
    }
}

/// An operator alias is the one request edit: only the `model` value changes, in place.
#[tokio::test]
async fn an_alias_routes_only_the_model() {
    // An API-key entry from config, so that the alias is the operator's.
    let upstream = CaptureUpstream::start(Script {
        ws: replies(&["sse-completed"]),
        ..Script::default()
    })
    .await;
    let auth_dir = AuthDir::new();
    let cfg = Config::parse(&format!(
        "auth-dir: {}\naccess:\n  api-keys: [client-key]\ncodex-api-key:\n  - api-key: key-FAKE\n    base-url: {}\n    websockets: true\n    models:\n      - name: gpt-upstream\n        alias: gpt-alias\n",
        auth_dir.0.display(),
        upstream.base_url
    ))
    .unwrap();
    let credentials = cpa_core::config::credentials::from_config(&cfg);
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        meta: Default::default(),
        xai: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(cfg, credentials, executors));
    let proxy = Proxy {
        addr: serve(cpa_server::router(rt)).await,
        upstream,
        _auth_dir: auth_dir,
    };
    let mut socket = connect(&proxy).await;
    let sent = client_frame("ws-create-first", "gpt-alias");
    socket.send(Message::text(sent.clone())).await.unwrap();
    let read = read(&mut socket, &TERMINAL).await;
    assert_eq!(read.frames, events("sse-completed"));
    assert_eq!(
        upstream_frames(&proxy, 0),
        [sent.replacen(r#""model":"gpt-alias""#, r#""model":"gpt-upstream""#, 1)],
        "only the model value changed"
    );
}

/// A client's prewarm (`generate: false`) is the provider's: it goes upstream as sent and
/// its continuation stays on the same socket. The proxy sends no request of its own.
#[tokio::test]
async fn a_prewarm_reaches_upstream_and_its_continuation_stays_on_the_socket() {
    let prewarm_done = r#"{"type":"response.completed","response":{"id":"resp_fixture_1","usage":null}}"#;
    let proxy = proxy(
        Profile::Chatgpt,
        Script {
            ws: vec![
                vec![
                    Bytes::from_static(br#"{"type":"response.created","response":{"id":"resp_fixture_1"}}"#),
                    Bytes::from_static(prewarm_done.as_bytes()),
                ],
                fixture("sse-completed").frames(),
            ],
            ..Script::default()
        },
        "",
    )
    .await;
    let mut socket = connect(&proxy).await;
    let prewarm = client_frame("ws-create-prewarm", "gpt-5.5");
    socket.send(Message::text(prewarm.clone())).await.unwrap();
    let warm = read(&mut socket, &TERMINAL).await;
    assert_eq!(
        warm.frames.last().map(String::as_str),
        Some(prewarm_done),
        "the provider's own prewarm answer, not a local one"
    );
    let continuation = client_frame("ws-create-continuation", "gpt-5.5");
    socket.send(Message::text(continuation.clone())).await.unwrap();
    let turn = read(&mut socket, &TERMINAL).await;
    assert_eq!(turn.frames, events("sse-completed"));
    assert_eq!(proxy.upstream.sockets().len(), 1);
    assert_eq!(
        upstream_frames(&proxy, 0),
        [prewarm, continuation],
        "nothing else was sent"
    );
}

/// The pinned client's steering: interrupt the running response, read the interrupt's
/// acknowledgement and the interrupted end, then send a new create
/// (`responses_websocket.rs:310-325`, 824-839). With and without the proxy's
/// `response-steering` duplex mode, the interrupt and the new create reach the same
/// socket as sent, and the acknowledgement events reach the client as they arrived.
#[tokio::test]
async fn interrupt_then_a_new_create_follows_the_first_party_sequence() {
    for steering in [false, true] {
        let interrupt_events = events("ws-events-interrupt");
        let config = if steering {
            "codex:\n  response-steering: true\n"
        } else {
            ""
        };
        let proxy = proxy(
            Profile::Chatgpt,
            Script {
                ws: vec![
                    // The first create: the response starts and keeps running.
                    vec![Bytes::from(interrupt_events[0].clone())],
                    // The interrupt: accepted, then the interrupted end.
                    interrupt_events[1..].iter().cloned().map(Bytes::from).collect(),
                    fixture("sse-completed").frames(),
                ],
                ..Script::default()
            },
            config,
        )
        .await;
        let mut socket = connect(&proxy).await;
        let first = client_frame("ws-create-first", "gpt-5.5");
        socket.send(Message::text(first.clone())).await.unwrap();
        let started = read(&mut socket, &["response.created"]).await;
        assert_eq!(started.frames, interrupt_events[..1], "steering={steering}");
        let interrupt = text(fixture("ws-interrupt").bytes());
        socket.send(Message::text(interrupt.clone())).await.unwrap();
        let ended = read(&mut socket, &["response.incomplete"]).await;
        assert_eq!(
            ended.frames,
            interrupt_events[1..],
            "steering={steering}: as upstream sent them"
        );
        let next = as_create("http-request-full", "gpt-5.5");
        socket.send(Message::text(next.clone())).await.unwrap();
        let turn = read(&mut socket, &TERMINAL).await;
        assert_eq!(turn.frames, events("sse-completed"), "steering={steering}");
        assert_eq!(proxy.upstream.sockets().len(), 1, "steering={steering}");
        assert_eq!(
            upstream_frames(&proxy, 0),
            [first, interrupt, next],
            "steering={steering}: the client's frames, in order"
        );
    }
}

/// Operator settings that rewrite legacy turns (payload rules, the multi-agent and orphan
/// delegation rewrites) cannot reach native frames: not the first create, not a create
/// or a steer the duplex sends while a response runs. The pinned client never sends
/// `response.steer`; OpenAI documents it for the GPT-6 family on the API
/// (`ws-steer-api-docs`), and it goes upstream as the client wrote it.
#[tokio::test]
async fn operator_rules_cannot_rewrite_native_frames() {
    let rules = "payload:\n  override:\n    - models: [{name: gpt-5.5}]\n      params: {instructions: injected, store: true}\n  filter:\n    - models: [{name: gpt-5.5}]\n      params: [prompt_cache_options, service_tier, input]\nupstream:\n  codex:\n    orphan-delegation-compatibility: true\nclient:\n  codex:\n    optimize-multi-agent-v2: true\ncodex:\n  response-steering: true\n";
    let created = r#"{"type":"response.created","response":{"id":"resp_fixture_1"}}"#;
    let steered = r#"{"type":"response.steer.accepted","steer":{"id":"s1","previous_response_id":"resp_fixture_1"}}"#;
    let proxy = proxy(
        Profile::Chatgpt,
        Script {
            ws: vec![
                vec![Bytes::from_static(created.as_bytes())],
                // The create waits upstream; the steer's answer ends the running response.
                Vec::new(),
                std::iter::once(Bytes::from_static(steered.as_bytes()))
                    .chain(fixture("sse-completed").frames().into_iter().skip(1))
                    .collect(),
            ],
            ..Script::default()
        },
        rules,
    )
    .await;
    let mut socket = connect(&proxy).await;
    let first = client_frame("ws-create-first", "gpt-5.5");
    socket.send(Message::text(first.clone())).await.unwrap();
    assert_eq!(read(&mut socket, &["response.created"]).await.frames, [created]);
    // A create while the response runs, without a model as a duplex create may be: the
    // model is the one routing edit, added as the first member.
    let late = client_frame("ws-create-continuation", "gpt-5.5").replacen(r#""model":"gpt-5.5","#, "", 1);
    socket.send(Message::text(late.clone())).await.unwrap();
    let steer = text(fixture("ws-steer-api-docs").bytes());
    socket.send(Message::text(steer.clone())).await.unwrap();
    let turn = read(&mut socket, &TERMINAL).await;
    let mut want = vec![steered.to_owned()];
    want.extend(events("sse-completed").into_iter().skip(1));
    assert_eq!(turn.frames, want, "as upstream sent them");
    assert_eq!(
        upstream_frames(&proxy, 0),
        [first, format!(r#"{{"model":"gpt-5.5",{}"#, &late[1..]), steer],
        "the client's frames"
    );
}

/// When the upstream socket ends, the client sees it and recovers on its own. A
/// continuation never moves to another socket or account: on a new connection it is
/// refused locally and nothing reaches upstream. A full create there opens a new socket.
#[tokio::test]
async fn a_lost_socket_never_replays_a_continuation() {
    let proxy = proxy_with(
        Script {
            ws: replies(&["sse-completed", "sse-completed"]),
            ws_close: vec![0],
            ..Script::default()
        },
        "",
        |url| {
            let mut second = credential(Profile::Chatgpt, url, true);
            second.id = "codex-second.json".into();
            second.metadata.insert("access_token".into(), "at-FAKE-second".into());
            vec![credential(Profile::Chatgpt, url, true), second]
        },
    )
    .await;
    let mut socket = connect(&proxy).await;
    socket
        .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
        .await
        .unwrap();
    assert_eq!(read(&mut socket, &TERMINAL).await.frames, events("sse-completed"));
    // The upstream closed the socket after its answer: the client connection ends.
    assert!(
        read(&mut socket, &[]).await.closed.is_some(),
        "the loss reaches the client"
    );

    let mut socket = connect(&proxy).await;
    let continuation = client_frame("ws-create-continuation", "gpt-5.5");
    socket.send(Message::text(continuation)).await.unwrap();
    let refused = read(&mut socket, &["error"]).await;
    let error = refused.frames.last().unwrap();
    assert!(error.contains("previous_response_not_found"), "{error}");
    assert_eq!(proxy.upstream.sockets().len(), 1, "no socket for the continuation");

    let fresh = client_frame("ws-create-first", "gpt-5.5");
    socket.send(Message::text(fresh.clone())).await.unwrap();
    assert_eq!(read(&mut socket, &TERMINAL).await.frames, events("sse-completed"));
    let sockets = proxy.upstream.sockets();
    assert_eq!(sockets.len(), 2, "a new socket for a full create");
    assert_eq!(upstream_frames(&proxy, 1), [fresh]);
}

/// A Codex credential without `websockets` serves the WebSocket route over HTTP (#28):
/// the client's `response.create` goes upstream as the HTTP body without its `type` tag
/// (the first-party frame is the HTTP request plus that tag), with nothing else changed,
/// and the upstream's events reach the client unchanged, one payload per frame. It has no
/// upstream socket to prewarm, so a `generate: false` prewarm is refused with an
/// actionable error instead of a made-up response, and nothing reaches the upstream; the
/// connection stays open for the real turn. A continuation needs an upstream socket's
/// state, so it gets the replay close and nothing is sent.
#[tokio::test]
async fn http_fallback_is_a_separate_explicit_conversion() {
    let proxy = proxy_with(
        Script {
            http: vec![cpa_server::testing::codex_capture::Reply::sse(
                fixture("sse-completed").bytes(),
            )],
            ..Script::default()
        },
        "",
        |url| vec![credential(Profile::Chatgpt, url, false)],
    )
    .await;
    let mut socket = connect(&proxy).await;
    socket
        .send(Message::text(client_frame("ws-create-prewarm", "gpt-5.5")))
        .await
        .unwrap();
    let refused = read(&mut socket, &["error"]).await;
    assert_eq!(refused.frames.len(), 1, "{refused:?}");
    let error: serde_json::Value = serde_json::from_str(&refused.frames[0]).unwrap();
    assert_eq!(error["status"], 400, "{error}");
    let message = error["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("generate: false") && message.contains("websockets"),
        "{message}"
    );
    assert!(
        !refused.frames.iter().any(|f| f.contains("resp_prewarm_")),
        "no fabricated response"
    );
    assert!(proxy.upstream.http().is_empty(), "the prewarm sends nothing upstream");

    let create = client_frame("ws-create-first", "gpt-5.5");
    socket.send(Message::text(create.clone())).await.unwrap();
    let turn = read(&mut socket, &TERMINAL).await;
    assert_eq!(turn.frames, events("sse-completed"), "the upstream's events, unchanged");
    assert!(proxy.upstream.sockets().is_empty(), "no upstream socket");
    let sent = proxy.upstream.http().pop().expect("an HTTP request");
    let untagged = cpa_common::codex_body::CodexBody::parse(create.as_bytes())
        .unwrap()
        .without_member("type");
    assert_eq!(
        text(sent.body),
        String::from_utf8(untagged).unwrap(),
        "the frame without its tag"
    );

    // A continuation: the replay close, and nothing upstream.
    let continuation = r#"{"type":"response.create","model":"gpt-5.5","previous_response_id":"resp_fixture_1","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"next"}]}]}"#;
    socket.send(Message::text(continuation)).await.unwrap();
    let replay = read(&mut socket, &[]).await;
    assert_eq!(replay.closed, Some(1012), "{replay:?}");
    assert_eq!(
        proxy.upstream.http().len(),
        1,
        "the continuation sends nothing upstream"
    );
}

// Native failure delivery (#25, WebSocket half). Each failure frame is hand-authored to
// probe preservation: an unknown member, a raw number spelling (`1.50`) and a `null`
// message must reach the client byte for byte. Their shapes follow the first-party
// decoders: the wrapped status error of `responses_websocket.rs:712-764` and the
// `response.failed` of `sse/responses.rs` (`ws-errors-first-party`, `sse-failed-*`).

const STEERING: &str = "codex:\n  response-steering: true\n";

/// A wrapped status error that fails the request (a request fault: no failover).
const WRAPPED_400: &str = r#"{"type":"error","status":400,"error":{"type":"invalid_request_error","code":"invalid_value","message":null,"param":"input","future_error_field":{"n":1.50}},"future_frame_field":[1e3,-0.0]}"#;
/// A server failure after the response started.
const FAILED_500: &str = r#"{"type":"response.failed","sequence_number":3,"response":{"id":"resp_fixture_1","status":"failed","error":{"code":"server_error","message":null,"future_error_field":1.50},"usage":null}}"#;

/// A failure before `response.created` reaches the client as the upstream sent it, then
/// the connection closes; with and without response steering.
#[tokio::test]
async fn a_failure_before_the_response_starts_reaches_the_client_as_sent() {
    for steering in [false, true] {
        let proxy = proxy(
            Profile::Chatgpt,
            Script {
                ws: vec![vec![Bytes::from_static(WRAPPED_400.as_bytes())]],
                ..Script::default()
            },
            if steering { STEERING } else { "" },
        )
        .await;
        let mut socket = connect(&proxy).await;
        socket
            .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
            .await
            .unwrap();
        let read = read(&mut socket, &[]).await;
        assert_eq!(read.frames, [WRAPPED_400], "steering={steering}: the provider's frame");
        assert!(read.closed.is_some(), "steering={steering}");
    }
}

/// A failure after the response started reaches the client as sent. Without response
/// steering it ends the connection; with it, a failure after the first response is an
/// event the client may correct, and the connection stays open.
#[tokio::test]
async fn a_failure_after_the_response_starts_reaches_the_client_as_sent() {
    let created = r#"{"type":"response.created","response":{"id":"resp_fixture_1"}}"#;
    let delta = r#"{"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"Partial"}"#;
    for steering in [false, true] {
        let proxy = proxy(
            Profile::Chatgpt,
            Script {
                ws: vec![vec![
                    Bytes::from_static(created.as_bytes()),
                    Bytes::from_static(delta.as_bytes()),
                    Bytes::from_static(FAILED_500.as_bytes()),
                ]],
                ..Script::default()
            },
            if steering { STEERING } else { "" },
        )
        .await;
        let mut socket = connect(&proxy).await;
        socket
            .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
            .await
            .unwrap();
        let read = read(&mut socket, &["response.failed"]).await;
        assert_eq!(read.frames, [created, delta, FAILED_500], "steering={steering}");
        let after = tokio::time::timeout(Duration::from_millis(500), socket.next()).await;
        if steering {
            assert!(after.is_err(), "steering keeps the connection open: {after:?}");
        } else {
            assert!(
                matches!(after, Ok(Some(Ok(Message::Close(_))) | Some(Err(_)) | None)),
                "the connection closes: {after:?}"
            );
        }
    }
}

/// A failure that fails over before anything was delivered: every credential answers a
/// retryable wrapped status error, and once the attempts run out the client gets the last
/// provider frame as sent, not a rebuilt error. With and without response steering.
#[tokio::test]
async fn the_exhausted_failure_reaches_the_client_as_sent() {
    let first = r#"{"type":"error","status":503,"error":{"type":"server_error","message":"first","x":1.50}}"#;
    let last = r#"{"type":"error","status":503,"error":{"type":"server_error","message":null,"x":2.50},"future_frame_field":true}"#;
    for steering in [false, true] {
        let proxy = proxy_with(
            Script {
                ws: vec![
                    vec![Bytes::from_static(first.as_bytes())],
                    vec![Bytes::from_static(last.as_bytes())],
                ],
                ..Script::default()
            },
            if steering { STEERING } else { "" },
            |url| {
                let mut second = credential(Profile::Chatgpt, url, true);
                second.id = "codex-second.json".into();
                second.metadata.insert("access_token".into(), "at-FAKE-second".into());
                vec![credential(Profile::Chatgpt, url, true), second]
            },
        )
        .await;
        let mut socket = connect(&proxy).await;
        socket
            .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
            .await
            .unwrap();
        let read = read(&mut socket, &[]).await;
        assert_eq!(
            proxy.upstream.sockets().len(),
            2,
            "steering={steering}: both credentials tried"
        );
        assert_eq!(read.frames, [last], "steering={steering}: the last provider frame");
        assert!(read.closed.is_some(), "steering={steering}");
    }
}

/// Handshake metadata (#3). The downstream socket was upgraded before the first request
/// chose a credential, so `x-codex-turn-state` and `openai-model` from the upstream
/// handshake arrive as a `response.metadata` event the pinned client reads
/// (`sse/responses.rs:198-222`), ahead of the opening turn's first event only.
/// `x-reasoning-included` cannot travel this way: the client reads it from a handshake
/// alone (`responses_websocket.rs:617`), so it is not sent. Metadata that arrives in
/// events (`headers` on an event) reaches the client inside that event, unchanged, on
/// every turn.
#[tokio::test]
async fn handshake_metadata_reaches_the_opening_turn_and_event_metadata_every_turn() {
    let with_headers =
        r#"{"type":"response.created","response":{"id":"resp_fixture_1"},"headers":{"openai-model":"gpt-5.5-event"}}"#;
    let mut first = fixture("sse-completed").frames();
    first[0] = Bytes::from_static(with_headers.as_bytes());
    let proxy = proxy(
        Profile::Chatgpt,
        Script {
            ws_handshake: vec![
                ("x-codex-turn-state".into(), "ts-FAKE".into()),
                ("openai-model".into(), "gpt-5.5".into()),
                ("x-reasoning-included".into(), "true".into()),
            ],
            ws: vec![first.clone(), first.clone()],
            ..Script::default()
        },
        "",
    )
    .await;
    let mut socket = connect(&proxy).await;
    socket
        .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
        .await
        .unwrap();
    let opening = read(&mut socket, &TERMINAL).await;
    let metadata: serde_json::Value = serde_json::from_str(&opening.frames[0]).unwrap();
    assert_eq!(metadata["type"], "response.metadata");
    assert_eq!(metadata["headers"]["x-codex-turn-state"], "ts-FAKE");
    assert_eq!(metadata["headers"]["openai-model"], "gpt-5.5");
    assert!(metadata["headers"].get("x-reasoning-included").is_none());
    let upstream: Vec<String> = first.iter().cloned().map(text).collect();
    assert_eq!(opening.frames[1..], upstream, "then the events as sent");

    socket
        .send(Message::text(client_frame("ws-create-continuation", "gpt-5.5")))
        .await
        .unwrap();
    let reused = read(&mut socket, &TERMINAL).await;
    assert_eq!(reused.frames, upstream, "a reused socket has no handshake to report");
}

// Home (`remote_dispatch`): the pool is remote, and the local store holds no credential.

type Pick = Result<Credential, cpa_server::remote::RemoteError>;

/// A fake Home: hands out `picks` in order, counting picks and released leases.
struct Home {
    picks: std::sync::Mutex<std::collections::VecDeque<Pick>>,
    dispatched: Arc<std::sync::atomic::AtomicUsize>,
    released: Arc<std::sync::atomic::AtomicUsize>,
}

impl cpa_server::remote::RemoteDispatch for Home {
    fn available(&self) -> bool {
        true
    }

    fn dispatch(
        &self,
        _: cpa_server::remote::RemoteRequest,
    ) -> futures_util::future::BoxFuture<'_, Result<cpa_server::remote::RemoteGrant, cpa_server::remote::RemoteError>>
    {
        self.dispatched.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pick = self.picks.lock().unwrap().pop_front().expect("an unexpected Home pick");
        let released = self.released.clone();
        Box::pin(async move {
            pick.map(|credential| cpa_server::remote::RemoteGrant {
                credential,
                end: Box::new(move || {
                    released.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    None
                }),
                cancel: None,
                request_retry: Some(0),
                user_api_key: String::new(),
                private: Default::default(),
            })
        })
    }

    fn models(
        &self,
        _: Vec<(String, String)>,
        _: Vec<(String, String)>,
    ) -> futures_util::future::BoxFuture<'_, Result<Vec<u8>, cpa_server::remote::ModelsError>> {
        unreachable!("no model lists")
    }
}

/// A native WebSocket credential that Home hands out, pointing at `base`.
fn home_credential(base: &str) -> Credential {
    let mut credential = credential(Profile::Compatible, base, true);
    credential.id = "home-native.json".into();
    credential.attributes.insert(
        cpa_core::config::credentials::HOME_AUTH_INDEX.into(),
        "idx-home-native".into(),
    );
    credential
}

/// Home's `model_cooldown` with a positive retry hint and no retry round left.
fn home_cooldown() -> Pick {
    Err(cpa_server::remote::RemoteError {
        error: Box::new(cpa_core::exec::ExecError::local(
            429,
            cpa_core::exec::FailureScope::Credential,
            "model_cooldown: every credential is cooling",
        )),
        code: "model_cooldown".into(),
        kind: cpa_server::remote::RemoteErrorKind::Cooldown {
            retry_after: Some(Duration::from_secs(30)),
            request_retry: Some(0),
        },
    })
}

struct HomeProxy {
    proxy: Proxy,
    dispatched: Arc<std::sync::atomic::AtomicUsize>,
    released: Arc<std::sync::atomic::AtomicUsize>,
}

/// A proxy with an empty local store under a fake Home handing out `picks(base_url)`.
async fn home_proxy(script: Script, extra_config: &str, picks: impl FnOnce(&str) -> Vec<Pick>) -> HomeProxy {
    let upstream = CaptureUpstream::start(script).await;
    let auth_dir = AuthDir::new();
    let cfg = Config::parse(&format!(
        "auth-dir: {}\naccess:\n  api-keys: [client-key]\n{extra_config}",
        auth_dir.0.display()
    ))
    .unwrap();
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        meta: Default::default(),
        xai: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(cfg, Vec::new(), executors));
    let dispatched = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let released = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    rt.set_remote_dispatch(Some(Arc::new(Home {
        picks: std::sync::Mutex::new(picks(&upstream.base_url).into()),
        dispatched: dispatched.clone(),
        released: released.clone(),
    })));
    HomeProxy {
        proxy: Proxy {
            addr: serve(cpa_server::router(rt)).await,
            upstream,
            _auth_dir: auth_dir,
        },
        dispatched,
        released,
    }
}

async fn wait_released(home: &HomeProxy, count: usize) {
    for _ in 0..100 {
        if home.released.load(std::sync::atomic::Ordering::SeqCst) >= count {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        home.released.load(std::sync::atomic::Ordering::SeqCst),
        count,
        "every Home lease released"
    );
}

/// Under Home, before this connection's first pick, nobody knows which credential or
/// transport will serve it: a `generate: false` prewarm is refused with an actionable
/// error, without a Home pick, an upstream request or a made-up response. The connection
/// stays usable: the first real turn takes a Home pick onto the native WebSocket route,
/// and a prewarm after it goes to the provider on that socket.
#[tokio::test]
async fn a_home_prewarm_before_the_first_pick_is_refused() {
    let prewarm_done = r#"{"type":"response.completed","response":{"id":"resp_warm","usage":null}}"#;
    let home = home_proxy(
        Script {
            ws: vec![
                fixture("sse-completed").frames(),
                vec![Bytes::from_static(prewarm_done.as_bytes())],
            ],
            ..Script::default()
        },
        "",
        |url| vec![Ok(home_credential(url))],
    )
    .await;
    let mut socket = connect(&home.proxy).await;
    let prewarm = client_frame("ws-create-prewarm", "gpt-5.5");
    socket.send(Message::text(prewarm.clone())).await.unwrap();
    let refused = read(&mut socket, &["error"]).await;
    assert_eq!(refused.frames.len(), 1, "{refused:?}");
    assert!(!refused.frames[0].contains("resp_prewarm_"), "no fabricated response");
    let error: serde_json::Value = serde_json::from_str(&refused.frames[0]).unwrap();
    assert_eq!(error["status"], 400, "{error}");
    let message = error["error"]["message"].as_str().unwrap();
    assert!(message.contains("before Home selects a credential"), "{message}");
    assert_eq!(
        home.dispatched.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no Home pick"
    );
    assert!(home.proxy.upstream.sockets().is_empty() && home.proxy.upstream.http().is_empty());

    let create = client_frame("ws-create-first", "gpt-5.5");
    socket.send(Message::text(create.clone())).await.unwrap();
    assert_eq!(read(&mut socket, &TERMINAL).await.frames, events("sse-completed"));
    socket.send(Message::text(prewarm.clone())).await.unwrap();
    let warm = read(&mut socket, &TERMINAL).await;
    assert_eq!(warm.frames, [prewarm_done], "the provider's prewarm answer");
    assert_eq!(home.dispatched.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        upstream_frames(&home.proxy, 0),
        [create, prewarm],
        "nothing else upstream"
    );
    drop(socket);
    wait_released(&home, 1).await;
}

/// A native failure that a Home round ends with a retry hint: Home grants a native
/// credential, its upstream answers a retryable wrapped 503, and the next pick is a
/// `model_cooldown` with a positive retry hint and no retry round. Dispatch then ends
/// with `Failure::Remote` around the upstream error, and the client still gets the
/// upstream frame as sent; with and without response steering. Every lease is released.
#[tokio::test]
async fn a_home_cooldown_after_a_native_failure_keeps_the_frame() {
    let wrapped = r#"{"type":"error","status":503,"error":{"type":"server_error","message":null,"x":1.50,"big":12345678901234567890},"future_frame_field":[1e3,-0.0]}"#;
    for steering in [false, true] {
        let home = home_proxy(
            Script {
                ws: vec![vec![Bytes::from_static(wrapped.as_bytes())]],
                ..Script::default()
            },
            if steering { STEERING } else { "" },
            |url| vec![Ok(home_credential(url)), home_cooldown()],
        )
        .await;
        let mut socket = connect(&home.proxy).await;
        socket
            .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
            .await
            .unwrap();
        let read = read(&mut socket, &[]).await;
        assert_eq!(
            read.frames,
            [wrapped],
            "steering={steering}: the upstream frame as sent"
        );
        assert!(read.closed.is_some(), "steering={steering}");
        assert_eq!(
            home.dispatched.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "steering={steering}: the grant, then the cooldown"
        );
        assert_eq!(home.proxy.upstream.sockets().len(), 1, "steering={steering}");
        drop(socket);
        wait_released(&home, 1).await;
    }
}

/// Under Home with an empty local store, a healthy `previous_response_id` continuation
/// stays on the Home pick's credential and upstream socket: no fresh pick, no reconnect,
/// the client's frames unchanged, one grant released once, when the connection ends.
/// With and without response steering. The fake Home has exactly one grant, so a second
/// pick would fail the test.
#[tokio::test]
async fn a_home_continuation_stays_on_its_pick_and_socket() {
    for steering in [false, true] {
        let home = home_proxy(
            Script {
                ws: vec![fixture("sse-completed").frames(), fixture("sse-completed").frames()],
                ..Script::default()
            },
            if steering { STEERING } else { "" },
            |url| vec![Ok(home_credential(url))],
        )
        .await;
        let mut socket = connect(&home.proxy).await;
        let first = client_frame("ws-create-first", "gpt-5.5");
        socket.send(Message::text(first.clone())).await.unwrap();
        assert_eq!(read(&mut socket, &TERMINAL).await.frames, events("sse-completed"));
        let continuation = client_frame("ws-create-continuation", "gpt-5.5");
        socket.send(Message::text(continuation.clone())).await.unwrap();
        let turn = read(&mut socket, &TERMINAL).await;
        assert_eq!(turn.frames, events("sse-completed"), "steering={steering}: {turn:?}");
        assert_eq!(
            home.proxy.upstream.sockets().len(),
            1,
            "steering={steering}: one socket"
        );
        assert_eq!(
            upstream_frames(&home.proxy, 0),
            [first, continuation],
            "steering={steering}: the client's frames"
        );
        assert_eq!(
            home.dispatched.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "steering={steering}: one Home grant"
        );
        assert_eq!(
            home.released.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "steering={steering}: the pick is kept while the connection lives"
        );
        drop(socket);
        wait_released(&home, 1).await;
    }
}

/// A Home continuation that cannot stay on its pick never moves. A continuation for
/// another model releases the pin and closes with the replay code; after the upstream
/// socket is lost, the connection closes, and on a new connection the continuation is
/// refused locally. Neither reaches an upstream socket or takes a Home pick.
#[tokio::test]
async fn a_home_continuation_that_cannot_stay_never_moves() {
    // Another model.
    let home = home_proxy(
        Script {
            ws: vec![fixture("sse-completed").frames()],
            ..Script::default()
        },
        "",
        |url| vec![Ok(home_credential(url))],
    )
    .await;
    let mut socket = connect(&home.proxy).await;
    let first = client_frame("ws-create-first", "gpt-5.5");
    socket.send(Message::text(first.clone())).await.unwrap();
    assert_eq!(read(&mut socket, &TERMINAL).await.frames, events("sse-completed"));
    socket
        .send(Message::text(client_frame("ws-create-continuation", "gpt-5.4")))
        .await
        .unwrap();
    let moved = read(&mut socket, &TERMINAL).await;
    assert_eq!(moved.closed, Some(1012), "{moved:?}");
    assert_eq!(upstream_frames(&home.proxy, 0), [first], "nothing else upstream");
    assert_eq!(home.proxy.upstream.sockets().len(), 1);
    assert_eq!(
        home.dispatched.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "no fresh pick"
    );
    wait_released(&home, 1).await;

    // A lost socket.
    let home = home_proxy(
        Script {
            ws: vec![fixture("sse-completed").frames()],
            ws_close: vec![0],
            ..Script::default()
        },
        "",
        |url| vec![Ok(home_credential(url))],
    )
    .await;
    let mut socket = connect(&home.proxy).await;
    let first = client_frame("ws-create-first", "gpt-5.5");
    socket.send(Message::text(first.clone())).await.unwrap();
    assert_eq!(read(&mut socket, &TERMINAL).await.frames, events("sse-completed"));
    assert!(
        read(&mut socket, &[]).await.closed.is_some(),
        "the loss reaches the client"
    );
    let mut socket = connect(&home.proxy).await;
    socket
        .send(Message::text(client_frame("ws-create-continuation", "gpt-5.5")))
        .await
        .unwrap();
    let refused = read(&mut socket, &["error"]).await;
    assert!(
        refused.frames.last().unwrap().contains("previous_response_not_found"),
        "{refused:?}"
    );
    assert_eq!(home.proxy.upstream.sockets().len(), 1, "no new socket");
    assert_eq!(upstream_frames(&home.proxy, 0), [first]);
    assert_eq!(
        home.dispatched.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "no fresh pick"
    );
    drop(socket);
    wait_released(&home, 1).await;
}

// The Codex review of #38 (threads 4501618723, 4501618725, 4501618727): a native frame never depends on fallback-only
// preparation, keeps its bytes until the request boundary reads them, and nothing on a
// native connection is restored.

/// `ws-create-first` with `input` replaced: a string or `null` passes the request
/// boundary, which type-checks routing fields only; the provider judges the rest.
fn with_input(name: &str, input: &str) -> String {
    let frame = client_frame(name, "gpt-5.5");
    let body = cpa_common::codex_body::CodexBody::parse(frame.as_bytes()).unwrap();
    String::from_utf8(body.with_member("input", input)).unwrap()
}

/// A native first create and continuation whose `input` the HTTP fallback could not
/// convert (a string, `null`) reach the upstream socket unchanged, locally and under
/// Home. The fallback's array rule does not decide a native attempt.
#[tokio::test]
async fn a_native_frame_never_depends_on_fallback_preparation() {
    let first = with_input("ws-create-first", r#""List the files.""#);
    let null = with_input("ws-create-first", "null");
    let continuation = with_input("ws-create-continuation", r#""and the hidden ones""#);
    let replies = || replies(&["sse-completed", "sse-completed", "sse-completed"]);
    let proxy = proxy(
        Profile::Chatgpt,
        Script {
            ws: replies(),
            ..Script::default()
        },
        "",
    )
    .await;
    let mut socket = connect(&proxy).await;
    for frame in [&first, &continuation, &null] {
        socket.send(Message::text(frame.clone())).await.unwrap();
        assert_eq!(
            read(&mut socket, &TERMINAL).await.frames,
            events("sse-completed"),
            "{frame}"
        );
    }
    assert_eq!(proxy.upstream.sockets().len(), 1);
    assert_eq!(
        upstream_frames(&proxy, 0),
        [first.clone(), continuation.clone(), null],
        "as sent"
    );

    let home = home_proxy(
        Script {
            ws: replies(),
            ..Script::default()
        },
        "",
        |url| vec![Ok(home_credential(url))],
    )
    .await;
    let mut socket = connect(&home.proxy).await;
    for frame in [&first, &continuation] {
        socket.send(Message::text(frame.clone())).await.unwrap();
        assert_eq!(
            read(&mut socket, &TERMINAL).await.frames,
            events("sse-completed"),
            "Home: {frame}"
        );
    }
    assert_eq!(upstream_frames(&home.proxy, 0), [first, continuation], "Home: as sent");
    assert_eq!(home.dispatched.load(std::sync::atomic::Ordering::SeqCst), 1);
    drop(socket);
    wait_released(&home, 1).await;
}

/// The server's transcript fallback refused a string `input`. A Codex credential without
/// `websockets` no longer uses that fallback (#28): the frame is the client's
/// `response.create`, so it goes upstream as sent, without its tag, and the provider
/// judges it. Other providers still get the fallback's refusal.
#[tokio::test]
async fn a_codex_turn_over_http_forwards_what_the_fallback_refused() {
    let proxy = proxy_with(
        Script {
            http: vec![cpa_server::testing::codex_capture::Reply::sse(
                fixture("sse-completed").bytes(),
            )],
            ..Script::default()
        },
        "",
        |url| vec![credential(Profile::Chatgpt, url, false)],
    )
    .await;
    let mut socket = connect(&proxy).await;
    let frame = with_input("ws-create-first", r#""List the files.""#);
    socket.send(Message::text(frame.clone())).await.unwrap();
    let turn = read(&mut socket, &TERMINAL).await;
    assert_eq!(turn.frames, events("sse-completed"), "{turn:?}");
    let sent = proxy.upstream.http().pop().expect("an HTTP request");
    let untagged = cpa_common::codex_body::CodexBody::parse(frame.as_bytes())
        .unwrap()
        .without_member("type");
    assert_eq!(text(sent.body), String::from_utf8(untagged).unwrap());
}

/// `ws-create-first` as bytes with an invalid UTF-8 byte inside a string.
fn invalid_utf8_frame() -> Vec<u8> {
    let frame = client_frame("ws-create-first", "gpt-5.5");
    let at = frame.find("List the files.").unwrap();
    let mut bytes = frame.into_bytes();
    bytes[at] = 0xff;
    bytes
}

/// A binary frame keeps its bytes until the request boundary reads them: invalid UTF-8
/// is refused with the boundary's error and reaches no upstream socket, on a first turn,
/// a later turn, and a response-steering connection, where the connection stays open.
/// A valid binary frame goes upstream byte for byte.
#[tokio::test]
async fn a_binary_frame_keeps_its_bytes_until_the_boundary() {
    let invalid = invalid_utf8_frame();
    let at = invalid.iter().position(|b| *b == 0xff).unwrap();
    let message = format!("request body is not valid UTF-8 at byte {at}");

    // First turn.
    let proxy = proxy(Profile::Chatgpt, Script::default(), "").await;
    let mut socket = connect(&proxy).await;
    socket.send(Message::binary(invalid.clone())).await.unwrap();
    let refused = read(&mut socket, &[]).await;
    assert_eq!(refused.frames.len(), 1, "{refused:?}");
    assert!(refused.frames[0].contains(&message), "{refused:?}");
    assert!(!refused.frames[0].contains('\u{fffd}'));
    assert!(proxy.upstream.sockets().is_empty(), "nothing reached the upstream");

    // A later turn, after a valid binary frame.
    let proxy = proxy_with_steering(false).await;
    let mut socket = connect(&proxy).await;
    let valid = client_frame("ws-create-first", "gpt-5.5");
    socket.send(Message::binary(valid.clone().into_bytes())).await.unwrap();
    assert_eq!(read(&mut socket, &TERMINAL).await.frames, events("sse-completed"));
    socket.send(Message::binary(invalid.clone())).await.unwrap();
    let refused = read(&mut socket, &[]).await;
    assert!(refused.frames.iter().any(|f| f.contains(&message)), "{refused:?}");
    assert_eq!(
        upstream_frames(&proxy, 0),
        std::slice::from_ref(&valid),
        "only the valid frame, as sent"
    );

    // A response-steering connection: the duplex refuses it and stays open.
    let proxy = proxy_with_steering(true).await;
    let mut socket = connect(&proxy).await;
    socket.send(Message::binary(valid.clone().into_bytes())).await.unwrap();
    assert_eq!(read(&mut socket, &TERMINAL).await.frames, events("sse-completed"));
    socket.send(Message::binary(invalid)).await.unwrap();
    let refused = read(&mut socket, &["error"]).await;
    assert!(refused.frames.iter().any(|f| f.contains(&message)), "{refused:?}");
    assert_eq!(refused.closed, None);
    assert_eq!(upstream_frames(&proxy, 0), [valid]);
}

async fn proxy_with_steering(steering: bool) -> Proxy {
    proxy(
        Profile::Chatgpt,
        Script {
            ws: replies(&["sse-completed", "sse-completed"]),
            ..Script::default()
        },
        if steering { STEERING } else { "" },
    )
    .await
}

/// `response.append` is refused on a Codex credential (#28): no first-party or documented
/// source defines it. On a response-steering connection the refusal is a local error
/// event and the connection stays open, with the multi-agent setting on; nothing of the
/// append reaches the upstream, and the client's next `response.create` goes upstream as
/// sent. On an ordinary turn the client gets the error and the connection closes.
#[tokio::test]
async fn an_append_is_refused_on_a_codex_connection() {
    let created = r#"{"type":"response.created","response":{"id":"resp_fixture_1"}}"#;
    let created2 =
        r#"{"type":"response.created","response":{"id":"resp_fixture_2","previous_response_id":"resp_fixture_1"}}"#;
    let done = r#"{"type":"response.completed","response":{"id":"resp_fixture_2","output":[]}}"#;
    let append = r#"{"type":"response.append","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"delegate"}]}],"tools":[{"type":"namespace","name":"collaboration","tools":[]}]}"#;
    let refusal = |frame: &str| {
        let error: serde_json::Value = serde_json::from_str(frame).unwrap();
        assert_eq!(error["type"], "error", "{frame}");
        assert_eq!(error["status"], 400, "{frame}");
        assert_eq!(error["error"]["message"], cpa_exec::codex::APPEND_REFUSED, "{frame}");
    };
    let proxy = proxy(
        Profile::Chatgpt,
        Script {
            ws: vec![
                vec![Bytes::from_static(created.as_bytes())],
                vec![
                    Bytes::from_static(created2.as_bytes()),
                    Bytes::from_static(done.as_bytes()),
                ],
            ],
            ..Script::default()
        },
        "client:\n  codex:\n    optimize-multi-agent-v2: true\ncodex:\n  response-steering: true\n",
    )
    .await;
    let mut socket = wreq::Client::new()
        .websocket(format!("ws://{}/v1/responses", proxy.addr))
        .header("authorization", "Bearer client-key")
        .header("user-agent", CODEX_UA)
        .header("originator", "codex_cli_rs")
        .header("x-openai-subagent", "collab_spawn")
        .send()
        .await
        .unwrap()
        .into_websocket()
        .await
        .unwrap();
    socket
        .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
        .await
        .unwrap();
    assert_eq!(read(&mut socket, &["response.created"]).await.frames, [created]);
    socket.send(Message::text(append)).await.unwrap();
    let refused = read(&mut socket, &["error"]).await;
    assert_eq!(refused.closed, None, "{refused:?}");
    refusal(&refused.frames[0]);
    let next = r#"{"type":"response.create","model":"gpt-5.5","previous_response_id":"resp_fixture_1","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"delegate"}]}]}"#;
    socket.send(Message::text(next)).await.unwrap();
    assert_eq!(read(&mut socket, &TERMINAL).await.frames, [created2, done]);
    let frames = upstream_frames(&proxy, 0);
    assert_eq!(frames.len(), 2, "the append sent nothing: {frames:?}");
    assert_eq!(frames[1], next, "the next create as sent");

    // An ordinary turn on the same socket: the error, then the connection closes.
    let proxy = proxy_with(
        Script {
            ws: replies(&["sse-completed"]),
            ..Script::default()
        },
        "",
        |url| vec![credential(Profile::Chatgpt, url, true)],
    )
    .await;
    let mut socket = connect(&proxy).await;
    socket
        .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
        .await
        .unwrap();
    assert_eq!(read(&mut socket, &TERMINAL).await.frames, events("sse-completed"));
    socket.send(Message::text(append)).await.unwrap();
    let refused = read(&mut socket, &["error"]).await;
    refusal(&refused.frames[0]);
    assert!(read(&mut socket, &[]).await.closed.is_some(), "the connection closes");
    assert_eq!(upstream_frames(&proxy, 0).len(), 1, "the append sent nothing");
}

/// An HTTP Responses upstream that answers as the API does: an SSE stream for a
/// `stream: true` body, otherwise one JSON Response object. It records each body.
async fn request_sensitive_upstream() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let bodies = seen.clone();
    let app = axum::Router::new().fallback(move |body: axum::body::Bytes| {
        let bodies = bodies.clone();
        async move {
            use axum::response::IntoResponse;
            let text = String::from_utf8_lossy(&body).into_owned();
            let streamed = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v["stream"].as_bool())
                == Some(true);
            bodies.lock().unwrap().push(text);
            if streamed {
                (
                    [("content-type", "text/event-stream")],
                    fixture("sse-completed").bytes().to_vec(),
                )
                    .into_response()
            } else {
                (
                    [("content-type", "application/json")],
                    r#"{"id":"resp_json","object":"response","status":"completed","output":[]}"#,
                )
                    .into_response()
            }
        }
    });
    let addr = serve(app).await;
    (format!("http://{addr}"), seen)
}

/// A first turn on a credential without `websockets` whose frame omits `stream` or sets
/// it to `false`: a WebSocket turn always delivers events, so the HTTP body asks for the
/// event stream (`stream: true`, the transport adaptation in docs/codex/CONTRACT.md) and
/// the client gets the upstream's events, ending with `response.completed`. Nothing else
/// in the frame changes. Against an upstream that answers a non-streamed body with one
/// JSON object, as the API does.
#[tokio::test]
async fn a_turn_over_http_asks_for_the_event_stream_whatever_the_frame_says() {
    for profile in [Profile::Chatgpt, Profile::Compatible] {
        for (name, stream) in [("omitted", None), ("false", Some("false"))] {
            let (base, seen) = request_sensitive_upstream().await;
            let proxy = proxy_with(Script::default(), "", |_| vec![credential(profile, &base, false)]).await;
            let mut socket = connect(&proxy).await;
            let frame = match stream {
                None => r#"{"type":"response.create","model":"gpt-5.5","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#.to_owned(),
                Some(value) => format!(
                    r#"{{"type":"response.create","model":"gpt-5.5","stream":{value},"input":[{{"type":"message","role":"user","content":[{{"type":"input_text","text":"hi"}}]}}]}}"#
                ),
            };
            socket.send(Message::text(frame.clone())).await.unwrap();
            let turn = read(&mut socket, &TERMINAL).await;
            assert_eq!(turn.frames, events("sse-completed"), "{profile:?} {name}: {turn:?}");
            assert_eq!(turn.closed, None, "{profile:?} {name}: the connection stays open");
            let sent = seen.lock().unwrap().clone();
            assert_eq!(sent.len(), 1, "{profile:?} {name}");
            let untagged = cpa_common::codex_body::CodexBody::parse(frame.as_bytes())
                .unwrap()
                .without_member("type");
            let want = cpa_common::codex_body::CodexBody::parse(&untagged)
                .unwrap()
                .with_member("stream", "true");
            assert_eq!(
                sent[0],
                String::from_utf8(want).unwrap(),
                "{profile:?} {name}: only `stream` changes"
            );
        }
    }
}

/// An upstream that answers a WebSocket turn's `stream: true` HTTP request with a plain
/// JSON object has no events to deliver: the turn fails, the client never receives the
/// bare object, and the proxy invents no event.
#[tokio::test]
async fn a_turn_over_http_refuses_an_answer_without_an_event_stream() {
    let proxy = proxy_with(
        Script {
            http: vec![cpa_server::testing::codex_capture::Reply::json(
                200,
                r#"{"id":"resp_json","object":"response","status":"completed","output":[]}"#,
            )],
            ..Script::default()
        },
        "",
        |url| vec![credential(Profile::Compatible, url, false)],
    )
    .await;
    let mut socket = connect(&proxy).await;
    socket
        .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
        .await
        .unwrap();
    let turn = read(&mut socket, &[]).await;
    assert!(turn.closed.is_some(), "the turn ends: {turn:?}");
    assert!(
        !turn.frames.iter().any(|f| f.contains("resp_json")),
        "the bare object never reaches the client: {turn:?}"
    );
    assert!(
        !turn.frames.iter().any(|f| f.contains("response.completed")),
        "no fabricated event: {turn:?}"
    );
    assert_eq!(proxy.upstream.http().len(), 1);
}

/// A real backend can omit MIME on valid SSE. Both endpoint profiles must still
/// deliver the exact fixture events, without changing the request.
#[tokio::test]
async fn valid_sse_without_content_type_reaches_websocket_client() {
    for profile in [Profile::Chatgpt, Profile::Compatible] {
        for has_content_type in [true, false] {
            let mut reply = cpa_server::testing::codex_capture::Reply::sse(fixture("sse-completed").bytes().to_vec());
            if !has_content_type {
                reply.headers.clear();
            }
            let proxy = proxy_with(
                Script {
                    http: vec![reply],
                    ..Script::default()
                },
                "",
                |url| vec![credential(profile, url, false)],
            )
            .await;
            let mut socket = connect(&proxy).await;
            socket
                .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
                .await
                .unwrap();
            let turn = read(&mut socket, &TERMINAL).await;
            assert_eq!(
                turn.frames,
                events("sse-completed"),
                "{profile:?} MIME={has_content_type}"
            );
            assert_eq!(turn.closed, None);
            let sent = proxy.upstream.http();
            assert_eq!(sent.len(), 1);
        }
    }
}

#[tokio::test]
async fn initial_bom_does_not_hide_the_only_terminal_event_on_websocket_fallback() {
    let terminal = fixture("sse-completed").frames().pop().unwrap();
    for profile in [Profile::Chatgpt, Profile::Compatible] {
        for mime in [None, Some("text/event-stream")] {
            for event_line in ["", "event: response.completed\n"] {
                let mut reply = cpa_server::testing::codex_capture::Reply::sse(format!(
                    "\u{feff}{event_line}data: {}\n\n",
                    std::str::from_utf8(&terminal).unwrap()
                ));
                reply.headers = mime
                    .map(|mime| vec![("content-type".into(), mime.into())])
                    .unwrap_or_default();
                let proxy = proxy_with(
                    Script {
                        http: vec![reply],
                        ..Script::default()
                    },
                    "",
                    |url| vec![credential(profile, url, false)],
                )
                .await;
                let mut socket = connect(&proxy).await;
                socket
                    .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
                    .await
                    .unwrap();
                let turn = read(&mut socket, &TERMINAL).await;
                assert_eq!(
                    turn.frames,
                    vec![std::str::from_utf8(&terminal).unwrap().to_owned()],
                    "{profile:?} MIME={mime:?} event_line={event_line:?}"
                );
                assert_eq!(turn.closed, None);
            }
        }
    }
}
