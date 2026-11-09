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
    rt: Arc<cpa_server::runtime::Runtime>,
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
        addr: serve(cpa_server::router(rt.clone())).await,
        upstream,
        rt,
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
                let kind = cpa_common::rawjson::get(&text, "type").str().to_owned();
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
        addr: serve(cpa_server::router(rt.clone())).await,
        rt,
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

/// Handshake metadata (#3, #24). The downstream socket was upgraded before the first
/// request chose a credential, so `x-codex-turn-state` and `openai-model` from the
/// upstream handshake arrive as a `response.metadata` event the pinned client reads
/// (`sse/responses.rs:193-222`), ahead of the opening turn's first event. A later turn on
/// the reused socket gets `openai-model` alone the same way, as the directly connected
/// client re-emits its upgrade model on every request (`responses_websocket.rs:302-303`).
/// `x-reasoning-included` cannot travel this way: the client reads it from a handshake
/// alone (`responses_websocket.rs:617`), so it is not sent. Metadata that arrives in
/// events (`headers` on an event) reaches the client inside that event, unchanged and
/// after the proxy's event, on every turn.
#[tokio::test]
async fn handshake_metadata_reaches_each_turn_and_event_metadata_passes_unchanged() {
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
    assert_eq!(
        reused.frames[0], r#"{"type":"response.metadata","headers":{"openai-model":"gpt-5.5"}}"#,
        "the reused socket's model alone, never its turn state"
    );
    assert_eq!(reused.frames[1..], upstream, "then the events as sent");
}

/// The upgrade headers of every metadata test: two the client reads and three that must
/// never travel.
fn metadata_handshake(model: &str) -> Vec<(String, String)> {
    [
        ("x-codex-turn-state", "ts-FAKE"),
        ("openai-model", model),
        ("x-reasoning-included", "true"),
        ("set-cookie", "session=SECRET"),
        ("x-request-id", "req-SECRET"),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value.to_owned()))
    .collect()
}

/// #24 (refs #3). The pinned client's startup prewarm (`generate: false`) opens the
/// socket and drains its notifications, keeping only the completion
/// (`core/src/client.rs:2550-2557`), then hands that session to the real request
/// (`core/src/tasks/regular.rs:100-131`). A directly connected client still reports its
/// upgrade model on that request, because it re-emits it on every request
/// (`responses_websocket.rs:302-303`). Through the proxy, the real request, its
/// continuation and a later logical turn on the same socket each get a
/// `response.metadata` holding only `openai-model`. The turn state reached the prewarm,
/// which opened the socket; repeating it would seed a later turn's first-wins lock with
/// a stale value. The upgrade names another model than requested, a reroute that stays
/// visible on every request. With and without response steering, the upstream receives
/// exactly the client's three frames.
#[tokio::test]
async fn a_prewarmed_socket_reports_its_model_on_every_later_request() {
    const OPENING: &str =
        r#"{"type":"response.metadata","headers":{"x-codex-turn-state":"ts-FAKE","openai-model":"gpt-5.5-served"}}"#;
    const LATER: &str = r#"{"type":"response.metadata","headers":{"openai-model":"gpt-5.5-served"}}"#;
    let prewarm_created = r#"{"type":"response.created","response":{"id":"resp_fixture_1"}}"#;
    let prewarm_done = r#"{"type":"response.completed","response":{"id":"resp_fixture_1","usage":null}}"#;
    for steering in [false, true] {
        let proxy = proxy(
            Profile::Chatgpt,
            Script {
                ws_handshake: metadata_handshake("gpt-5.5-served"),
                ws: vec![
                    vec![
                        Bytes::from_static(prewarm_created.as_bytes()),
                        Bytes::from_static(prewarm_done.as_bytes()),
                    ],
                    fixture("sse-completed").frames(),
                    fixture("sse-completed").frames(),
                ],
                ..Script::default()
            },
            if steering { STEERING } else { "" },
        )
        .await;
        let mut socket = connect(&proxy).await;
        let prewarm = client_frame("ws-create-prewarm", "gpt-5.5");
        socket.send(Message::text(prewarm.clone())).await.unwrap();
        let warm = read(&mut socket, &TERMINAL).await;
        assert_eq!(
            warm.frames,
            [OPENING, prewarm_created, prewarm_done],
            "steering={steering}"
        );
        let mut later = vec![LATER.to_owned()];
        later.extend(events("sse-completed"));
        let continuation = client_frame("ws-create-continuation", "gpt-5.5");
        socket.send(Message::text(continuation.clone())).await.unwrap();
        let real = read(&mut socket, &TERMINAL).await;
        assert_eq!(real.frames, later, "steering={steering}: the real request");
        let next = client_frame("ws-create-first", "gpt-5.5");
        socket.send(Message::text(next.clone())).await.unwrap();
        let turn = read(&mut socket, &TERMINAL).await;
        assert_eq!(turn.frames, later, "steering={steering}: a later logical turn");
        let sockets = proxy.upstream.sockets();
        assert_eq!(sockets.len(), 1, "steering={steering}");
        assert_eq!(
            upstream_frames(&proxy, 0),
            [prewarm, continuation, next],
            "steering={steering}: nothing else was sent"
        );
        assert_eq!(sockets[0].headers["authorization"], "Bearer at-FAKE");
        let delivered = [warm.frames, real.frames, turn.frames].concat();
        assert!(
            delivered
                .iter()
                .all(|f| !f.contains("SECRET") && !f.contains("x-reasoning-included") && !f.contains("client-key")),
            "steering={steering}: {delivered:?}"
        );
    }
}

/// #24: the alias rule is judged on each request. Codex reports any `openai-model` that
/// differs from the model it sent as a cyber-safety reroute, so a request whose model the
/// proxy rewrote never gets the upgrade's model, and a request on the same socket that
/// named the upstream model does. With and without response steering.
#[tokio::test]
async fn each_request_on_a_reused_socket_judges_the_alias_rule_itself() {
    for steering in [false, true] {
        let upstream = CaptureUpstream::start(Script {
            ws_handshake: metadata_handshake("gpt-upstream"),
            ws: replies(&["sse-completed", "sse-completed", "sse-completed"]),
            ..Script::default()
        })
        .await;
        let auth_dir = AuthDir::new();
        let cfg = Config::parse(&format!(
            "auth-dir: {}\naccess:\n  api-keys: [client-key]\n{}codex-api-key:\n  - api-key: key-FAKE\n    base-url: {}\n    websockets: true\n    models:\n      - name: gpt-upstream\n      - name: gpt-upstream\n        alias: gpt-alias\n",
            auth_dir.0.display(),
            if steering { STEERING } else { "" },
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
            addr: serve(cpa_server::router(rt.clone())).await,
            rt,
            upstream,
            _auth_dir: auth_dir,
        };
        let mut socket = connect(&proxy).await;
        let routed = |frame: &str| frame.replacen(r#""model":"gpt-alias""#, r#""model":"gpt-upstream""#, 1);
        let with_metadata = |metadata: &str| {
            let mut frames = vec![metadata.to_owned()];
            frames.extend(events("sse-completed"));
            frames
        };
        let turns = [
            (
                client_frame("ws-create-first", "gpt-alias"),
                with_metadata(r#"{"type":"response.metadata","headers":{"x-codex-turn-state":"ts-FAKE"}}"#),
            ),
            (
                client_frame("ws-create-continuation", "gpt-upstream"),
                with_metadata(r#"{"type":"response.metadata","headers":{"openai-model":"gpt-upstream"}}"#),
            ),
            (
                client_frame("ws-create-continuation", "gpt-alias"),
                events("sse-completed"),
            ),
        ];
        let mut sent = Vec::new();
        for (i, (frame, want)) in turns.into_iter().enumerate() {
            socket.send(Message::text(frame.clone())).await.unwrap();
            let read = read(&mut socket, &TERMINAL).await;
            assert_eq!(read.frames, want, "steering={steering}: turn {i}");
            sent.push(routed(&frame));
        }
        assert_eq!(proxy.upstream.sockets().len(), 1, "steering={steering}");
        assert_eq!(upstream_frames(&proxy, 0), sent, "steering={steering}");
    }
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
            addr: serve(cpa_server::router(rt.clone())).await,
            rt,
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

// #24: the response-steering path, keepalive and continuation ownership, captured as the
// upstream received them through the public router.

/// Every Codex rewrite control Go had, with response steering on: none may reach a frame.
const STEERING_WITH_REWRITES: &str = "payload:\n  default:\n    - models: [{name: \"*\"}]\n      params: {max_output_tokens: 7}\n  override:\n    - models: [{name: \"*\", protocol: codex}]\n      params: {instructions: injected, parallel_tool_calls: true, service_tier: priority}\n  filter:\n    - models: [{name: \"*\"}]\n      params: [include, prompt_cache_key, prompt_cache_options, service_tier, previous_response_id]\ncodex:\n  response-steering: true\n  optimize-multi-agent-v2: true\n  orphan-delegation-compatibility: true\n";

/// Contract probe, from the multi-agent body of `codex_settings.rs`: a create each Go
/// rewrite would have changed (the `collaboration` namespace, an orphan delegation output,
/// a replayed web search call, `parallel_tool_calls: false`, output replay in `include`, a
/// cache key and the `flex` tier), as a continuation.
const MULTI_AGENT_CREATE: &str = r#"{"type":"response.create","model":"gpt-5.5","previous_response_id":"resp_fixture_1","input":[{"type":"function_call_output","call_id":"orphan","name":"create_thread","namespace":"codex_app","output":"thread ok"},{"type":"web_search_call","id":"ws_1","status":"completed","action":{"type":"search","query":"q"}},{"type":"message","role":"user","content":[{"type":"input_text","text":"delegate"}]}],"tools":[{"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"spawn_agent","description":"Spawns an agent.","parameters":{"type":"object","properties":{"message":{"type":"string"}}}}]}],"parallel_tool_calls":false,"include":["reasoning.encrypted_content","output.encrypted_content"],"prompt_cache_key":"pck-1","service_tier":"flex","store":false}"#;

/// Contract probe: `ws-steer-api-docs` with unknown members, raw number spellings, `null`
/// and `false`. The upstream decides whether it accepts a steer; the proxy adds nothing.
const STEER_PROBE: &str = r#"{"type":"response.steer","previous_response_id":"resp_probe_4","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"Keep the scope small."}]}],"future_steer_field":{"n":1.50,"e":1e3,"z":null,"f":false}}"#;

/// #24: on a `codex.response-steering` connection, every frame after the first goes
/// through the duplex queue, not the turn path, and still reaches the one upstream socket
/// as the client sent it, with every rewrite control on and a multi-agent Codex client:
/// a client prewarm and its continuation (incremental input, `previous_response_id`),
/// encrypted reasoning with a namespaced call, unknown members and item kinds with raw
/// numbers, `null` and `flex`, the multi-agent probe as a binary frame, and a steer. The
/// provider's prewarm answer reaches the client as sent; the proxy answers nothing itself.
#[tokio::test]
async fn steering_later_frames_reach_the_socket_as_sent() {
    let warm_created = r#"{"type":"response.created","response":{"id":"resp_warm"}}"#;
    let warm_done = r#"{"type":"response.completed","response":{"id":"resp_warm","usage":null}}"#;
    let probe_created = r#"{"type":"response.created","response":{"id":"resp_probe_4"}}"#;
    let steered =
        r#"{"type":"response.steer.accepted","steer":{"id":"steer_probe_1","previous_response_id":"resp_probe_4"}}"#;
    let probe_done = r#"{"type":"response.completed","response":{"id":"resp_probe_4","output":[]}}"#;
    let replies = vec![
        vec![
            Bytes::from_static(warm_created.as_bytes()),
            Bytes::from_static(warm_done.as_bytes()),
        ],
        fixture("sse-completed").frames(),
        fixture("sse-completed").frames(),
        fixture("sse-completed").frames(),
        vec![Bytes::from_static(probe_created.as_bytes())],
        vec![
            Bytes::from_static(steered.as_bytes()),
            Bytes::from_static(probe_done.as_bytes()),
        ],
    ];
    let proxy = proxy(
        Profile::Chatgpt,
        Script {
            ws: replies,
            ..Script::default()
        },
        STEERING_WITH_REWRITES,
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

    let prewarm = client_frame("ws-create-prewarm", "gpt-5.5");
    socket.send(Message::text(prewarm.clone())).await.unwrap();
    let warm = read(&mut socket, &TERMINAL).await;
    assert_eq!(warm.frames, [warm_created, warm_done], "the provider's prewarm answer");

    let later = [
        client_frame("ws-create-continuation", "gpt-5.5"),
        as_create("http-request-full", "gpt-5.5"),
        as_create("probe-request-unknown-fields", "gpt-5.5"),
    ];
    for frame in &later {
        socket.send(Message::text(frame.clone())).await.unwrap();
        let turn = read(&mut socket, &TERMINAL).await;
        assert_eq!(turn.frames, events("sse-completed"), "{frame}");
        assert_eq!(turn.closed, None);
    }
    socket
        .send(Message::binary(MULTI_AGENT_CREATE.as_bytes().to_vec()))
        .await
        .unwrap();
    assert_eq!(read(&mut socket, &["response.created"]).await.frames, [probe_created]);
    socket.send(Message::text(STEER_PROBE)).await.unwrap();
    let steering = read(&mut socket, &TERMINAL).await;
    assert_eq!(steering.frames, [steered, probe_done], "as upstream sent them");
    assert_eq!(steering.closed, None);

    let mut want = vec![prewarm];
    want.extend(later);
    want.extend([MULTI_AGENT_CREATE.to_owned(), STEER_PROBE.to_owned()]);
    assert_eq!(proxy.upstream.sockets().len(), 1, "one upstream socket");
    assert_eq!(upstream_frames(&proxy, 0), want, "the client's frames, byte for byte");
    assert!(proxy.upstream.http().is_empty(), "no HTTP request");
}

/// What the client read while a turn was idle: ping control frames until `pings`
/// arrived. Any text frame meanwhile fails the test.
async fn pings(socket: &mut WebSocket, pings: usize) {
    let mut seen = 0;
    while seen < pings {
        let next = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .unwrap_or_else(|_| panic!("no keepalive ping after {seen}"));
        match next {
            Some(Ok(Message::Ping(_))) => seen += 1,
            Some(Ok(Message::Pong(_))) => {}
            other => panic!("only pings while the turn is idle: {other:?}"),
        }
    }
}

/// #24: the proxy sends no inference keepalive. With
/// `requests.streaming.keepalive-seconds`, an idle native turn gets WebSocket ping control
/// frames to the client, and nothing reaches the upstream socket: the provider sees only
/// the client's create, then the client's interrupt. Ordinary and steering connections.
#[tokio::test]
async fn keepalive_pings_only_the_client() {
    for steering in [false, true] {
        let interrupt_events = events("ws-events-interrupt");
        let config = format!(
            "requests:\n  streaming:\n    keepalive-seconds: 1\n{}",
            if steering { STEERING } else { "" }
        );
        let proxy = proxy(
            Profile::Chatgpt,
            Script {
                ws: vec![
                    vec![Bytes::from(interrupt_events[0].clone())],
                    interrupt_events[1..].iter().cloned().map(Bytes::from).collect(),
                ],
                ..Script::default()
            },
            &config,
        )
        .await;
        let mut socket = connect(&proxy).await;
        let first = client_frame("ws-create-first", "gpt-5.5");
        socket.send(Message::text(first.clone())).await.unwrap();
        let started = read(&mut socket, &["response.created"]).await;
        assert_eq!(started.frames, interrupt_events[..1], "steering={steering}");
        pings(&mut socket, 2).await;
        assert_eq!(
            upstream_frames(&proxy, 0),
            std::slice::from_ref(&first),
            "steering={steering}: the keepalive sent nothing upstream"
        );
        let interrupt = text(fixture("ws-interrupt").bytes());
        socket.send(Message::text(interrupt.clone())).await.unwrap();
        let ended = read(&mut socket, &["response.incomplete"]).await;
        assert_eq!(ended.frames, interrupt_events[1..], "steering={steering}");
        assert_eq!(proxy.upstream.sockets().len(), 1, "steering={steering}");
        assert_eq!(
            upstream_frames(&proxy, 0),
            [first, interrupt],
            "steering={steering}: the client's frames only"
        );
    }
}

/// #24: a continuation never moves to another account. Two credentials serve the model,
/// and the continuation's socket answers a retryable status error, which fails a first
/// create over to the other credential (`the_exhausted_failure_reaches_the_client_as_sent`).
/// A continuation stays on its socket: the client gets the provider's frame, and no second
/// socket or account receives the continuation. With and without response steering.
#[tokio::test]
async fn a_failed_continuation_never_fails_over() {
    let retryable = r#"{"type":"error","status":503,"error":{"type":"server_error","message":null,"x":1.50}}"#;
    for steering in [false, true] {
        let proxy = proxy_with(
            Script {
                ws: vec![
                    fixture("sse-completed").frames(),
                    vec![Bytes::from_static(retryable.as_bytes())],
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
        let first = client_frame("ws-create-first", "gpt-5.5");
        socket.send(Message::text(first.clone())).await.unwrap();
        assert_eq!(read(&mut socket, &TERMINAL).await.frames, events("sse-completed"));
        let continuation = client_frame("ws-create-continuation", "gpt-5.5");
        socket.send(Message::text(continuation.clone())).await.unwrap();
        let failed = read(&mut socket, &["error"]).await;
        assert_eq!(failed.frames, [retryable], "steering={steering}: the provider's frame");
        let sockets = proxy.upstream.sockets();
        assert_eq!(sockets.len(), 1, "steering={steering}: no second socket");
        assert_eq!(
            upstream_frames(&proxy, 0),
            [first, continuation],
            "steering={steering}: the continuation went upstream once"
        );
    }
}

/// Fires its `Notify` when dropped: the upstream's streamed body was dropped.
struct OnDrop(Arc<tokio::sync::Notify>);

impl Drop for OnDrop {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

/// An HTTP Responses upstream that streams `head`, then only SSE comments, so the
/// response never ends on its own and a dropped connection fails its next write. It
/// records each body, and the `Notify` fires once the proxy dropped the response.
async fn unending_upstream(head: String) -> (String, Arc<std::sync::Mutex<Vec<String>>>, Arc<tokio::sync::Notify>) {
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let cancelled = Arc::new(tokio::sync::Notify::new());
    let (bodies, dropped) = (seen.clone(), cancelled.clone());
    let app = axum::Router::new().fallback(move |body: Bytes| {
        let (bodies, dropped, head) = (bodies.clone(), dropped.clone(), head.clone());
        async move {
            bodies.lock().unwrap().push(String::from_utf8_lossy(&body).into_owned());
            let guard = OnDrop(dropped);
            let comments = futures_util::stream::unfold((), |()| async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Some((Bytes::from_static(b": idle\n\n"), ()))
            });
            let stream = futures_util::stream::once(async move { Bytes::from(head) })
                .chain(comments)
                .map(move |chunk| {
                    let _held = &guard;
                    Ok::<_, std::io::Error>(chunk)
                });
            (
                [("content-type", "text/event-stream")],
                axum::body::Body::from_stream(stream),
            )
        }
    });
    let addr = serve(app).await;
    (format!("http://{addr}"), seen, cancelled)
}

/// #24 with #1: a Codex turn served over HTTP has no upstream socket for the client's
/// `response.interrupt`. The pinned client waits, after a cancel, for both an
/// acknowledgement and the response's terminal event (`responses_websocket.rs:310-333`,
/// 824-839, 886-913), and reads only the ID, the reason and the usage of an interrupted
/// `response.incomplete` (`sse/responses.rs:451-518`). The proxy cancels the upstream
/// request and answers that handshake itself. Its two events are local transport answers,
/// not provider output: the acknowledgement names the interrupt's response, and the
/// interrupted end carries only the output items the provider already delivered, with no
/// usage and no text of its own. Nothing else reaches the upstream.
#[tokio::test]
async fn an_http_turn_answers_an_interrupt_without_inventing_output() {
    let frames = events("sse-completed");
    let delivered = &frames[..2];
    let head: String = delivered.iter().map(|data| format!("data: {data}\n\n")).collect();
    let (base, bodies, cancelled) = unending_upstream(head).await;
    let proxy = proxy_with(Script::default(), "", |_| {
        vec![credential(Profile::Chatgpt, &base, false)]
    })
    .await;
    let mut socket = connect(&proxy).await;
    let create = client_frame("ws-create-first", "gpt-5.5");
    socket.send(Message::text(create.clone())).await.unwrap();
    let started = read(&mut socket, &["response.output_item.done"]).await;
    assert_eq!(started.frames, delivered, "the provider's events, unchanged");

    socket
        .send(Message::text(text(fixture("ws-interrupt").bytes())))
        .await
        .unwrap();
    let ended = read(&mut socket, &["response.incomplete"]).await;
    assert_eq!(ended.closed, None, "{ended:?}");
    assert_eq!(
        ended.frames[0], r#"{"type":"response.interrupt.accepted","response_id":"resp_fixture_1"}"#,
        "the local acknowledgement"
    );
    assert_eq!(ended.frames.len(), 2, "{ended:?}");
    let incomplete: serde_json::Value = serde_json::from_str(&ended.frames[1]).unwrap();
    let item: serde_json::Value = serde_json::from_str(&delivered[1]).unwrap();
    assert_eq!(
        incomplete,
        serde_json::json!({
            "type": "response.incomplete",
            "response": {
                "id": "resp_fixture_1",
                "status": "incomplete",
                "incomplete_details": {"reason": "interrupted"},
                "output": [item["item"]],
            },
        }),
        "an interrupted end with the delivered items only, and no usage"
    );
    tokio::time::timeout(Duration::from_secs(5), cancelled.notified())
        .await
        .expect("the proxy cancels the upstream request");
    let untagged = cpa_common::codex_body::CodexBody::parse(create.as_bytes())
        .unwrap()
        .without_member("type");
    assert_eq!(
        *bodies.lock().unwrap(),
        [String::from_utf8(untagged).unwrap()],
        "one upstream request: the create, without its tag"
    );
    assert!(proxy.upstream.sockets().is_empty(), "no upstream socket");
}

/// Waits until upstream socket `socket` read `count` client frames: the barrier on what
/// upstream actually received, not a quiet period.
async fn upstream_read(proxy: &Proxy, socket: usize, count: usize) {
    let started = std::time::Instant::now();
    while proxy.upstream.sockets()[socket].frames.len() < count {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "upstream read {} of {count} frames",
            proxy.upstream.sockets()[socket].frames.len()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// #24: a `codex.response-steering` connection keeps at most 64 steers in flight, a local
/// resource ceiling (`docs/codex/CONTRACT.md` "Socket and account ownership"). Through
/// the public router, an upstream that reads every steer and acknowledges none receives
/// exactly 64, each as the client sent it, while the test waits for each read. The 65th
/// ends the client connection and never goes upstream. The end is request-scoped: the
/// credential is not cooled, so the next connection's create is served by it on a fresh
/// socket, and no steer is replayed there.
#[tokio::test]
async fn steering_stops_forwarding_steers_at_the_ceiling() {
    const CEILING: usize = 64;
    let created = r#"{"type":"response.created","response":{"id":"resp_fixture_1"}}"#;
    let mut ws = vec![vec![Bytes::from_static(created.as_bytes())]];
    ws.extend((0..CEILING).map(|_| Vec::new()));
    ws.push(fixture("sse-completed").frames());
    let proxy = proxy(
        Profile::Chatgpt,
        Script {
            ws,
            ..Script::default()
        },
        STEERING,
    )
    .await;
    let mut socket = connect(&proxy).await;
    let first = client_frame("ws-create-first", "gpt-5.5");
    socket.send(Message::text(first.clone())).await.unwrap();
    assert_eq!(read(&mut socket, &["response.created"]).await.frames, [created]);
    let steer = |n: usize| {
        format!(r#"{{"type":"response.steer","previous_response_id":"resp_fixture_1","input":"{n}","future":1.50}}"#)
    };
    let mut want = vec![first.clone()];
    for n in 0..CEILING {
        socket.send(Message::text(steer(n))).await.unwrap();
        want.push(steer(n));
        upstream_read(&proxy, 0, want.len()).await;
    }
    socket.send(Message::text(steer(CEILING))).await.unwrap();
    let ended = tokio::select! {
        ended = read(&mut socket, &[]) => ended,
        () = upstream_read(&proxy, 0, want.len() + 1) => panic!("the 65th steer went upstream"),
    };
    assert!(ended.frames.is_empty(), "no event after the ceiling: {ended:?}");
    assert!(ended.closed.is_some(), "the connection ends: {ended:?}");
    assert_eq!(
        upstream_frames(&proxy, 0),
        want,
        "the create and 64 steers as sent; the 65th never went upstream"
    );

    let mut again = connect(&proxy).await;
    again.send(Message::text(first.clone())).await.unwrap();
    let served = read(&mut again, &TERMINAL).await;
    assert_eq!(served.frames, events("sse-completed"), "the credential still serves");
    assert_eq!(proxy.upstream.sockets().len(), 2, "one fresh socket");
    assert_eq!(upstream_frames(&proxy, 1), [first], "nothing replayed on it");
}

/// #25: on a `codex.response-steering` connection, every upstream frame reaches the
/// client as it arrived, with a steer before and a steer after the first output delta:
/// private and unknown events with raw number spellings, steer acknowledgements, a
/// namespaced tool call, an incomplete end with a reason the proxy does not know, an
/// automatic successor whose completion has no usage, an explicit create whose usage
/// is reported as zero, and an incomplete end with zero output and no reason, which no
/// steering connection answers locally (docs/codex/CONTRACT.md "Permitted
/// transformations"). Nothing is renamed, repaired, filled in or replaced, the client's
/// frames reach the one upstream socket as sent, and the connection stays open. Each
/// batch ends with an event the proxy does not model, so the client has every event of
/// the batch before the next frame goes out.
#[tokio::test]
async fn steering_responses_pass_unchanged_before_and_after_the_first_delta() {
    const CREATED_1: &str =
        r#"{"type":"response.created","response":{"id":"resp_st_1","status":"in_progress","output":[]}}"#;
    const RATE_LIMITS: &str =
        r#"{"type":"codex.rate_limits","primary":{"used_percent":1.50,"window_minutes":300},"secondary":null}"#;
    const BARRIER_1: &str = r#"{"type":"test.delivery_barrier","n":1}"#;
    const STEER_1: &str = r#"{"type":"response.steer","previous_response_id":"resp_st_1","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"before the first delta"}]}],"future":{"n":1e3}}"#;
    const ACCEPTED_1: &str = r#"{"type":"response.steer.accepted","steer":{"id":"steer_st_1","previous_response_id":"resp_st_1"},"sequence_number":4}"#;
    const DELTA: &str = r#"{"type":"response.output_text.delta","item_id":"msg_st_1","output_index":0,"content_index":0,"delta":"Hel"}"#;
    const BARRIER_2: &str = r#"{"type":"test.delivery_barrier","n":2,"x":-0.0}"#;
    const STEER_2: &str =
        r#"{"type":"response.steer","previous_response_id":"resp_st_1","input":"after the first delta"}"#;
    const ACCEPTED_2: &str =
        r#"{"type":"response.steer.accepted","steer":{"id":"steer_st_2","previous_response_id":"resp_st_1"}}"#;
    const CALL: &str = r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"fc_st_1","status":"completed","name":"spawn_agent","namespace":"collaboration","arguments":"{}","call_id":"call_st_1"}}"#;
    const ENDED_1: &str = r#"{"type":"response.incomplete","response":{"id":"resp_st_1","status":"incomplete","incomplete_details":{"reason":"future_reason"},"output":[]}}"#;
    const CREATED_2: &str =
        r#"{"type":"response.created","response":{"id":"resp_st_2","status":"in_progress","output":[]}}"#;
    const DONE_2: &str =
        r#"{"type":"response.completed","response":{"id":"resp_st_2","status":"completed","output":[]}}"#;
    const BARRIER_3: &str = r#"{"type":"test.delivery_barrier","n":3}"#;
    const CREATED_3: &str =
        r#"{"type":"response.created","response":{"id":"resp_st_3","status":"in_progress","output":[]}}"#;
    const DONE_3: &str = r#"{"type":"response.completed","response":{"id":"resp_st_3","status":"completed","output":[],"usage":{"input_tokens":0,"input_tokens_details":{"cached_tokens":0},"output_tokens":0,"total_tokens":0}}}"#;
    const CREATED_4: &str =
        r#"{"type":"response.created","response":{"id":"resp_st_4","status":"in_progress","output":[]}}"#;
    const EMPTY_4: &str = r#"{"type":"response.incomplete","response":{"id":"resp_st_4","status":"incomplete","output":[],"usage":{"input_tokens":5,"output_tokens":0,"total_tokens":5}}}"#;
    let batch = |frames: &[&'static str]| frames.iter().map(|f| Bytes::from_static(f.as_bytes())).collect();
    let batches: [&[&'static str]; 5] = [
        &[CREATED_1, RATE_LIMITS, BARRIER_1],
        &[ACCEPTED_1, DELTA, BARRIER_2],
        &[ACCEPTED_2, CALL, ENDED_1, CREATED_2, DONE_2, BARRIER_3],
        &[CREATED_3, DONE_3],
        &[CREATED_4, EMPTY_4],
    ];
    let proxy = proxy(
        Profile::Chatgpt,
        Script {
            ws: batches.iter().map(|b| batch(b)).collect(),
            ..Script::default()
        },
        STEERING,
    )
    .await;
    let mut socket = connect(&proxy).await;
    let sent = [
        client_frame("ws-create-first", "gpt-5.5"),
        STEER_1.to_owned(),
        STEER_2.to_owned(),
        client_frame("ws-create-continuation", "gpt-5.5"),
        client_frame("ws-create-first", "gpt-5.5"),
    ];
    let until: [&[&str]; 5] = [
        &["test.delivery_barrier"],
        &["test.delivery_barrier"],
        &["test.delivery_barrier"],
        &TERMINAL,
        &TERMINAL,
    ];
    for ((frame, want), until) in sent.iter().zip(batches).zip(until) {
        socket.send(Message::text(frame.clone())).await.unwrap();
        let got = read(&mut socket, until).await;
        assert_eq!(got.frames, want, "after {frame}: as upstream sent them");
        assert_eq!(got.closed, None, "after {frame}");
    }
    assert_eq!(proxy.upstream.sockets().len(), 1, "one upstream socket");
    assert_eq!(upstream_frames(&proxy, 0), sent, "the client's frames, byte for byte");
    assert!(proxy.upstream.http().is_empty(), "no HTTP request");
}

/// Each event as an SSE event, for a turn served over HTTP.
fn as_sse(frames: &[String]) -> String {
    frames
        .iter()
        .map(|f| format!("event: {}\ndata: {f}\n\n", cpa_common::rawjson::get(f, "type").str()))
        .collect()
}

/// #25 and #7: every `response.incomplete` the provider sends (the hand-written probe:
/// absent, null, empty, unknown and named reasons with zero output, missing usage, opaque
/// members and raw spellings, partial output with partial usage) reaches the client as
/// sent, at every stage of the response: as the turn's first event, after a delivered
/// `response.created`, and after output. It does on an ordinary upstream socket, with
/// bootstrap buffering holding the leading events, on a `codex.response-steering`
/// connection, and on a turn served over HTTP with and without buffering. The pinned
/// client reads each end itself (`codex-api/src/sse/responses.rs:451-506` at
/// 44ccf394573c), so the proxy invents no failure, completion, usage or output for it.
/// The connection stays open, the next create runs on the same upstream socket (or a
/// second HTTP request), and nothing is replayed.
#[tokio::test]
async fn every_incomplete_reaches_websocket_clients_as_sent() {
    const CREATED: &str =
        r#"{"type":"response.created","response":{"id":"resp_inc","status":"in_progress","output":[]}}"#;
    const DELTA: &str = r#"{"type":"response.output_text.delta","item_id":"msg_inc_1","output_index":0,"content_index":0,"delta":"Hel"}"#;
    const CREATED_2: &str =
        r#"{"type":"response.created","response":{"id":"resp_next","status":"in_progress","output":[]}}"#;
    const DONE_2: &str = r#"{"type":"response.completed","response":{"id":"resp_next","status":"completed","output":[],"usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}}}"#;
    const BUFFERING: &str = "codex:\n  stream-bootstrap-buffering: true\n";
    let next = vec![CREATED_2.to_owned(), DONE_2.to_owned()];
    let ends = events("probe-sse-incomplete-ends");
    assert_eq!(ends.len(), 11);
    for end in &ends {
        for leading in [&[][..], &[CREATED][..], &[CREATED, DELTA][..]] {
            let mut first: Vec<String> = leading.iter().map(|f| (*f).to_owned()).collect();
            first.push(end.clone());
            for (transport, config, websockets) in [
                ("socket", "", true),
                ("socket, buffering", BUFFERING, true),
                ("steering", STEERING, true),
                ("http", "", false),
                ("http, buffering", BUFFERING, false),
            ] {
                let case = format!("{transport} after {} events: {end}", leading.len());
                let script = if websockets {
                    Script {
                        ws: [&first, &next]
                            .iter()
                            .map(|batch| batch.iter().map(|f| Bytes::from(f.clone())).collect())
                            .collect(),
                        ..Script::default()
                    }
                } else {
                    Script {
                        http: [&first, &next]
                            .iter()
                            .map(|batch| cpa_server::testing::codex_capture::Reply::sse(as_sse(batch)))
                            .collect(),
                        ..Script::default()
                    }
                };
                let proxy = proxy_with(script, config, |url| {
                    vec![credential(Profile::Chatgpt, url, websockets)]
                })
                .await;
                let mut socket = connect(&proxy).await;
                let create = client_frame("ws-create-first", "gpt-5.5");
                // On a steering connection a first response that ends without
                // `response.created` still lets the next create through
                // (`first_response_ends_without_created_on_a_steering_connection`).
                for want in [&first, &next] {
                    socket.send(Message::text(create.clone())).await.unwrap();
                    let got = read(&mut socket, &TERMINAL).await;
                    assert_eq!(&got.frames, want, "{case}: as upstream sent them");
                    assert_eq!(got.closed, None, "{case}");
                }
                if websockets {
                    assert_eq!(proxy.upstream.sockets().len(), 1, "{case}: one upstream socket");
                    assert_eq!(upstream_frames(&proxy, 0), [create.clone(), create], "{case}");
                    assert!(proxy.upstream.http().is_empty(), "{case}");
                } else {
                    assert_eq!(proxy.upstream.http().len(), 2, "{case}: one request per turn");
                    assert!(proxy.upstream.sockets().is_empty(), "{case}");
                }
            }
        }
    }
}

/// #25: a remote compaction v2 answer on a WebSocket turn. The pinned client sends the
/// `compaction_trigger` request through its ordinary Responses stream, WebSocket
/// included, and requires one `compaction` output item before `response.completed`
/// (`core/src/compact_remote_v2.rs:291-492` at 44ccf394573c). The first-party test's
/// answer and the opaque probe reach the client frame for frame, on an upstream socket
/// and over HTTP, and the request frame reaches the socket as sent.
#[tokio::test]
async fn compaction_v2_answers_pass_unchanged_on_websocket_turns() {
    let create = as_create("http-request-compaction", "gpt-5.5");
    for name in ["sse-compaction-v2", "probe-sse-compaction-v2-opaque"] {
        let answer = events(name);
        for profile in [Profile::Chatgpt, Profile::Compatible] {
            for websockets in [true, false] {
                let case = format!("{name} {profile:?} websockets={websockets}");
                let script = if websockets {
                    Script {
                        ws: replies(&[name]),
                        ..Script::default()
                    }
                } else {
                    Script {
                        http: vec![cpa_server::testing::codex_capture::Reply::sse(fixture(name).bytes())],
                        ..Script::default()
                    }
                };
                let proxy = proxy_with(script, "", |url| vec![credential(profile, url, websockets)]).await;
                let mut socket = connect(&proxy).await;
                socket.send(Message::text(create.clone())).await.unwrap();
                let got = read(&mut socket, &TERMINAL).await;
                assert_eq!(got.frames, answer, "{case}");
                assert_eq!(got.closed, None, "{case}");
                if websockets {
                    assert_eq!(upstream_frames(&proxy, 0), std::slice::from_ref(&create), "{case}");
                } else {
                    assert_eq!(proxy.upstream.http().len(), 1, "{case}");
                }
            }
        }
    }
}

/// An upstream WebSocket the first-terminal tests drive: it answers the first client
/// frame only once the test releases it, and each later frame at once, with `replies`
/// in order. Its handshake names `openai-model`, so the proxy's metadata event shows
/// which request each response belongs to. `seen` counts the client frames it read.
struct Gated {
    base_url: String,
    frames: Arc<std::sync::Mutex<Vec<String>>>,
    seen: tokio::sync::watch::Receiver<usize>,
    release: Arc<tokio::sync::Notify>,
}

impl Gated {
    async fn start(replies: Vec<Vec<String>>) -> Self {
        use axum::extract::ws::{Message as Upstream, WebSocketUpgrade};
        let frames = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (count, seen) = tokio::sync::watch::channel(0usize);
        let release = Arc::new(tokio::sync::Notify::new());
        let shared = (frames.clone(), Arc::new(count), release.clone(), Arc::new(replies));
        let app = axum::Router::new().fallback(move |upgrade: WebSocketUpgrade| {
            let (frames, count, release, replies) = shared.clone();
            async move {
                let mut response = upgrade.on_upgrade(move |mut socket| async move {
                    let mut n = 0;
                    while let Some(Ok(message)) = socket.recv().await {
                        let Upstream::Text(text) = message else { continue };
                        frames.lock().unwrap().push(text.as_str().to_owned());
                        count.send_replace(n + 1);
                        if n == 0 {
                            release.notified().await;
                        }
                        for reply in replies.get(n).into_iter().flatten() {
                            if socket.send(Upstream::Text(reply.clone().into())).await.is_err() {
                                return;
                            }
                        }
                        n += 1;
                    }
                });
                response
                    .headers_mut()
                    .insert("openai-model", axum::http::HeaderValue::from_static("gpt-5.5"));
                response
            }
        });
        Self {
            base_url: format!("http://{}", serve(app).await),
            frames,
            seen,
            release,
        }
    }

    /// Waits until the upstream read `n` client frames, and returns them.
    async fn read(&mut self, n: usize) -> Vec<String> {
        tokio::time::timeout(Duration::from_secs(5), self.seen.wait_for(|seen| *seen >= n))
            .await
            .unwrap_or_else(|_| panic!("upstream read {:?}, not {n} frames", self.frames.lock().unwrap()))
            .unwrap();
        self.frames.lock().unwrap().clone()
    }
}

/// Waits until `n` usage records were published, and returns them.
async fn records(proxy: &Proxy, n: usize) -> Vec<serde_json::Value> {
    let mut records = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while records.len() < n {
            let queue = proxy.rt.usage_queue();
            records.extend(queue.pop_oldest(10).iter().map(|r| serde_json::from_slice(r).unwrap()));
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{} of {n} usage records: {records:?}", records.len()));
    records
}

fn tokens(record: &serde_json::Value) -> [i64; 3] {
    ["input_tokens", "output_tokens", "total_tokens"].map(|k| record["tokens"][k].as_i64().unwrap_or(-1))
}

/// #25 and #7, on a `codex.response-steering` connection: upstream ends the first
/// response with its terminal event and no `response.created` (an interrupted
/// `response.incomplete` with usage 3/0/3, or a `response.completed`). The client gets
/// that event as sent. The duplex then settles the bootstrap create with it and reads
/// the client's next frames, as after a first response that started. A next create runs
/// on the same upstream socket, whether the client sent it after the terminal or while
/// the first response still ran. An interrupt and a graceful close still work. With
/// usage on, the first response's record is published once, with the usage its terminal
/// reported, before anything else happens. The next create gets a record of its own.
/// Without usage, nothing is published. Nothing is invented: no `response.created`, no
/// replay and no second socket. The proxy's metadata event goes before the opening
/// request's first event and before the next create's `response.created`. The same
/// cases with a `response.created` before the terminal are the control.
#[tokio::test]
async fn first_response_ends_without_created_on_a_steering_connection() {
    const CREATED: &str =
        r#"{"type":"response.created","response":{"id":"resp_ft_1","status":"in_progress","output":[]}}"#;
    const INTERRUPTED: &str = r#"{"type":"response.incomplete","response":{"id":"resp_ft_1","status":"incomplete","incomplete_details":{"reason":"interrupted"},"output":[],"usage":{"input_tokens":3,"output_tokens":0,"total_tokens":3}}}"#;
    const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"resp_ft_1","status":"completed","output":[],"usage":{"input_tokens":3,"output_tokens":0,"total_tokens":3}}}"#;
    const INTERRUPT: &str = r#"{"type":"response.interrupt","response_id":"resp_ft_1","mode":"discard_partial_items"}"#;
    const ACK: &str = r#"{"type":"response.interrupt.accepted","response_id":"resp_ft_1"}"#;
    const CREATED_2: &str =
        r#"{"type":"response.created","response":{"id":"resp_ft_2","status":"in_progress","output":[]}}"#;
    const DONE_2: &str = r#"{"type":"response.completed","response":{"id":"resp_ft_2","status":"completed","output":[],"usage":{"input_tokens":5,"output_tokens":2,"total_tokens":7}}}"#;
    const METADATA: &str = r#"{"type":"response.metadata","headers":{"openai-model":"gpt-5.5"}}"#;
    let owned = |frames: &[&str]| frames.iter().map(|f| (*f).to_owned()).collect::<Vec<_>>();
    let create = client_frame("ws-create-first", "gpt-5.5");
    let next = client_frame("ws-create-continuation", "gpt-5.5");
    for end in [INTERRUPTED, COMPLETED] {
        for created in [false, true] {
            for usage in [false, true] {
                for action in ["create after", "create before", "interrupt", "close"] {
                    let case = format!("created={created} usage={usage} {action}: {end}");
                    let first = if created { owned(&[CREATED, end]) } else { owned(&[end]) };
                    let second = if action == "interrupt" {
                        owned(&[ACK])
                    } else {
                        owned(&[CREATED_2, DONE_2])
                    };
                    let mut upstream = Gated::start(vec![first.clone(), second.clone()]).await;
                    let proxy = proxy_with(Script::default(), STEERING, |_| {
                        vec![credential(Profile::Chatgpt, &upstream.base_url, true)]
                    })
                    .await;
                    if usage {
                        proxy.rt.usage_queue().configure(
                            true,
                            &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
                        );
                    }
                    let mut socket = connect(&proxy).await;
                    socket.send(Message::text(create.clone())).await.unwrap();
                    assert_eq!(upstream.read(1).await, std::slice::from_ref(&create), "{case}");
                    if action == "create before" {
                        // Sent while the first response still runs: the duplex holds it.
                        socket.send(Message::text(next.clone())).await.unwrap();
                    }
                    upstream.release.notify_one();
                    // Barrier: the client has the first response's terminal event.
                    let got = read(&mut socket, &TERMINAL).await;
                    let mut want = vec![METADATA.to_owned()];
                    want.extend(first);
                    assert_eq!(got.frames, want, "{case}: the first response as sent");
                    assert_eq!(got.closed, None, "{case}");
                    if usage {
                        // Settled at the terminal, before any further client frame.
                        let first = records(&proxy, 1).await;
                        assert_eq!(first.len(), 1, "{case}: {first:?}");
                        assert_eq!(first[0]["failed"], false, "{case}: {first:?}");
                        assert_eq!(tokens(&first[0]), [3, 0, 3], "{case}: {first:?}");
                    }
                    match action {
                        "create after" | "create before" => {
                            if action == "create after" {
                                socket.send(Message::text(next.clone())).await.unwrap();
                            }
                            assert_eq!(
                                upstream.read(2).await,
                                [create.clone(), next.clone()],
                                "{case}: the next create on the same socket, as sent"
                            );
                            let got = read(&mut socket, &TERMINAL).await;
                            assert_eq!(got.frames, owned(&[METADATA, CREATED_2, DONE_2]), "{case}");
                            assert_eq!(got.closed, None, "{case}");
                            if usage {
                                let second = records(&proxy, 1).await;
                                assert_eq!(second.len(), 1, "{case}: {second:?}");
                                assert_eq!(second[0]["failed"], false, "{case}: {second:?}");
                                assert_eq!(tokens(&second[0]), [5, 2, 7], "{case}: {second:?}");
                            }
                        }
                        "interrupt" => {
                            socket.send(Message::text(INTERRUPT)).await.unwrap();
                            assert_eq!(upstream.read(2).await, [create.clone(), INTERRUPT.to_owned()], "{case}");
                            let got = read(&mut socket, &["response.interrupt.accepted"]).await;
                            assert_eq!(got.frames, [ACK], "{case}");
                            assert_eq!(got.closed, None, "{case}");
                        }
                        _ => {}
                    }
                    socket.send(Message::Close(None)).await.unwrap();
                    // Barrier: the proxy ended the connection, and with it the duplex.
                    let got = read(&mut socket, &[]).await;
                    assert!(got.frames.is_empty(), "{case}: {got:?}");
                    assert!(got.closed.is_some(), "{case}: {got:?}");
                    assert_eq!(
                        proxy.upstream.sockets().len(),
                        0,
                        "{case}: the capture upstream is unused"
                    );
                    assert_eq!(
                        upstream.seen.borrow().to_owned(),
                        if action == "close" { 1 } else { 2 },
                        "{case}"
                    );
                    let rest = proxy.rt.usage_queue().pop_oldest(10);
                    assert!(rest.is_empty(), "{case}: no record after the close: {rest:?}");
                }
            }
        }
    }
}

/// #25 (F1, F3) on every WebSocket transport: upstream events the pinned client does not
/// decode (a repeated known member in either order, a known member of the wrong type,
/// invalid JSON that would read as a failure, a context-window `error` or a completion)
/// reach the client as sent, one text frame each, and never end or fail the turn. The
/// real completion after them follows, the connection stays open and the credential
/// does not cool. Invalid JSON whose failure reading would be an account failure is
/// withheld, because such a frame can name the pooled account (#6). It is not
/// classified either, so the turn still ends with the completion. A frame that is not a
/// `ResponsesStreamEvent` but is the WebSocket reader's wrapped status `error`
/// (`endpoint/responses_websocket.rs:864-884` at 44ccf394573c) still fails a socket
/// turn as that status; the SSE reader has no such reading, so over HTTP it is an
/// undecodable event. As the first event and after `response.created`, on an upstream
/// socket with and without bootstrap buffering, on a steering connection, and on a turn
/// served over HTTP with and without buffering.
#[tokio::test]
async fn undecodable_events_never_end_or_fail_websocket_turns() {
    const CREATED: &str = r#"{"type":"response.created","response":{"id":"resp_real"}}"#;
    const REAL_END: &str =
        r#"{"type":"response.completed","response":{"id":"resp_real","status":"completed","output":[]}}"#;
    const BUFFERING: &str = "codex:\n  stream-bootstrap-buffering: true\n";
    let account = r#"{"type":"response.failed","response":{"error":{"code":"rate_limit_exceeded","message":"limit for acct-FAKE"}}"#;
    let wrapped = r#"{"type":"error","status":429,"error":{"code":"rate_limit_exceeded","message":"limit for acct-FAKE"},"delta":5}"#;
    let cases = [
        r#"{"type":"response.completed","type":"response.future","response":{"id":"resp_dup","output":[]}}"#,
        r#"{"type":"response.future","type":"response.completed","response":{"id":"resp_dup","output":[]}}"#,
        r#"{"type":"response.completed","response":{"id":"resp_old","output":[]},"response":{"id":"resp_new","output":[]}}"#,
        r#"{"type":"response.failed","type":"response.future","response":{"error":{"code":"server_error","message":"m"}}}"#,
        r#"{"type":"response.completed","delta":5,"response":{"id":"resp_typed","output":[]}}"#,
        r#"{"type":"response.failed","response":{"error":{"code":"server_error","message":"broken"}}"#,
        r#"{"type":"error","error":{"code":"context_length_exceeded","message":"broken"}"#,
        r#"{"type":"response.completed","response":{"id":"resp_bad","output":[]}"#,
        account,
        wrapped,
    ];
    for event in cases {
        for leading in [&[][..], &[CREATED][..]] {
            for (transport, config, websockets) in [
                ("socket", "", true),
                ("socket, buffering", BUFFERING, true),
                ("steering", STEERING, true),
                ("http", "", false),
                ("http, buffering", BUFFERING, false),
            ] {
                let case = format!("{transport} after {} events: {event}", leading.len());
                let frames: Vec<&str> = leading.iter().copied().chain([event, REAL_END]).collect();
                let script = if websockets {
                    Script {
                        ws: vec![frames.iter().map(|f| Bytes::from(f.to_string())).collect()],
                        ..Script::default()
                    }
                } else {
                    let sse: String = frames.iter().map(|f| format!("data: {f}\n\n")).collect();
                    Script {
                        http: vec![cpa_server::testing::codex_capture::Reply::sse(sse)],
                        ..Script::default()
                    }
                };
                let proxy = proxy_with(script, config, |url| {
                    vec![credential(Profile::Chatgpt, url, websockets)]
                })
                .await;
                let mut socket = connect(&proxy).await;
                socket
                    .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
                    .await
                    .unwrap();
                // The reader's own `type` lookup takes a repeated key's first value, so
                // it reads on until the real completion or a close.
                let mut got = Read::default();
                while got.closed.is_none() && got.frames.last().map(String::as_str) != Some(REAL_END) {
                    let more = read(&mut socket, &["response.completed"]).await;
                    got.frames.extend(more.frames);
                    got.closed = more.closed;
                }
                let credential = proxy.rt.store().snapshot()[0].id.clone();
                if event == wrapped && websockets {
                    // The wrapped 429 is an account failure: never delivered as sent, and it
                    // ends the turn; the completion after it is not read.
                    assert!(
                        !got.frames.iter().any(|f| f == wrapped || f == REAL_END),
                        "{case}: {got:?}"
                    );
                    assert!(got.closed.is_some(), "{case}: {got:?}");
                    continue;
                }
                let mut want: Vec<String> = leading.iter().map(|f| (*f).to_owned()).collect();
                if event != account && event != wrapped {
                    want.push(event.to_owned());
                }
                want.push(REAL_END.to_owned());
                assert_eq!(got.frames, want, "{case}");
                assert_eq!(got.closed, None, "{case}");
                if websockets {
                    assert_eq!(proxy.upstream.sockets().len(), 1, "{case}: one socket");
                } else {
                    assert_eq!(proxy.upstream.http().len(), 1, "{case}: one attempt");
                }
                assert!(proxy.rt.store().cooldowns(&credential).is_empty(), "{case}");
            }
        }
    }
}
