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
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
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
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
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

/// A Codex credential without `websockets` serves the WebSocket route over HTTP. That is
/// an explicit conversion, not the native route: the turn goes upstream as an HTTP
/// Responses request built by the legacy path, and a prewarm is answered locally with a
/// `resp_prewarm_` response that no provider produced.
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
    let warm = read(&mut socket, &TERMINAL).await;
    let done = warm.frames.last().unwrap();
    assert!(
        gjson::get(done, "response.id").str().starts_with("resp_prewarm_"),
        "a locally fabricated prewarm: {done}"
    );
    assert!(proxy.upstream.http().is_empty(), "the prewarm stays local");

    let create = client_frame("ws-create-first", "gpt-5.5");
    socket.send(Message::text(create.clone())).await.unwrap();
    let turn = read(&mut socket, &TERMINAL).await;
    assert_eq!(
        gjson::get(turn.frames.last().unwrap(), "type").str(),
        "response.completed"
    );
    assert!(proxy.upstream.sockets().is_empty(), "no upstream socket");
    let sent = proxy.upstream.http().pop().expect("an HTTP request");
    let body = text(sent.body);
    assert_ne!(body, create, "the HTTP conversion, not the client's frame");
    assert!(!body.contains(r#""type":"response.create""#), "{body}");
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
