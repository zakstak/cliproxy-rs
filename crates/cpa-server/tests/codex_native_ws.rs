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

/// A positional handshake is still a handshake: hold it for bootstrap failover until
/// content is delivered, just as its object representation is held. No replay after a
/// delta, on either the socket or HTTP upstream path.
#[tokio::test]
async fn positional_handshakes_keep_bootstrap_failover_before_delivery() {
    const OBJECT: &str = r#"{"type":"response.created","response":{"id":"r1"}}"#;
    const POSITIONAL: &str =
        r#"["response.created",null,null,{"id":"r1"},null,null,null,null,null,null,null,null,null,null]"#;
    const DELTA: &str = r#"{"type":"response.output_text.delta","delta":"started"}"#;
    const FAILED: &str = r#"{"type":"response.failed","response":{"id":"r1","error":{"code":"server_is_overloaded","message":"try next"}}}"#;
    const GOOD: &str = r#"{"type":"response.completed","response":{"id":"healthy","output":[],"usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}}}"#;
    for created in [OBJECT, POSITIONAL] {
        for content in [false, true] {
            for (sockets, ws_client) in [(true, true), (false, true), (false, false)] {
                let case = format!("sockets={sockets}, ws_client={ws_client}, content={content}: {created}");
                let mut leading = vec![created];
                if content {
                    leading.push(DELTA);
                }
                leading.push(FAILED);
                let batch = |frames: &[&str]| frames.iter().map(|f| Bytes::from((*f).to_owned())).collect();
                let sse = |frames: &[&str]| frames.iter().map(|f| format!("data: {f}\n\n")).collect::<String>();
                let script = if sockets {
                    Script {
                        ws: vec![batch(&leading), batch(&[GOOD])],
                        ..Script::default()
                    }
                } else {
                    Script {
                        http: vec![
                            cpa_server::testing::codex_capture::Reply::sse(sse(&leading)),
                            cpa_server::testing::codex_capture::Reply::sse(sse(&[GOOD])),
                        ],
                        ..Script::default()
                    }
                };
                let proxy = proxy_with(script, "codex:\n  stream-bootstrap-buffering: true\n", |url| {
                    let mut second = credential(Profile::Chatgpt, url, sockets);
                    second.id = "codex-second.json".into();
                    second.metadata.insert("access_token".into(), "at-SECOND".into());
                    vec![credential(Profile::Chatgpt, url, sockets), second]
                })
                .await;
                let got = if ws_client {
                    let mut socket = connect(&proxy).await;
                    socket
                        .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
                        .await
                        .unwrap();
                    let got = read(&mut socket, &["response.completed"]).await;
                    if !content {
                        assert_eq!(got.frames, [GOOD], "{case}: {got:?}");
                    }
                    let _ = socket.close(1000, "done").await;
                    got.frames.join("\n")
                } else {
                    wreq::Client::new()
                        .post(format!("http://{}/v1/responses", proxy.addr))
                        .bearer_auth("client-key")
                        .header("content-type", "application/json")
                        .body(r#"{"model":"gpt-5.5","input":[],"stream":true}"#)
                        .send()
                        .await
                        .unwrap()
                        .text()
                        .await
                        .unwrap()
                };
                let attempts = if sockets {
                    proxy.upstream.sockets().len()
                } else {
                    proxy.upstream.http().len()
                };
                assert_eq!(attempts, if content { 1 } else { 2 }, "{case}: {got}");
                assert_eq!(got.contains(GOOD), !content, "{case}: {got}");
                if content {
                    assert!(got.contains(created) && got.contains(DELTA), "{case}: {got}");
                } else {
                    assert!(!got.contains(created) && !got.contains(FAILED), "{case}: {got}");
                }
            }
        }
    }
}

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
    release_second: Arc<tokio::sync::Notify>,
}

impl Gated {
    async fn start(replies: Vec<Vec<String>>, hold_second: bool) -> Self {
        use axum::extract::ws::{Message as Upstream, WebSocketUpgrade};
        let frames = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (count, seen) = tokio::sync::watch::channel(0usize);
        let release = Arc::new(tokio::sync::Notify::new());
        let release_second = Arc::new(tokio::sync::Notify::new());
        let shared = (
            frames.clone(),
            Arc::new(count),
            release.clone(),
            release_second.clone(),
            Arc::new(replies),
        );
        let app = axum::Router::new().fallback(move |upgrade: WebSocketUpgrade| {
            let (frames, count, release, release_second, replies) = shared.clone();
            async move {
                let mut response = upgrade.on_upgrade(move |mut socket| async move {
                    let mut n = 0;
                    while let Some(Ok(message)) = socket.recv().await {
                        let Upstream::Text(text) = message else { continue };
                        frames.lock().unwrap().push(text.as_str().to_owned());
                        count.send_replace(n + 1);
                        if n == 0 {
                            release.notified().await;
                        } else if n == 1 && hold_second {
                            release_second.notified().await;
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
            release_second,
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

/// Once the steering bootstrap ended, a local refusal or an upstream rejection before
/// the next `response.created` is recoverable, even if the bootstrap had no created
/// event either. The valid successor proves the relay kept the socket open, not just
/// that it delivered the error before closing. The after-created cases are controls.
#[tokio::test]
async fn a_first_terminal_keeps_local_refusals_recoverable() {
    first_terminal_error_recovery(false).await;
}

#[tokio::test]
async fn a_first_terminal_keeps_upstream_rejections_recoverable() {
    first_terminal_error_recovery(true).await;
}

async fn first_terminal_error_recovery(reject_upstream: bool) {
    const CREATED: &str = r#"{"type":"response.created","response":{"id":"relay_first","output":[]}}"#;
    const INCOMPLETE: &str = r#"{"type":"response.incomplete","response":{"id":"relay_first","incomplete_details":{"reason":"interrupted"},"output":[],"usage":{"input_tokens":3,"output_tokens":0,"total_tokens":3}}}"#;
    const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"relay_first","output":[],"usage":{"input_tokens":3,"output_tokens":0,"total_tokens":3}}}"#;
    const APPEND: &str = r#"{"type":"response.append","input":[]}"#;
    const REJECTED: &str = r#"{"type":"error","status":400,"response_id":"rejected","error":{"type":"invalid_request_error","message":"correct this create","future":null}}"#;
    const CREATED_NEXT: &str = r#"{"type":"response.created","response":{"id":"relay_next","output":[]}}"#;
    const DONE_NEXT: &str = r#"{"type":"response.completed","response":{"id":"relay_next","output":[],"usage":{"input_tokens":5,"output_tokens":2,"total_tokens":7}}}"#;
    const METADATA: &str = r#"{"type":"response.metadata","headers":{"openai-model":"gpt-5.5"}}"#;
    let owned = |frames: &[&str]| frames.iter().map(|f| (*f).to_owned()).collect::<Vec<_>>();
    let create = client_frame("ws-create-first", "gpt-5.5");
    let next = r#"{"type":"response.create","model":"gpt-5.5","previous_response_id":"relay_first","input":[],"service_tier":"flex"}"#;
    for end in [INCOMPLETE, COMPLETED] {
        for created in [false, true] {
            let case = format!("created={created} upstream={reject_upstream}: {end}");
            let first = if created { owned(&[CREATED, end]) } else { owned(&[end]) };
            let mut replies = vec![first.clone()];
            if reject_upstream {
                replies.push(owned(&[REJECTED]));
            }
            replies.push(owned(&[CREATED_NEXT, DONE_NEXT]));
            let mut upstream = Gated::start(replies, false).await;
            let proxy = proxy_with(Script::default(), STEERING, |_| {
                vec![credential(Profile::Chatgpt, &upstream.base_url, true)]
            })
            .await;
            proxy.rt.usage_queue().configure(
                true,
                &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
            );
            let mut socket = connect(&proxy).await;
            socket.send(Message::text(create.clone())).await.unwrap();
            assert_eq!(upstream.read(1).await, std::slice::from_ref(&create), "{case}");
            upstream.release.notify_one();
            let got = read(&mut socket, &TERMINAL).await;
            let mut want = owned(&[METADATA]);
            want.extend(first);
            assert_eq!(got.frames, want, "{case}");
            assert_eq!(got.closed, None, "{case}");
            let first_record = records(&proxy, 1).await;
            assert_eq!(first_record.len(), 1, "{case}");
            assert_eq!(first_record[0]["failed"], false, "{case}");
            assert_eq!(tokens(&first_record[0]), [3, 0, 3], "{case}");

            socket
                .send(Message::text(if reject_upstream { next } else { APPEND }))
                .await
                .unwrap();
            let error = read(&mut socket, &["error"]).await;
            assert_eq!(error.closed, None, "{case}: {error:?}");
            assert_eq!(error.frames.len(), 1, "{case}: {error:?}");
            if reject_upstream {
                assert_eq!(error.frames, [REJECTED], "{case}");
                assert_eq!(upstream.read(2).await, [create.clone(), next.to_owned()], "{case}");
                let rejected = records(&proxy, 1).await;
                assert_eq!(rejected.len(), 1, "{case}");
                assert_eq!(rejected[0]["failed"], true, "{case}");
            } else {
                let refusal: serde_json::Value = serde_json::from_str(&error.frames[0]).unwrap();
                assert_eq!(refusal["status"], 400, "{case}");
                assert_eq!(refusal["error"]["message"], cpa_exec::codex::APPEND_REFUSED, "{case}");
                assert_eq!(
                    *upstream.frames.lock().unwrap(),
                    std::slice::from_ref(&create),
                    "{case}"
                );
            }
            socket.send(Message::text(next)).await.unwrap();
            let count = if reject_upstream { 3 } else { 2 };
            let sent = upstream.read(count).await;
            assert_eq!(sent[0], create, "{case}");
            assert!(sent[1..].iter().all(|frame| frame == next), "{case}: {sent:?}");
            let successor = read(&mut socket, &TERMINAL).await;
            assert_eq!(successor.frames, owned(&[METADATA, CREATED_NEXT, DONE_NEXT]), "{case}");
            assert_eq!(successor.closed, None, "{case}");
            let final_record = records(&proxy, 1).await;
            assert_eq!(final_record.len(), 1, "{case}");
            assert_eq!(final_record[0]["failed"], false, "{case}");
            assert_eq!(tokens(&final_record[0]), [5, 2, 7], "{case}");
            socket.send(Message::Close(None)).await.unwrap();
            assert!(read(&mut socket, &[]).await.closed.is_some(), "{case}");
            assert!(
                proxy.rt.usage_queue().pop_oldest(10).is_empty(),
                "{case}: no extra record"
            );
            let credential = proxy.rt.store().snapshot()[0].id.clone();
            assert!(proxy.rt.store().cooldowns(&credential).is_empty(), "{case}");
            assert!(proxy.upstream.sockets().is_empty(), "{case}: no other upstream socket");
            assert!(proxy.upstream.http().is_empty(), "{case}: no HTTP fallback");
        }
    }
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
                    // The early next create may already reach upstream. Hold its reply
                    // until the first record has been checked, so that draining the
                    // queue cannot race two valid response records against an expected one.
                    let mut upstream =
                        Gated::start(vec![first.clone(), second.clone()], action.starts_with("create")).await;
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
                            upstream.release_second.notify_one();
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

/// What a WebSocket client sees for one upstream frame followed by a real completion.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Outcome {
    /// The frame and the completion reach the client as sent; nothing cools.
    Passes,
    /// The frame is withheld (#6), the completion still arrives, nothing cools.
    Withheld,
    /// A request-scoped failure: the frame reaches the client as sent and ends the turn;
    /// nothing cools and the completion after it is not read.
    FailsAsSent,
    /// A terminal provider failure reaches the client as sent and cools the model.
    CooledProviderFailure,
    /// An account failure: the frame never reaches the client, the turn ends and the
    /// credential cools.
    AccountFailure,
}

/// #25 (E1-E4) and #6: the pinned client's readings, not the proxy's, decide what a frame
/// does to a WebSocket turn, as the first event, on an upstream socket with and without
/// bootstrap buffering, on a steering connection, and on a turn served over HTTP with and
/// without buffering:
///
/// - E1: the first-party build's `serde_json::Value` (`arbitrary_precision`,
///   `raw_value`) refuses an object whose first key is a private number or raw-value
///   token with anything but its one valid value, so such an event does not decode.
/// - E2: a failure is classified as its pinned `Value` holds it, with the last of a
///   repeated key. A request fault first and an account failure last is an account
///   failure; its frame, with the account and the credential's own token in it, never
///   reaches the client. The reverse order is the request fault the client reads, sent
///   as it arrived. An undecodable frame that would read as an account failure with
///   either its first or its last repeated key is withheld, and nothing cools.
/// - E3: the pinned wrapped mapping (`responses_websocket.rs:712-764`) decides a wrapped
///   frame: a status that maps to no error (2xx, outside 100-999) is a notice that ends
///   nothing and cools nothing; a retryable code fails the turn without a status and
///   reaches the client as sent. The SSE reader has no wrapped reading, so these hold for
///   upstream sockets; over HTTP an undecodable one is an undecodable event and passes.
/// - E4: a decoded `response.completed` without a `response`, and a `response.done`, are
///   no terminal event for the pinned reader, so the turn reads on.
#[tokio::test]
async fn pinned_readings_decide_websocket_turns() {
    use Outcome::*;
    const REAL_END: &str =
        r#"{"type":"response.completed","response":{"id":"resp_real","status":"completed","output":[]}}"#;
    const BUFFERING: &str = "codex:\n  stream-bootstrap-buffering: true\n";
    // The credential's own token (`credential`), standing in for a leaked secret.
    const TOKEN: &str = "at-FAKE";
    const ACCOUNT: &str = "acct-PRIVATE";
    let cases: [(&str, Outcome, bool); 25] = [
        // E1.
        (
            r#"{"type":"response.completed","response":{"id":"bad","future":{"$serde_json::private::Number":"bad"}}}"#,
            Passes,
            false,
        ),
        (
            r#"{"type":"response.completed","response":{"id":"bad","output":[],"n":{"$serde_json::private::Number":"1","x":3}}}"#,
            Passes,
            false,
        ),
        (
            r#"{"type":"response.completed","response":{"id":"bad","output":[{"$serde_json::private::RawValue":"{"}]}}"#,
            Passes,
            false,
        ),
        // Undecodable, but an account failure if it decoded: withheld, nothing cools.
        (
            r#"{"type":"response.failed","error":{"$serde_json::private::Number":"12a"},"response":{"error":{"code":"rate_limit_exceeded","message":"acct-PRIVATE"}}}"#,
            Withheld,
            false,
        ),
        // E4.
        (r#"{"type":"response.completed","response":null}"#, Passes, false),
        (r#"{"type":"response.completed","sequence_number":4}"#, Passes, false),
        (
            r#"{"type":"response.done","response":{"id":"resp_done","status":"completed","output":[]}}"#,
            Passes,
            false,
        ),
        // E3.
        (
            r#"{"type":"error","item_id":23,"status":200,"error":{"code":"other","message":"notice"}}"#,
            Passes,
            false,
        ),
        (
            r#"{"type":"error","status":204,"error":{"code":"other","message":"notice"}}"#,
            Passes,
            true,
        ),
        // A successful wrapped status does not override a terminal provider code
        // decoded from the pinned Value's private raw-value representation.
        (
            r#"{"type":"error","status":200,"error":{"$serde_json::private::RawValue":"{\"code\":\"model_not_found\"}"}}"#,
            CooledProviderFailure,
            true,
        ),
        (
            r#"{"type":"error","status":200,"error":{"code":"model_not_found"}}"#,
            CooledProviderFailure,
            true,
        ),
        (
            r#"{"type":"error","status_code":1000,"error":{"code":"other","message":"notice"}}"#,
            Passes,
            true,
        ),
        (
            r#"{"type":"error","item_id":23,"status":200,"error":{"code":"other","message":"acct-PRIVATE","type":"usage_limit_reached"}}"#,
            Withheld,
            false,
        ),
        (
            r#"{"type":"error","item_id":23,"error":{"code":"previous_response_not_found","message":"resend the full input"}}"#,
            FailsAsSent,
            true,
        ),
        (
            r#"{"type":"error","item_id":23,"status":200,"error":{"code":"previous_response_not_found","message":"resend the full input"}}"#,
            FailsAsSent,
            true,
        ),
        // E2: decoded failures, read with the last repeated key.
        (
            r#"{"type":"response.failed","response":{"id":"r1","error":{"type":"invalid_request_error","type":"rate_limit_error","code":"other","message":"limited","private_account":"acct-PRIVATE","token":"at-FAKE"}}}"#,
            AccountFailure,
            false,
        ),
        (
            r#"{"type":"response.failed","response":{"id":"r1","error":{"code":"context_length_exceeded","code":"invalid_api_key","message":"limited","private_account":"acct-PRIVATE","token":"at-FAKE"}}}"#,
            AccountFailure,
            false,
        ),
        // A repeated `status` is a duplicate field `WrappedWebsocketErrorEvent` refuses
        // (`responses_websocket.rs:692-709`), so it is no wrapped frame, and the pinned
        // `error` branch reads no status (`sse/responses.rs:375-385`): `other` is no
        // terminal provider code, so the reader goes on (review finding 3). The last status
        // reads as an account failure, so the frame is withheld; nothing cools. SSE has no
        // status frame to read.
        (
            r#"{"type":"error","status":400,"status":429,"error":{"code":"other","message":"acct-PRIVATE"}}"#,
            Withheld,
            true,
        ),
        (
            r#"{"type":"response.failed","response":{"id":"r1","error":{"type":"rate_limit_error","type":"invalid_request_error","code":"other","message":"bad input"}}}"#,
            FailsAsSent,
            false,
        ),
        (
            r#"{"type":"response.failed","response":{"id":"r1","error":{"code":"invalid_api_key","code":"context_length_exceeded","message":"context window exceeded"}}}"#,
            FailsAsSent,
            false,
        ),
        (
            r#"{"type":"error","error":{"code":"other","type":"invalid_request_error","type":"usage_limit_reached","message":"acct-PRIVATE"}}"#,
            AccountFailure,
            false,
        ),
        // E2: undecodable envelopes, either reading of the repeated key.
        (
            r#"{"type":"response.future","type":"error","status":429,"error":{"type":"usage_limit_reached","private_account":"acct-PRIVATE","token":"at-FAKE"}}"#,
            Withheld,
            false,
        ),
        (
            r#"{"type":"error","type":"response.future","status":429,"error":{"type":"usage_limit_reached","private_account":"acct-PRIVATE","token":"at-FAKE"}}"#,
            Withheld,
            false,
        ),
        // A decoded wrapped notice keeps the pinned last-key reading: an account `type`
        // only in the middle of a repeated key does not withhold it (the any-occurrence
        // rule is for frames the pinned client cannot decode). Socket only: over HTTP it
        // is a decoded `error` the SSE route classifies.
        (
            r#"{"type":"error","status":200,"error":{"code":"other","type":"x","type":"rate_limit_error","type":"x","message":"notice"}}"#,
            Passes,
            true,
        ),
        // A safe control: the undecodable envelope with no account reading passes.
        (
            r#"{"type":"response.future","type":"error","status":400,"error":{"code":"other","message":"bad input"}}"#,
            Passes,
            false,
        ),
    ];
    for (event, outcome, socket_only) in cases {
        for (transport, config, websockets) in [
            ("socket", "", true),
            ("socket, buffering", BUFFERING, true),
            ("steering", STEERING, true),
            ("http", "", false),
            ("http, buffering", BUFFERING, false),
        ] {
            if socket_only && !websockets {
                continue;
            }
            let case = format!("{transport}: {event}");
            let frames = [event, REAL_END];
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
            let mut got = Read::default();
            while got.closed.is_none() && got.frames.last().map(String::as_str) != Some(REAL_END) {
                let more = read(&mut socket, &["response.completed"]).await;
                got.frames.extend(more.frames);
                got.closed = more.closed;
            }
            assert!(
                !got.frames.iter().any(|f| f.contains(TOKEN)),
                "{case}: the credential's token reached the client: {got:?}"
            );
            let credential = proxy.rt.store().snapshot()[0].id.clone();
            let cooled = !proxy.rt.store().cooldowns(&credential).is_empty();
            match outcome {
                Passes => {
                    assert_eq!(got.frames, [event, REAL_END], "{case}");
                    assert_eq!(got.closed, None, "{case}");
                }
                Withheld => {
                    assert_eq!(got.frames, [REAL_END], "{case}");
                    assert_eq!(got.closed, None, "{case}");
                }
                FailsAsSent | CooledProviderFailure => {
                    assert!(got.frames.iter().any(|f| f == event), "{case}: {got:?}");
                    assert!(!got.frames.iter().any(|f| f == REAL_END), "{case}: {got:?}");
                    assert!(got.closed.is_some(), "{case}: {got:?}");
                }
                AccountFailure => {
                    assert!(
                        !got.frames
                            .iter()
                            .any(|f| f == event || f.contains(ACCOUNT) || f == REAL_END),
                        "{case}: {got:?}"
                    );
                    assert!(got.closed.is_some(), "{case}: {got:?}");
                }
            }
            assert_eq!(
                cooled,
                matches!(outcome, AccountFailure | CooledProviderFailure),
                "{case}: cooldown"
            );
        }
    }
}

/// The relay must recognize the same logical context failure as the executor, including
/// a last duplicate code and a private raw value. The one upstream failure is the only
/// error the client receives; the classified stream error cannot add a local error.
#[tokio::test]
async fn logical_context_failures_are_preserved_without_an_extra_error() {
    const BUFFERING: &str = "codex:\n  stream-bootstrap-buffering: true\n";
    for event in [
        r#"{"type":"response.failed","response":{"id":"r1","error":{"code":"other","code":"context_length_exceeded","message":"too long"}}}"#,
        r#"{"type":"response.failed","response":{"id":"r1","error":{"$serde_json::private::RawValue":"{\"code\":\"context_length_exceeded\",\"message\":\"too long\"}"}}}"#,
    ] {
        for (transport, config, websockets) in [
            ("socket", "", true),
            ("socket, buffering", BUFFERING, true),
            ("steering", STEERING, true),
            ("http", "", false),
            ("http, buffering", BUFFERING, false),
        ] {
            let script = if websockets {
                Script {
                    ws: vec![vec![Bytes::from(event.to_owned())]],
                    ..Script::default()
                }
            } else {
                Script {
                    http: vec![cpa_server::testing::codex_capture::Reply::sse(format!(
                        "data: {event}\n\n"
                    ))],
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
            let got = read(&mut socket, &[]).await;
            assert_eq!(got.frames, [event], "{transport}: {got:?}");
            assert!(got.closed.is_some(), "{transport}: {got:?}");
        }
    }
}

/// #25 (E4) and usage: a `response.done` and a `response.completed` without a `response`
/// end nothing for the pinned reader, so the turn's record keeps the usage of the real
/// completion after them, not the earlier event's. On an upstream socket with and without
/// bootstrap buffering, on a steering connection (whose records settle per response), and
/// on a turn served over HTTP with and without buffering. One record, not failed.
#[tokio::test]
async fn non_terminal_events_leave_the_record_to_the_real_completion() {
    const CREATED: &str = r#"{"type":"response.created","response":{"id":"resp_real"}}"#;
    const REAL_END: &str = r#"{"type":"response.completed","response":{"id":"resp_real","status":"completed","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    const BUFFERING: &str = "codex:\n  stream-bootstrap-buffering: true\n";
    for event in [
        r#"{"type":"response.done","response":{"id":"resp_real","status":"completed","output":[],"usage":{"input_tokens":9,"output_tokens":9,"total_tokens":18}}}"#,
        r#"{"type":"response.completed","response":null,"usage":{"input_tokens":9,"output_tokens":9,"total_tokens":18}}"#,
    ] {
        for (transport, config, websockets) in [
            ("socket", "", true),
            ("socket, buffering", BUFFERING, true),
            ("steering", STEERING, true),
            ("http", "", false),
            ("http, buffering", BUFFERING, false),
        ] {
            let case = format!("{transport}: {event}");
            let frames = [CREATED, event, REAL_END];
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
            proxy.rt.usage_queue().configure(
                true,
                &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
            );
            let mut socket = connect(&proxy).await;
            socket
                .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
                .await
                .unwrap();
            let mut got = Read::default();
            while got.closed.is_none() && got.frames.last().map(String::as_str) != Some(REAL_END) {
                let more = read(&mut socket, &["response.completed"]).await;
                got.frames.extend(more.frames);
                got.closed = more.closed;
            }
            assert_eq!(got.frames, frames, "{case}");
            let records = records(&proxy, 1).await;
            assert_eq!(records.len(), 1, "{case}: {records:?}");
            assert_eq!(records[0]["failed"], false, "{case}: {records:?}");
            assert_eq!(tokens(&records[0]), [4, 1, 5], "{case}: {records:?}");
        }
    }
}

/// #25 (read-on) on WebSocket turns: an event the pinned SSE reader records and reads past
/// (a `response` that does not read as `ResponseCompleted`, or a content filter) is a
/// processing error the pinned WebSocket reader returns at once, ending the turn and
/// retiring its socket (`endpoint/responses_websocket.rs:968-978` and 387-395 at
/// 44ccf394573c). A WebSocket client is that reader, whatever carries the turn upstream,
/// so the turn ends on the event as sent: on an upstream socket with and without
/// bootstrap buffering, and on a turn served over HTTP with and without buffering, where
/// the SSE upstream would have more to read. The completion after it is not delivered to
/// the turn, and the turn's one record carries the event's reported usage (18), not
/// failed. On a steering connection every frame is relayed (the client's reader decides),
/// so the event and the completion both arrive, and the first response's record settles
/// on the event.
#[tokio::test]
async fn read_on_events_end_websocket_turns_as_the_websocket_reader_does() {
    const CREATED: &str = r#"{"type":"response.created","response":{"id":"resp_real"}}"#;
    const REAL_END: &str = r#"{"type":"response.completed","response":{"id":"resp_real","status":"completed","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    const BUFFERING: &str = "codex:\n  stream-bootstrap-buffering: true\n";
    for event in [
        r#"{"type":"response.completed","response":{"status":"completed","output":[],"usage":{"input_tokens":9,"output_tokens":9,"total_tokens":18}}}"#,
        r#"{"type":"response.completed","response":{"id":"resp_real","status":"completed","output":[],"usage":{"input_tokens":9,"output_tokens":9,"total_tokens":18,"output_tokens_details":{}}}}"#,
        r#"{"type":"response.incomplete","response":{"id":"resp_real","status":"incomplete","incomplete_details":{"reason":"content_filter"},"output":[],"usage":{"input_tokens":9,"output_tokens":9,"total_tokens":18}}}"#,
        r#"{"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"interrupted"},"output":[],"usage":{"input_tokens":9,"output_tokens":9,"total_tokens":18}}}"#,
    ] {
        for (transport, config, websockets) in [
            ("socket", "", true),
            ("socket, buffering", BUFFERING, true),
            ("steering", STEERING, true),
            ("http", "", false),
            ("http, buffering", BUFFERING, false),
        ] {
            let case = format!("{transport}: {event}");
            let frames = [CREATED, event, REAL_END];
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
            proxy.rt.usage_queue().configure(
                true,
                &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
            );
            let mut socket = connect(&proxy).await;
            socket
                .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
                .await
                .unwrap();
            let mut got = read(&mut socket, &["response.completed", "response.incomplete"]).await;
            assert_eq!(got.frames, [CREATED, event], "{case}");
            // Whatever else reaches the client before it goes quiet.
            while let Ok(Some(Ok(message))) = tokio::time::timeout(Duration::from_millis(300), socket.next()).await {
                if let Message::Text(text) = message {
                    got.frames.push(text.as_str().to_owned());
                }
            }
            if transport == "steering" {
                assert_eq!(got.frames, frames, "{case}");
            } else {
                assert_eq!(got.frames, [CREATED, event], "{case}: the turn ended on the event");
            }
            let records = records(&proxy, 1).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            let more = proxy.rt.usage_queue().pop_oldest(10);
            assert!(more.is_empty(), "{case}: one record: {records:?} {more:?}");
            assert_eq!(records[0]["failed"], false, "{case}: {records:?}");
            assert_eq!(tokens(&records[0]), [9, 9, 18], "{case}: {records:?}");
            let credential = proxy.rt.store().snapshot()[0].id.clone();
            assert!(proxy.rt.store().cooldowns(&credential).is_empty(), "{case}");
        }
    }
}

/// #25 (E2) and #6, every duplicate reading: a frame the pinned client cannot decode is
/// withheld when any occurrence of a repeated key, not only its first or last, could make
/// it an account failure, because some client reading could act on that occurrence and
/// the frame can name the pooled account or carry the credential's own token. Three and
/// more occurrences with the account reading only in the middle: `type`, `status`,
/// `status_code`, a nested `error.type` and `error.code`, a whole `error` object, an
/// escaped key, a `response.failed` with a nested `response.error`, and invalid JSON.
/// Each is withheld without cooling, usage, acknowledgement or ownership effects: the
/// real completion after it still arrives, the turn's one record carries its usage, and
/// neither the token nor the account marker reaches the client. The safe controls pass as
/// sent: undecodable frames whose no occurrence reads as an account failure, including an
/// account-looking status under an unknown type, failure words inside a string value, and
/// account fields in an unrelated nested object. On an upstream socket with and without
/// bootstrap buffering, on a steering connection, and on a turn served over HTTP with and
/// without buffering.
#[tokio::test]
async fn ambiguous_account_frames_are_withheld_under_every_duplicate_reading() {
    const REAL_END: &str = r#"{"type":"response.completed","response":{"id":"resp_real","status":"completed","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    const BUFFERING: &str = "codex:\n  stream-bootstrap-buffering: true\n";
    // The credential's own token (`credential`), standing in for a leaked secret; the
    // client authenticates with `client-key`.
    const TOKEN: &str = "at-FAKE";
    const ACCOUNT: &str = "acct-PRIVATE";
    let withheld = [
        r#"{"type":"response.future","type":"error","type":"response.future","status":429,"error":{"type":"rate_limit_error","message":"limited","private_account":"acct-PRIVATE","token":"at-FAKE"}}"#,
        r#"{"type":"response.future","type":"error","status":400,"status":429,"status":400,"error":{"code":"other","message":"acct-PRIVATE at-FAKE"}}"#,
        r#"{"type":"response.future","type":"error","status_code":400,"status_code":401,"status_code":400,"error":{"code":"other","message":"acct-PRIVATE at-FAKE"}}"#,
        r#"{"type":"error","type":"response.future","error":{"type":"invalid_request_error","type":"authentication_error","type":"invalid_request_error","message":"acct-PRIVATE","token":"at-FAKE"}}"#,
        r#"{"type":"error","type":"response.future","error":{"code":"other","code":"invalid_api_key","code":"other","message":"acct-PRIVATE","token":"at-FAKE"}}"#,
        r#"{"type":"error","type":"response.future","error":{"code":"other","message":"m"},"error":{"type":"usage_limit_reached","message":"acct-PRIVATE","token":"at-FAKE"},"error":{"code":"other","message":"m"}}"#,
        r#"{"type":"response.future","\u0074ype":"error","type":"response.future","status":429,"error":{"message":"acct-PRIVATE","token":"at-FAKE"}}"#,
        r#"{"type":"a","\u0074\u0079\u0070\u0065":"error","type":"b","type":"c","s\u0074atus":403,"error":{"message":"acct-PRIVATE","token":"at-FAKE"}}"#,
        r#"{"type":"response.future","type":"response.failed","type":"response.future","response":{"error":{"code":"rate_limit_exceeded","message":"acct-PRIVATE","token":"at-FAKE"}}}"#,
        r#"{"type":"response.future","type":"error","type":"response.future","status":429,"error":{"type":"rate_limit_error","message":"acct-PRIVATE","token":"at-FAKE"}"#,
    ];
    let passes = [
        r#"{"type":"response.future","delta":5,"status":429,"error":{"type":"rate_limit_error","message":"limited"}}"#,
        r#"{"type":"response.future","type":"error","status":400,"error":{"code":"other","message":"bad input"}}"#,
        r#"{"type":"a","type":"b","type":"c","status":429,"error":{"type":"rate_limit_error","message":"limited"}}"#,
        r#"{"type":"response.future","type":"response.future","note":"\"type\":\"error\",\"status\":429","error":{"type":"rate_limit_error","message":"limited"}}"#,
        r#"{"type":"response.future","type":"error","status":400,"meta":{"status":429,"error":{"type":"rate_limit_error"}},"error":{"code":"other","message":"bad input"}}"#,
    ];
    let cases = withheld
        .iter()
        .map(|e| (*e, true))
        .chain(passes.iter().map(|e| (*e, false)));
    for (event, withhold) in cases {
        for (transport, config, websockets) in [
            ("socket", "", true),
            ("socket, buffering", BUFFERING, true),
            ("steering", STEERING, true),
            ("http", "", false),
            ("http, buffering", BUFFERING, false),
        ] {
            let case = format!("{transport}: {event}");
            let frames = [event, REAL_END];
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
            proxy.rt.usage_queue().configure(
                true,
                &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
            );
            let mut socket = connect(&proxy).await;
            socket
                .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
                .await
                .unwrap();
            let mut got = Read::default();
            while got.closed.is_none() && got.frames.last().map(String::as_str) != Some(REAL_END) {
                let more = read(&mut socket, &["response.completed"]).await;
                got.frames.extend(more.frames);
                got.closed = more.closed;
            }
            assert!(
                !got.frames.iter().any(|f| f.contains(TOKEN) || f.contains(ACCOUNT)),
                "{case}: the credential's token or the account reached the client: {got:?}"
            );
            if withhold {
                assert_eq!(got.frames, [REAL_END], "{case}");
            } else {
                assert_eq!(got.frames, frames, "{case}");
            }
            assert_eq!(got.closed, None, "{case}");
            let records = records(&proxy, 1).await;
            assert_eq!(records[0]["failed"], false, "{case}: {records:?}");
            assert_eq!(tokens(&records[0]), [4, 1, 5], "{case}: {records:?}");
            let credential = proxy.rt.store().snapshot()[0].id.clone();
            assert!(proxy.rt.store().cooldowns(&credential).is_empty(), "{case}");
            if websockets {
                assert_eq!(proxy.upstream.sockets().len(), 1, "{case}: one socket");
            } else {
                assert_eq!(proxy.upstream.http().len(), 1, "{case}: one attempt");
            }
        }
    }
}

/// The five native paths a WebSocket client's turn can take: (name, extra config, whether
/// the credential uses upstream sockets).
const NATIVE_PATHS: [(&str, &str, bool); 5] = [
    ("socket", "", true),
    (
        "socket, buffering",
        "codex:\n  stream-bootstrap-buffering: true\n",
        true,
    ),
    ("steering", STEERING, true),
    ("http", "", false),
    ("http, buffering", "codex:\n  stream-bootstrap-buffering: true\n", false),
];

/// What a WebSocket client got from one turn whose upstream sent `frames`, and what the
/// proxy recorded.
#[derive(Debug, PartialEq)]
struct Turned {
    frames: Vec<String>,
    closed: Option<u16>,
    records: Vec<serde_json::Value>,
    cooldowns: usize,
    attempts: usize,
    /// The `Authorization` each upstream attempt received: the selected credential's.
    authorization: Vec<String>,
    /// The connection's request log, when the turn ran logged ([`native_turn_on`]).
    log: String,
}

/// One turn on `path` ([`NATIVE_PATHS`]): the upstream sends `frames` (as socket frames or
/// SSE events), the client reads until `until` arrives, the socket closes or it goes
/// quiet. The fake credential's token is `at-FAKE`; the client's key is `client-key`.
async fn native_turn(path: (&str, &str, bool), frames: &[&str], until: &str) -> Turned {
    let turned = native_turn_on(path, &[frames], until, &[SELECTED], &[], false).await;
    assert_eq!(turned.records.len(), 1, "one record: {:?}", turned.records);
    turned
}

/// [`native_turn`] with one Codex credential per token of `tokens`, the first preferred
/// (a fake second account, `at-SECOND`, shows a failover as a second `authorization`),
/// an upstream that answers the attempts in order from `replies`, and upstream sockets
/// that close after the client frames numbered in `ws_close` (a truncated turn). Every
/// usage record is returned; with `logged`, the request log of the turn's connection too.
async fn native_turn_on(
    path: (&str, &str, bool),
    replies: &[&[&str]],
    until: &str,
    tokens: &[&str],
    ws_close: &[usize],
    logged: bool,
) -> Turned {
    let (_, config, websockets) = path;
    let config = if logged {
        format!("{config}request-log: true\n")
    } else {
        config.to_owned()
    };
    let script = if websockets {
        Script {
            ws: replies
                .iter()
                .map(|frames| frames.iter().map(|f| Bytes::from(f.to_string())).collect())
                .collect(),
            ws_close: ws_close.to_vec(),
            ..Script::default()
        }
    } else {
        let sse = |frames: &&[&str]| frames.iter().map(|f| format!("data: {f}\n\n")).collect::<String>();
        Script {
            http: replies
                .iter()
                .map(|frames| cpa_server::testing::codex_capture::Reply::sse(sse(frames)))
                .collect(),
            ..Script::default()
        }
    };
    let mut proxy = proxy_with(script, &config, |url| {
        tokens
            .iter()
            .enumerate()
            .map(|(i, token)| {
                let mut c = credential(Profile::Chatgpt, url, websockets);
                if i > 0 {
                    c.id = format!("codex-{i}.json");
                    c.metadata.insert("access_token".into(), serde_json::json!(token));
                }
                c.attributes.insert("priority".into(), (9 - i).to_string());
                c
            })
            .collect()
    })
    .await;
    let logs = logged.then(|| log_requests(&mut proxy));
    proxy.rt.usage_queue().configure(
        true,
        &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
    );
    let mut socket = connect(&proxy).await;
    socket
        .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
        .await
        .unwrap();
    let mut got = Read::default();
    loop {
        match tokio::time::timeout(Duration::from_millis(500), socket.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                got.frames.push(text.as_str().to_owned());
                if text.as_str() == until {
                    break;
                }
            }
            Ok(Some(Ok(Message::Close(frame)))) => {
                got.closed = Some(frame.map_or(1005, |f| u16::from(f.code)));
                break;
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_)) | None) => {
                got.closed = Some(1006);
                break;
            }
            Err(_) => break,
        }
    }
    let mut records = records(&proxy, 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    records.extend(
        proxy
            .rt
            .usage_queue()
            .pop_oldest(10)
            .iter()
            .map(|r| serde_json::from_slice::<serde_json::Value>(r).unwrap()),
    );
    let cooldowns = proxy
        .rt
        .store()
        .snapshot()
        .iter()
        .map(|c| proxy.rt.store().cooldowns(&c.id).len())
        .sum();
    let headers: Vec<axum::http::HeaderMap> = if websockets {
        proxy.upstream.sockets().into_iter().map(|s| s.headers).collect()
    } else {
        proxy.upstream.http().into_iter().map(|e| e.headers).collect()
    };
    let authorization = headers
        .iter()
        .map(|h| {
            h.get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    drop(socket);
    let log = match &logs {
        Some((dir, _)) => finished_logs(dir, 1).await.remove(0),
        None => String::new(),
    };
    Turned {
        frames: got.frames,
        closed: got.closed,
        records,
        cooldowns,
        attempts: headers.len(),
        authorization,
        log,
    }
}

/// Serves `proxy` through the shared request-log middleware (with `request-log: true` in
/// its config): its connections are logged to the returned directory in its private
/// `auth-dir`. Keep the management handle as long as the proxy serves.
fn log_requests(proxy: &mut Proxy) -> (std::path::PathBuf, Arc<cpa_server::management::Management>) {
    let dir = proxy._auth_dir.0.join("logs");
    std::fs::create_dir_all(&dir).unwrap();
    let management = cpa_server::management::Management::with_options(
        proxy.rt.clone(),
        proxy._auth_dir.0.join("config.yaml"),
        cpa_server::management::Options {
            log_dir: Some(dir.clone()),
            management_password: Some(String::new()),
            auth_dir: Some(proxy._auth_dir.0.clone()),
            ..Default::default()
        },
    );
    let app = cpa_server::request_logging::router(&management, cpa_server::router(proxy.rt.clone()));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    proxy.addr = listener.local_addr().unwrap().to_string();
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (dir, management)
}

/// The `n` request logs in `dir`, oldest first, once complete: no spool file is left and
/// the files read the same three times in a row. A log is written when its connection
/// ends.
async fn finished_logs(dir: &Path, n: usize) -> Vec<String> {
    let mut last: Option<Vec<String>> = None;
    let mut same = 0;
    for _ in 0..250 {
        let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().collect();
        let spooling = entries.iter().any(|e| e.path().extension().is_some_and(|x| x == "tmp"));
        entries.retain(|e| e.path().extension().is_some_and(|x| x == "log"));
        entries.sort_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());
        if entries.len() == n && !spooling {
            let content: Vec<String> = entries
                .iter()
                .map(|e| std::fs::read_to_string(e.path()).unwrap())
                .collect();
            if last.as_ref() == Some(&content) {
                same += 1;
                if same == 3 {
                    return content;
                }
            } else {
                (last, same) = (Some(content), 0);
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no {n} complete request logs in {dir:?}: {last:?}")
}

/// #25 (E2) and #6, a capacity message composed from several members: a failure reading
/// builds one error body from the error object as sent, the frame's `message`, `code`,
/// `error_type` and `param`, and a message taken from the other error object
/// (`codex_response::terminal_failure_body`), and its capacity check reads that whole
/// body. So `model` in one member and `at capacity` in another make the middle reading of
/// a repeated `type` a 429 although no member is one alone, and the frame, which the
/// pinned client cannot decode, could name the pooled account or carry the credential's
/// own token. Each is withheld without cooling, usage, acknowledgement or ownership
/// effects: the real completion after it arrives, the one record carries its usage, and
/// neither `at-FAKE` nor `acct-PRIVATE` reaches the client (whose key is `client-key`).
/// The safe controls pass as sent: the same pieces where no reading composes them (an
/// unrelated object), and pieces that are no capacity message together. On the five
/// native paths.
#[tokio::test]
async fn composed_capacity_frames_are_withheld_on_every_native_path() {
    const REAL_END: &str = r#"{"type":"response.completed","response":{"id":"resp_real","status":"completed","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    let withheld = [
        // The three review witnesses: split `code` and `message`, `param`, and the
        // response error's message composed into the root error object.
        r#"{"type":"response.future","type":"error","type":"response.future","code":"model","message":"at capacity","private_account":"acct-PRIVATE","token":"at-FAKE"}"#,
        r#"{"type":"response.future","type":"error","type":"response.future","param":"model at capacity","private_account":"acct-PRIVATE","token":"at-FAKE"}"#,
        r#"{"type":"response.future","type":"error","type":"response.future","error":{"type":"model","private_account":"acct-PRIVATE","token":"at-FAKE"},"response":{"error":{"message":"at capacity"}}}"#,
        // `error_type` with `message`; an escaped phrase a reading unescapes; a key of the
        // error object holding `model`; a `response.failed` whose error object composes.
        r#"{"type":"x","type":"error","type":"x","error_type":"model","message":"at capacity","token":"at-FAKE","private_account":"acct-PRIVATE"}"#,
        r#"{"type":"x","type":"error","type":"x","message":"mod\u0065l at\u0020capacity","token":"at-FAKE","private_account":"acct-PRIVATE"}"#,
        r#"{"type":"x","type":"error","type":"x","error":{"model":"gpt","note":"at capacity","token":"at-FAKE","private_account":"acct-PRIVATE"}}"#,
        r#"{"type":"x","type":"response.failed","type":"x","response":{"error":{"code":"model","message":"at capacity","token":"at-FAKE","private_account":"acct-PRIVATE"}}}"#,
        // A capacity code alone, and an error string.
        r#"{"type":"x","type":"error","type":"x","code":"model_at_capacity","token":"at-FAKE","private_account":"acct-PRIVATE"}"#,
        r#"{"type":"x","type":"error","type":"x","error":"model at capacity","token":"at-FAKE","private_account":"acct-PRIVATE"}"#,
        // A private raw value the pinned `Value` reads as the error object.
        r#"{"type":"x","type":"error","type":"x","error":{"$serde_json::private::RawValue":"{\"type\":\"rate_limit_error\"}"},"token":"at-FAKE","private_account":"acct-PRIVATE"}"#,
    ];
    let passes = [
        r#"{"type":"x","type":"error","type":"x","meta":{"code":"model","message":"at capacity"},"error":{"code":"other","message":"bad input"}}"#,
        r#"{"type":"x","type":"error","type":"x","code":"model","message":"busy"}"#,
        r#"{"type":"x","type":"error","type":"x","error":{"code":"other","message":"at capacity"}}"#,
    ];
    let cases = withheld
        .iter()
        .map(|e| (*e, true))
        .chain(passes.iter().map(|e| (*e, false)));
    for (event, withhold) in cases {
        for path in NATIVE_PATHS {
            let case = format!("{}: {event}", path.0);
            let got = native_turn(path, &[event, REAL_END], REAL_END).await;
            assert!(
                !got.frames
                    .iter()
                    .any(|f| f.contains("at-FAKE") || f.contains("acct-PRIVATE")),
                "{case}: the credential's token or the account reached the client: {got:?}"
            );
            if withhold {
                assert_eq!(got.frames, [REAL_END], "{case}");
            } else {
                assert_eq!(got.frames, [event, REAL_END], "{case}");
            }
            assert_eq!(got.closed, None, "{case}");
            assert_eq!(got.records[0]["failed"], false, "{case}: {got:?}");
            assert_eq!(tokens(&got.records[0]), [4, 1, 5], "{case}: {got:?}");
            assert_eq!(got.cooldowns, 0, "{case}");
            assert_eq!(got.attempts, 1, "{case}");
        }
    }
}

/// #25 and #6, a decoded failure as the pinned `Value` holds it: the first-party build's
/// serde_json (1.0.149 with `arbitrary_precision` and `raw_value`) reads an object whose
/// one member is its private raw value as the JSON text it holds, and one that is its
/// private number as that number (`value/de.rs`, `visit_map`). A decoded `response.failed`
/// whose `response.error` is such a raw value is the rate-limit failure inside it, so it
/// is classified 429 and never reaches the client as sent: its frame can carry the
/// credential's own token or name the pooled account. Each wrapped event takes the same
/// turn, record and cooldowns as the same event with the value resolved, on the five
/// native paths: the raw value, a raw value inside a raw value, the last of a repeated
/// key inside the text, a private number as the error's status, and the whole `response`
/// as a raw value. The reversed repeat (an account type first) is no account failure in
/// either form, and reaches the client as sent.
#[tokio::test]
async fn decoded_private_values_classify_as_the_pinned_value_reads_them() {
    const REAL_END: &str = r#"{"type":"response.completed","response":{"id":"resp_real","status":"completed","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    // (event as sent, the same event with its private values resolved, an account failure)
    let cases = [
        (
            r#"{"type":"response.failed","response":{"id":"r1","error":{"$serde_json::private::RawValue":"{\"type\":\"rate_limit_error\",\"code\":\"rate_limit_exceeded\",\"message\":\"limited\",\"token\":\"at-FAKE\",\"private_account\":\"acct-PRIVATE\"}"}}}"#,
            r#"{"type":"response.failed","response":{"id":"r1","error":{"type":"rate_limit_error","code":"rate_limit_exceeded","message":"limited","token":"at-FAKE","private_account":"acct-PRIVATE"}}}"#,
            true,
        ),
        (
            r#"{"type":"response.failed","response":{"id":"r1","error":{"$serde_json::private::RawValue":"{\"$serde_json::private::RawValue\":\"{\\\"code\\\":\\\"invalid_api_key\\\",\\\"token\\\":\\\"at-FAKE\\\"}\"}"}}}"#,
            r#"{"type":"response.failed","response":{"id":"r1","error":{"code":"invalid_api_key","token":"at-FAKE"}}}"#,
            true,
        ),
        (
            r#"{"type":"response.failed","response":{"id":"r1","error":{"$serde_json::private::RawValue":"{\"type\":\"invalid_request_error\",\"type\":\"permission_error\",\"token\":\"at-FAKE\"}"}}}"#,
            r#"{"type":"response.failed","response":{"id":"r1","error":{"type":"permission_error","token":"at-FAKE"}}}"#,
            true,
        ),
        (
            r#"{"type":"response.failed","response":{"id":"r1","error":{"code":"other","status":{"$serde_json::private::Number":"429"},"token":"at-FAKE"}}}"#,
            r#"{"type":"response.failed","response":{"id":"r1","error":{"code":"other","status":429,"token":"at-FAKE"}}}"#,
            true,
        ),
        (
            r#"{"type":"response.failed","response":{"$serde_json::private::RawValue":"{\"id\":\"r1\",\"error\":{\"code\":\"rate_limit_exceeded\",\"token\":\"at-FAKE\"}}"}}"#,
            r#"{"type":"response.failed","response":{"id":"r1","error":{"code":"rate_limit_exceeded","token":"at-FAKE"}}}"#,
            true,
        ),
        (
            r#"{"type":"response.failed","response":{"id":"r1","error":{"$serde_json::private::RawValue":"{\"type\":\"permission_error\",\"type\":\"invalid_request_error\",\"message\":\"bad\"}"}}}"#,
            r#"{"type":"response.failed","response":{"id":"r1","error":{"type":"invalid_request_error","message":"bad"}}}"#,
            false,
        ),
    ];
    for (wrapped, resolved, account) in cases {
        assert!(cpa_exec::codex_event::decode(wrapped).is_some(), "{wrapped}");
        for path in NATIVE_PATHS {
            let case = format!("{}: {wrapped}", path.0);
            let got = native_turn(path, &[wrapped, REAL_END], REAL_END).await;
            let want = native_turn(path, &[resolved, REAL_END], REAL_END).await;
            assert!(
                !got.frames
                    .iter()
                    .any(|f| f.contains("at-FAKE") || f.contains("acct-PRIVATE")),
                "{case}: the credential's token or the account reached the client: {got:?}"
            );
            // The resolved event's frames, as sent, wherever one reached the client.
            let as_sent = |t: &Turned, raw: &str| -> Vec<String> {
                t.frames
                    .iter()
                    .map(|f| if f == raw { "EVENT".to_owned() } else { f.clone() })
                    .collect()
            };
            assert_eq!(as_sent(&got, wrapped), as_sent(&want, resolved), "{case}");
            assert_eq!(got.closed, want.closed, "{case}");
            assert_eq!(got.cooldowns, want.cooldowns, "{case}");
            assert_eq!(got.attempts, want.attempts, "{case}");
            for key in ["failed", "fail"] {
                assert_eq!(got.records[0][key], want.records[0][key], "{case}: {got:?} {want:?}");
            }
            assert_eq!(tokens(&got.records[0]), tokens(&want.records[0]), "{case}");
            // An account failure never reaches the client as sent; any other failure does.
            assert_eq!(
                !as_sent(&got, wrapped).iter().any(|f| f == "EVENT"),
                account,
                "{case}: {got:?}"
            );
            assert_eq!(got.records[0]["failed"], true, "{case}: {got:?}");
        }
    }
}

/// #25 and #7, response identity: a decoded event names its response by the `response.id`
/// the pinned `Value` reads, the last of a repeated `id` (and a private raw value's text).
/// A steering connection's ledger settles the response the pinned reader completes: a
/// completion repeating `id` with the created response's ID last is that response's end,
/// recorded while the socket stays open with its usage, as on an ordinary socket. Each
/// event with a repeated or wrapped ID takes the same turn and record as the same event
/// with the ID the pinned `Value` reads: the completion's last ID, the created event's
/// last ID, the reversed order (whose last ID names no created response), and a private
/// raw value. The client receives each frame as sent.
#[tokio::test]
async fn decoded_response_ids_settle_as_the_pinned_value_reads_them() {
    const CREATED: &str = r#"{"type":"response.created","response":{"id":"r1"}}"#;
    let end = |id: &str| {
        format!(
            r#"{{"type":"response.completed","response":{{{id},"output":[],"usage":{{"input_tokens":3,"output_tokens":2,"total_tokens":5}}}}}}"#
        )
    };
    // (created, completion) as sent, the same pair with the IDs the pinned `Value` reads,
    // and whether the created response settles while the socket is open.
    let cases = [
        (
            CREATED.to_owned(),
            end(r#""id":"stale","id":"r1""#),
            CREATED.to_owned(),
            end(r#""id":"r1""#),
            true,
        ),
        (
            r#"{"type":"response.created","response":{"id":"stale","id":"r1"}}"#.to_owned(),
            end(r#""id":"r1""#),
            CREATED.to_owned(),
            end(r#""id":"r1""#),
            true,
        ),
        (
            CREATED.to_owned(),
            end(r#""id":"r1","id":"stale""#),
            CREATED.to_owned(),
            end(r#""id":"stale""#),
            false,
        ),
        (
            CREATED.to_owned(),
            end(r#""id":{"$serde_json::private::RawValue":"\"r1\""}"#),
            CREATED.to_owned(),
            end(r#""id":"r1""#),
            true,
        ),
    ];
    for (created, completed, pinned_created, pinned_completed, settles) in cases {
        for steering in [false, true] {
            let case = format!("steering {steering}: {created} {completed}");
            let mut outcomes = Vec::new();
            for (created, completed) in [(&created, &completed), (&pinned_created, &pinned_completed)] {
                assert!(cpa_exec::codex_event::decode(completed).is_some(), "{completed}");
                let proxy = proxy(
                    Profile::Chatgpt,
                    Script {
                        ws: vec![vec![Bytes::from(created.clone()), Bytes::from(completed.clone())]],
                        ..Script::default()
                    },
                    if steering { STEERING } else { "" },
                )
                .await;
                proxy.rt.usage_queue().configure(
                    true,
                    &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
                );
                let mut socket = connect(&proxy).await;
                socket
                    .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
                    .await
                    .unwrap();
                let got = read(&mut socket, &["response.completed"]).await;
                assert_eq!(got.frames, [created.as_str(), completed.as_str()], "{case}");
                // Records published while the socket is open.
                let mut open = Vec::new();
                let _ = tokio::time::timeout(Duration::from_millis(500), async {
                    while open.is_empty() {
                        open.extend(
                            proxy
                                .rt
                                .usage_queue()
                                .pop_oldest(10)
                                .iter()
                                .map(|r| serde_json::from_slice::<serde_json::Value>(r).unwrap()),
                        );
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await;
                let published_open = open.len();
                socket.send(Message::Close(None)).await.unwrap();
                drop(socket);
                let all = if open.is_empty() {
                    records(&proxy, 1).await
                } else {
                    open
                };
                // A response left open fails when the socket goes: as the client's
                // cancellation (499) or as the upstream socket's end (502), whichever the
                // duplex sees first, so only a settled record's status is compared.
                let failed = all[0]["failed"].clone();
                let status = if failed == false {
                    all[0]["fail"]["status_code"].clone()
                } else {
                    serde_json::Value::Null
                };
                outcomes.push((published_open, all.len(), failed, status, tokens(&all[0])));
            }
            assert_eq!(
                outcomes[0], outcomes[1],
                "{case}: as sent, then as the pinned `Value` reads it: {outcomes:?}"
            );
            if settles {
                assert_eq!(outcomes[0].0, 1, "{case}: settled while open: {outcomes:?}");
                assert_eq!(outcomes[0].2, false, "{case}: {outcomes:?}");
                assert_eq!(outcomes[0].4, [3, 2, 5], "{case}: {outcomes:?}");
            }
        }
    }
}

/// A readable ResponseCompleted array names the completed response by position. It
/// must not donate its settlement to the latest open create on a steering socket.
#[tokio::test]
async fn positional_completions_settle_their_own_response() {
    const CREATED: &str = r#"{"type":"response.created","response":{"id":"r1"}}"#;
    const CREATED_2: &str = r#"{"type":"response.created","response":{"id":"r2"}}"#;
    const DONE_2: &str = r#"{"type":"response.completed","response":{"id":"r2","usage":{"input_tokens":7,"output_tokens":1,"total_tokens":8}}}"#;
    const FIRST: &str = r#"{"type":"response.create","model":"gpt-5.5","input":[],"service_tier":"priority"}"#;
    const SECOND: &str = r#"{"type":"response.create","model":"gpt-5.5","input":[],"service_tier":"flex"}"#;
    const STEER: &str = r#"{"type":"response.steer","previous_response_id":"r2","input":[]}"#;
    for completed in [
        r#"{"type":"response.completed","response":["r1",null,null,false]}"#,
        r#"{"type":"response.completed","response":{"$serde_json::private::RawValue":"[\"r1\",null,null,false]"}}"#,
        r#"{"type":"response.completed","response":{"id":"r1","end_turn":false}}"#,
    ] {
        let batch = |frames: &[&str]| frames.iter().map(|f| Bytes::from((*f).to_owned())).collect();
        let proxy = proxy(
            Profile::Chatgpt,
            Script {
                ws: vec![batch(&[CREATED]), batch(&[CREATED_2, completed]), batch(&[DONE_2])],
                ..Script::default()
            },
            STEERING,
        )
        .await;
        proxy.rt.usage_queue().configure(
            true,
            &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
        );
        let mut socket = connect(&proxy).await;
        socket.send(Message::text(FIRST)).await.unwrap();
        assert_eq!(read(&mut socket, &["response.created"]).await.frames, [CREATED]);
        socket.send(Message::text(SECOND)).await.unwrap();
        assert_eq!(
            read(&mut socket, &["response.completed"]).await.frames,
            [CREATED_2, completed]
        );
        let first = records(&proxy, 1).await;
        assert_eq!(first.len(), 1, "{completed}: {first:?}");
        assert_eq!(first[0]["service_tier"], "priority", "{completed}: {first:?}");
        assert_eq!(first[0]["failed"], false, "{completed}: {first:?}");
        socket.send(Message::text(STEER)).await.unwrap();
        assert_eq!(read(&mut socket, &["response.completed"]).await.frames, [DONE_2]);
        let second = records(&proxy, 1).await;
        assert_eq!(second.len(), 1, "{completed}: {second:?}");
        assert_eq!(second[0]["service_tier"], "flex", "{completed}: {second:?}");
        assert_eq!(second[0]["failed"], false, "{completed}: {second:?}");
        assert_eq!(tokens(&second[0]), [7, 1, 8]);
        assert_eq!(upstream_frames(&proxy, 0), [FIRST, SECOND, STEER]);
        socket.close(1000, "done").await.unwrap();
    }
}

/// The 14 members of a positional `ResponsesStreamEvent`, `null` but for the given ones.
fn positional(members: &[(usize, &str)]) -> String {
    let mut all = vec!["null"; 14];
    for (i, raw) in members {
        all[*i] = raw;
    }
    format!("[{}]", all.join(","))
}

/// What a client gets for one of [`positional_events_classify_as_their_object_form`]'s
/// events.
#[derive(Clone, Copy, PartialEq)]
enum Expect {
    /// An account failure: withheld, the credential cools.
    Withheld,
    /// Any other failure: the frame as sent.
    AsSent,
    /// The #5 context-window `response.failed`, built from the event's members.
    Converted,
    /// A completion: the frame as sent, the turn ends.
    Completes,
}

/// #25 and #6, an event written as a JSON array: serde reads a derived struct from an
/// array by position, so the pinned client decodes `["response.failed",null,null,{…}]`
/// as a `response.failed` whose `response` is the fourth element
/// (`codex-api/src/sse/responses.rs:161-179` at 44ccf394573c). Failure classification
/// reads the members it decodes to (`codex_event::classification_view`), so each
/// positional event takes the same turn, record and cooldowns as its object form on the
/// five native paths: an account failure never reaches the client as sent (neither the
/// credential's `at-FAKE` nor the account's `acct-PRIVATE`, while the client's key is
/// `client-key`), cools the credential and fails the record with zero tokens; a
/// non-account failure reaches the client as sent; a status-less context-window `error`
/// becomes the #5 `response.failed` built from its members' bytes; a positional
/// completion ends the turn as its object form does. The real completion after a
/// failure is never accepted, and nothing is replayed (one attempt each).
#[tokio::test]
async fn positional_events_classify_as_their_object_form() {
    const REAL_END: &str = r#"{"type":"response.completed","response":{"id":"resp_real","status":"completed","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    const LIMITED: &str = r#"{"type":"rate_limit_error","code":"rate_limit_exceeded","message":"limited","token":"at-FAKE","private_account":"acct-PRIVATE"}"#;
    let limited_response = format!(r#"{{"id":"r1","error":{LIMITED}}}"#);
    let wrapped_response = format!(
        r#"{{"$serde_json::private::RawValue":{}}}"#,
        serde_json::to_string(&limited_response).unwrap()
    );
    let context = r#"{"code":"context_length_exceeded","message":"Your input exceeds the context window of this model.","param":"input"}"#;
    // (positional, object form, what the client gets)
    let cases = [
        // The review witness.
        (
            positional(&[(0, r#""response.failed""#), (3, &limited_response)]),
            format!(r#"{{"type":"response.failed","response":{limited_response}}}"#),
            Expect::Withheld,
        ),
        (
            positional(&[(0, r#""error""#), (5, LIMITED)]),
            format!(r#"{{"type":"error","error":{LIMITED}}}"#),
            Expect::Withheld,
        ),
        (
            positional(&[(0, r#""response.failed""#), (3, &wrapped_response)]),
            format!(r#"{{"type":"response.failed","response":{wrapped_response}}}"#),
            Expect::Withheld,
        ),
        (
            positional(&[
                (0, r#""response.failed""#),
                (3, r#"{"id":"r1","error":{"code":"server_error","message":"boom"}}"#),
            ]),
            r#"{"type":"response.failed","response":{"id":"r1","error":{"code":"server_error","message":"boom"}}}"#
                .to_owned(),
            Expect::AsSent,
        ),
        (
            positional(&[(0, r#""error""#), (4, r#""resp_ctx""#), (5, context)]),
            format!(r#"{{"type":"error","response_id":"resp_ctx","error":{context}}}"#),
            Expect::Converted,
        ),
        (
            positional(&[(0, r#""response.completed""#), (3, r#"{"id":"r1","end_turn":true}"#)]),
            r#"{"type":"response.completed","response":{"id":"r1","end_turn":true}}"#.to_owned(),
            Expect::Completes,
        ),
    ];
    for (array, object, expect) in cases {
        assert!(cpa_exec::codex_event::decode(&array).is_some(), "{array}");
        for path in NATIVE_PATHS {
            let case = format!("{}: {array}", path.0);
            let got = native_turn(path, &[&array, REAL_END], REAL_END).await;
            let want = native_turn(path, &[&object, REAL_END], REAL_END).await;
            assert!(
                !got.frames
                    .iter()
                    .any(|f| f.contains("at-FAKE") || f.contains("acct-PRIVATE")),
                "{case}: the credential's token or the account reached the client: {got:?}"
            );
            let as_sent = |t: &Turned, raw: &str| -> Vec<String> {
                t.frames
                    .iter()
                    .map(|f| if f == raw { "EVENT".to_owned() } else { f.clone() })
                    .collect()
            };
            assert_eq!(as_sent(&got, &array), as_sent(&want, &object), "{case}");
            assert_eq!(got.closed, want.closed, "{case}");
            assert_eq!(got.cooldowns, want.cooldowns, "{case}");
            assert_eq!(got.attempts, 1, "{case}");
            assert_eq!(want.attempts, 1, "{case}");
            for key in ["failed", "fail"] {
                assert_eq!(got.records[0][key], want.records[0][key], "{case}: {got:?} {want:?}");
            }
            assert_eq!(tokens(&got.records[0]), tokens(&want.records[0]), "{case}");
            let delivered = as_sent(&got, &array).iter().any(|f| f == "EVENT");
            if expect == Expect::Completes {
                // A steering connection relays the later frame too, as for the object form.
                assert_eq!(got.frames[0], array, "{case}");
                assert_eq!(got.records[0]["failed"], false, "{case}: {got:?}");
                assert_eq!(got.cooldowns, 0, "{case}");
                continue;
            }
            assert_eq!(got.records[0]["failed"], true, "{case}: {got:?}");
            assert_eq!(tokens(&got.records[0]), [0, 0, 0], "{case}");
            assert!(!got.frames.iter().any(|f| f == REAL_END), "{case}: {got:?}");
            assert_eq!(delivered, expect == Expect::AsSent, "{case}: {got:?}");
            if expect == Expect::Converted {
                assert!(
                    got.frames[0].starts_with(r#"{"type":"response.failed","response":{"id":"resp_ctx""#),
                    "{case}: {got:?}"
                );
            }
            if expect == Expect::Withheld {
                // Bootstrap buffering records its overload policy's 503.
                let status = &got.records[0]["fail"]["status_code"];
                assert!(*status == 429 || *status == 503, "{case}: {got:?}");
                assert!(got.cooldowns > 0, "{case}");
            }
        }
    }
}

/// #25 and #6, a wrapped status error written as a JSON array: the pinned WebSocket
/// reader decodes `["error",429,{…}]` as `WrappedWebsocketErrorEvent` by position
/// (`endpoint/responses_websocket.rs:684-709` at 44ccf394573c, `error` itself possibly an
/// array) and fails the turn with its status, as for the object form; the SSE reader
/// cannot decode it and skips it. On the socket paths the frame takes its object form's
/// turn, record and cooldown; over HTTP it is an undecodable frame that would read as an
/// account failure, so it is withheld and the real completion follows. Neither path
/// delivers the credential's token or the account.
#[tokio::test]
async fn positional_wrapped_status_errors_classify_as_their_object_form() {
    const REAL_END: &str = r#"{"type":"response.completed","response":{"id":"resp_real","status":"completed","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    for (array, object) in [
        (
            r#"["error",429,{"code":"rate_limit_exceeded","message":"limited at-FAKE acct-PRIVATE"}]"#,
            r#"{"type":"error","status":429,"error":{"code":"rate_limit_exceeded","message":"limited at-FAKE acct-PRIVATE"}}"#,
        ),
        (
            r#"["error",401,["invalid_api_key","bad key at-FAKE acct-PRIVATE"],{"x-request-id":"req_1"}]"#,
            r#"{"type":"error","status":401,"error":{"code":"invalid_api_key","message":"bad key at-FAKE acct-PRIVATE"},"headers":{"x-request-id":"req_1"}}"#,
        ),
    ] {
        assert!(cpa_exec::codex_event::decode(array).is_none(), "{array}");
        for path in NATIVE_PATHS {
            let case = format!("{}: {array}", path.0);
            let got = native_turn(path, &[array, REAL_END], REAL_END).await;
            assert!(
                !got.frames
                    .iter()
                    .any(|f| f.contains("at-FAKE") || f.contains("acct-PRIVATE")),
                "{case}: the credential's token or the account reached the client: {got:?}"
            );
            assert_eq!(got.attempts, 1, "{case}");
            if path.2 {
                let want = native_turn(path, &[object, REAL_END], REAL_END).await;
                assert_eq!(got.frames, want.frames, "{case}");
                assert_eq!(got.closed, want.closed, "{case}");
                assert_eq!(got.cooldowns, want.cooldowns, "{case}");
                assert!(got.cooldowns > 0, "{case}");
                for key in ["failed", "fail"] {
                    assert_eq!(got.records[0][key], want.records[0][key], "{case}: {got:?} {want:?}");
                }
                assert_eq!(got.records[0]["failed"], true, "{case}: {got:?}");
            } else {
                assert_eq!(got.frames, [REAL_END], "{case}");
                assert_eq!(got.records[0]["failed"], false, "{case}: {got:?}");
                assert_eq!(tokens(&got.records[0]), [4, 1, 5], "{case}");
                assert_eq!(got.cooldowns, 0, "{case}");
            }
        }
    }
}

/// #25 (E2) and #6, a safe unique frame: every key is unique within its object, so a
/// frame naming `error.type` and `response.error.type` once each repeats nothing, and is
/// read by its first and last readings alone (the root error, a 400). The pinned client
/// cannot decode it (`item_id` is no string) and skips it; it reaches the client as sent
/// on the five native paths, followed by the real completion, one successful record with
/// its usage and no cooldown. A frame that really repeats a member on the same path
/// stays withheld.
#[tokio::test]
async fn unique_frames_reach_the_client_on_every_native_path() {
    const REAL_END: &str = r#"{"type":"response.completed","response":{"id":"resp_real","status":"completed","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    let cases = [
        (
            r#"{"type":"error","item_id":23,"error":{"type":"invalid_request_error","message":"bad input"},"response":{"error":{"type":"model_at_capacity"}}}"#,
            false,
        ),
        (
            r#"{"type":"error","item_id":23,"error":{"code":"other","message":"bad input"},"response":{"error":{"code":"rate_limit_exceeded","status":429}}}"#,
            false,
        ),
        (
            r#"{"type":"error","item_id":23,"error":{"type":"invalid_request_error","message":"bad input"},"error":{"type":"invalid_request_error","message":"bad input"},"response":{"error":{"type":"model_at_capacity","token":"at-FAKE"}}}"#,
            true,
        ),
        (
            r#"{"type":"response.failed","item_id":23,"response":{"id":"a"},"response":{"error":{"code":"rate_limit_exceeded","token":"at-FAKE"}},"response":{"id":"b"}}"#,
            true,
        ),
    ];
    for (frame, withheld) in cases {
        assert!(cpa_exec::codex_event::decode(frame).is_none(), "{frame}");
        for path in NATIVE_PATHS {
            let case = format!("{}: {frame}", path.0);
            let got = native_turn(path, &[frame, REAL_END], REAL_END).await;
            if withheld {
                assert_eq!(got.frames, [REAL_END], "{case}");
            } else {
                assert_eq!(got.frames, [frame, REAL_END], "{case}");
            }
            assert_eq!(got.closed, None, "{case}");
            assert_eq!(got.records[0]["failed"], false, "{case}: {got:?}");
            assert_eq!(tokens(&got.records[0]), [4, 1, 5], "{case}: {got:?}");
            assert_eq!(got.cooldowns, 0, "{case}");
            assert_eq!(got.attempts, 1, "{case}");
        }
    }
}

/// #25 and #6, a terminal provider code the pinned `error` branch reads through private
/// values (`parse_terminal_provider_error`, `codex-api/src/error.rs:79-127` at
/// 44ccf394573c, reading the `Value`): `budget_reached` beside a rate-limit type, sent
/// resolved, as a raw value holding the whole error object, and as a raw value holding
/// the code alone. A status-200 wrapped notice still fails the turn on it. Each wrapped
/// form takes the resolved form's turn, record and cooldowns on the five native paths:
/// the account failure is withheld, the credential cools, the record fails with zero
/// tokens and the later completion is never accepted.
#[tokio::test]
async fn private_terminal_provider_codes_classify_as_resolved() {
    const REAL_END: &str = r#"{"type":"response.completed","response":{"id":"resp_real","status":"completed","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    let resolved = r#"{"type":"error","status":200,"error":{"type":"rate_limit_error","code":"budget_reached","message":"limited","token":"at-FAKE","private_account":"acct-PRIVATE"}}"#;
    let wrapped_object = r#"{"type":"error","status":200,"error":{"$serde_json::private::RawValue":"{\"type\":\"rate_limit_error\",\"code\":\"budget_reached\",\"message\":\"limited\",\"token\":\"at-FAKE\",\"private_account\":\"acct-PRIVATE\"}"}}"#;
    let wrapped_code = r#"{"type":"error","status":200,"error":{"type":"rate_limit_error","code":{"$serde_json::private::RawValue":"\"budget_reached\""},"message":"limited","token":"at-FAKE","private_account":"acct-PRIVATE"}}"#;
    for path in NATIVE_PATHS {
        let want = native_turn(path, &[resolved, REAL_END], REAL_END).await;
        assert_eq!(want.records[0]["failed"], true, "{}: {want:?}", path.0);
        assert!(want.cooldowns > 0, "{}: {want:?}", path.0);
        for wrapped in [wrapped_object, wrapped_code] {
            assert!(cpa_exec::codex_event::decode(wrapped).is_some(), "{wrapped}");
            let case = format!("{}: {wrapped}", path.0);
            let got = native_turn(path, &[wrapped, REAL_END], REAL_END).await;
            for t in [&got, &want] {
                assert!(
                    !t.frames
                        .iter()
                        .any(|f| f.contains("at-FAKE") || f.contains("acct-PRIVATE")),
                    "{case}: the credential's token or the account reached the client: {t:?}"
                );
                assert!(!t.frames.iter().any(|f| f == REAL_END), "{case}: {t:?}");
            }
            assert_eq!(got.frames, want.frames, "{case}");
            assert_eq!(got.closed, want.closed, "{case}");
            assert_eq!(got.cooldowns, want.cooldowns, "{case}");
            assert_eq!(got.attempts, want.attempts, "{case}");
            for key in ["failed", "fail"] {
                assert_eq!(got.records[0][key], want.records[0][key], "{case}: {got:?} {want:?}");
            }
            assert_eq!(tokens(&got.records[0]), [0, 0, 0], "{case}");
        }
    }
}

/// #25 and #7, a positional event that is a positional completion: the event is the
/// `ResponsesStreamEvent` written as an array, its `response` the `ResponseCompleted`
/// written as one. A steering connection settles the response the pinned reader
/// completes (r1, `priority`), not the latest open one (r2, `flex`), while the socket
/// stays open; r2 then settles with its own usage. One upstream socket; each frame as
/// sent.
#[tokio::test]
async fn positional_completion_events_settle_their_own_response() {
    const CREATED: &str = r#"{"type":"response.created","response":{"id":"r1"}}"#;
    const CREATED_2: &str = r#"{"type":"response.created","response":{"id":"r2"}}"#;
    const DONE_2: &str = r#"{"type":"response.completed","response":{"id":"r2","usage":{"input_tokens":7,"output_tokens":1,"total_tokens":8}}}"#;
    const FIRST: &str = r#"{"type":"response.create","model":"gpt-5.5","input":[],"service_tier":"priority"}"#;
    const SECOND: &str = r#"{"type":"response.create","model":"gpt-5.5","input":[],"service_tier":"flex"}"#;
    const STEER: &str = r#"{"type":"response.steer","previous_response_id":"r2","input":[]}"#;
    let completed = positional(&[(0, r#""response.completed""#), (3, r#"["r1",null,null,false]"#)]);
    let batch = |frames: &[&str]| frames.iter().map(|f| Bytes::from((*f).to_owned())).collect();
    let proxy = proxy(
        Profile::Chatgpt,
        Script {
            ws: vec![batch(&[CREATED]), batch(&[CREATED_2, &completed]), batch(&[DONE_2])],
            ..Script::default()
        },
        STEERING,
    )
    .await;
    proxy.rt.usage_queue().configure(
        true,
        &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
    );
    let mut socket = connect(&proxy).await;
    socket.send(Message::text(FIRST)).await.unwrap();
    assert_eq!(read(&mut socket, &["response.created"]).await.frames, [CREATED]);
    socket.send(Message::text(SECOND)).await.unwrap();
    assert_eq!(read(&mut socket, &["response.created"]).await.frames, [CREATED_2]);
    let next = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .unwrap();
    assert!(
        matches!(next, Some(Ok(Message::Text(ref t))) if t.as_str() == completed),
        "{next:?}"
    );
    let first = records(&proxy, 1).await;
    assert_eq!(first.len(), 1, "{first:?}");
    assert_eq!(first[0]["service_tier"], "priority", "{first:?}");
    assert_eq!(first[0]["failed"], false, "{first:?}");
    socket.send(Message::text(STEER)).await.unwrap();
    assert_eq!(read(&mut socket, &["response.completed"]).await.frames, [DONE_2]);
    let second = records(&proxy, 1).await;
    assert_eq!(second.len(), 1, "{second:?}");
    assert_eq!(second[0]["service_tier"], "flex", "{second:?}");
    assert_eq!(tokens(&second[0]), [7, 1, 8]);
    assert_eq!(upstream_frames(&proxy, 0), [FIRST, SECOND, STEER]);
    assert_eq!(proxy.upstream.sockets().len(), 1);
    socket.close(1000, "done").await.unwrap();
}

/// The real completion the nested-error, explicit-status and zero-status tests end on.
const NESTED_END: &str = r#"{"type":"response.completed","response":{"id":"resp_real","status":"completed","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;

/// Content that reaches the client before a later failure: nothing fails over after it.
const NESTED_DELTA: &str =
    r#"{"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"partial"}"#;

/// Whether any frame the client got carries the fake credential's token or the account
/// ([`carries_secret`]).
fn discloses(t: &Turned) -> bool {
    t.frames.iter().any(|f| carries_secret(f))
}

/// Whether `text` carries the fake credential's token `at-FAKE` or the account
/// `acct-PRIVATE`: as sent, or in any JSON string or key once unescaped, the JSON text a
/// string or private raw value holds included (`at-FAKE` is the token too).
fn carries_secret(text: &str) -> bool {
    fn walk(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::String(s) => carries_secret(s),
            serde_json::Value::Array(items) => items.iter().any(walk),
            serde_json::Value::Object(map) => map.iter().any(|(k, v)| carries_secret(k) || walk(v)),
            _ => false,
        }
    }
    text.contains("at-FAKE")
        || text.contains("acct-PRIVATE")
        || serde_json::from_str::<serde_json::Value>(text).is_ok_and(|v| walk(&v))
}

/// #25 and #6 (review finding 1): `parse_failed_response` reads a decoded
/// `response.failed`'s `response.error` into its `Error` struct
/// (`codex-api/src/sse/responses_error.rs:14-24` at 44ccf394573c), so the struct written
/// as a JSON array of five or six members is the rate limit its object form is, with the
/// credential's own token in its message. On the five native paths, first and after
/// content, each positional form takes the object form's turn, record and cooldowns, and
/// neither `at-FAKE` nor `acct-PRIVATE` reaches the client (whose key is `client-key`):
/// six members, five, a private raw value holding the array, escapes, a private raw value
/// as the code, the whole event positional, and the whole `response` a raw value. A
/// four-member array is no `Error`: the pinned reader has its generic stream error, which
/// names no code, so it is no account failure; a safe one reaches the client as sent
/// (one that reads as an account failure by position is withheld:
/// `refused_failure_slots_never_reach_the_client`).
#[tokio::test]
async fn nested_positional_failed_errors_classify_as_their_object_form() {
    const OBJECT: &str = r#"{"type":"response.failed","response":{"id":"r1","error":{"type":"rate_limit_error","code":"rate_limit_exceeded","message":"limited at-FAKE acct-PRIVATE"}}}"#;
    let nested = [
        r#"{"type":"response.failed","response":{"id":"r1","error":["rate_limit_error","rate_limit_exceeded","limited at-FAKE acct-PRIVATE",null,null,null]}}"#,
        r#"{"type":"response.failed","response":{"id":"r1","error":["rate_limit_error","rate_limit_exceeded","limited at-FAKE acct-PRIVATE",null,null]}}"#,
        r#"{"type":"response.failed","response":{"id":"r1","error":{"$serde_json::private::RawValue":"[\"rate_limit_error\",\"rate_limit_exceeded\",\"limited at-FAKE acct-PRIVATE\",null,null,null]"}}}"#,
        r#"{"type":"response.failed","response":{"id":"r1","error":["rate_limit_\u0065rror","rate_limit_exceeded","limited at-\u0046AKE acct-PRIVATE",null,null,null]}}"#,
        r#"{"type":"response.failed","response":{"id":"r1","error":["rate_limit_error",{"$serde_json::private::RawValue":"\"rate_limit_exceeded\""},"limited at-FAKE acct-PRIVATE",null,null,null]}}"#,
        r#"["response.failed",null,null,{"id":"r1","error":["rate_limit_error","rate_limit_exceeded","limited at-FAKE acct-PRIVATE",null,null,null]},null,null,null,null,null,null,null,null,null,null]"#,
        r#"{"type":"response.failed","response":{"$serde_json::private::RawValue":"{\"id\":\"r1\",\"error\":[\"rate_limit_error\",\"rate_limit_exceeded\",\"limited at-FAKE acct-PRIVATE\",null,null,null]}"}}"#,
    ];
    let same = |raw: &str, object: &str, t: &Turned| -> Vec<String> {
        t.frames
            .iter()
            .map(|f| {
                if f == raw || f == object {
                    "EVENT".to_owned()
                } else {
                    f.clone()
                }
            })
            .collect()
    };
    for (stage, before) in [("first", None), ("after content", Some(NESTED_DELTA))] {
        for path in NATIVE_PATHS {
            let frames =
                |event: &'static str| -> Vec<&'static str> { before.into_iter().chain([event, NESTED_END]).collect() };
            let want = native_turn(path, &frames(OBJECT), NESTED_END).await;
            assert!(!discloses(&want), "{} {stage}: object control: {want:?}", path.0);
            assert_eq!(want.records[0]["failed"], true, "{} {stage}: {want:?}", path.0);
            assert_eq!(
                want.records[0]["fail"]["status_code"]
                    .as_u64()
                    .map(|s| s == 429 || s == 503),
                Some(true),
                "{} {stage}: {want:?}",
                path.0
            );
            for event in nested {
                let case = format!("{} {stage}: {event}", path.0);
                assert!(cpa_exec::codex_event::decode(event).is_some(), "{case}");
                let got = native_turn(path, &frames(event), NESTED_END).await;
                assert!(
                    !discloses(&got),
                    "{case}: the token or account reached the client: {got:?}"
                );
                assert_eq!(same(event, OBJECT, &got), same(event, OBJECT, &want), "{case}");
                assert_eq!(got.closed, want.closed, "{case}");
                assert_eq!(got.cooldowns, want.cooldowns, "{case}");
                assert_eq!(got.attempts, want.attempts, "{case}");
                assert_eq!(got.records[0]["failed"], want.records[0]["failed"], "{case}");
                let fail = |t: &Turned| {
                    let body = t.records[0]["fail"]["body"].as_str().unwrap_or_default().to_owned();
                    // The record keeps the frame's own spelling; compare what it says.
                    (
                        t.records[0]["fail"]["status_code"].clone(),
                        serde_json::from_str::<serde_json::Value>(&body).ok(),
                    )
                };
                // The record keeps no spelling of the sent token (#25, AGENTS.md): written
                // as sent it becomes `[REDACTED]` in place, so the body still says what the
                // object form's says; escaped, the body keeps only its length.
                if event.contains(r"at-\u0046AKE") {
                    let body = got.records[0]["fail"]["body"].as_str().unwrap_or_default();
                    assert_eq!(fail(&got).0, fail(&want).0, "{case}");
                    assert!(body.starts_with("[REDACTED: "), "{case}: {body}");
                } else {
                    assert_eq!(fail(&got), fail(&want), "{case}: {got:?} {want:?}");
                }
                assert_eq!(tokens(&got.records[0]), tokens(&want.records[0]), "{case}");
            }
            let short =
                r#"{"type":"response.failed","response":{"id":"r1","error":["server_error","x","generic",null]}}"#;
            let got = native_turn(path, &frames(short), NESTED_END).await;
            assert!(
                got.frames.iter().any(|f| f == short),
                "{} {stage}: as sent: {got:?}",
                path.0
            );
            assert_eq!(got.records[0]["failed"], true, "{} {stage}: {got:?}", path.0);
            assert_eq!(
                got.records[0]["fail"]["status_code"], 502,
                "{} {stage}: {got:?}",
                path.0
            );
        }
    }
}

/// #6 (review finding 2): the pinned wrapped mapping keeps an explicit `status: 400` for
/// the scheduler, but the frame's own error is a rate limit with the credential's token.
/// An upstream socket fails the turn with 400 (no cooldown, as for any 400) and the frame
/// never reaches the client, nor any rendering of its error: the turn ends silently (the
/// client is no subscription-limit case). A turn served over HTTP reads the event's error
/// alone, a 429, and withholds it too. The wrapped arrays an SSE reader cannot decode are
/// withheld as account-shaped frames and the real completion follows. Resolved, raw error
/// object and raw code; first and after content. (A wrapped array whose code is a private
/// raw value decodes as neither pinned struct, so the pinned reader skips it:
/// `refused_failure_slots_never_reach_the_client` covers it.)
#[tokio::test]
async fn explicit_status_account_errors_are_withheld_on_every_native_path() {
    let error = r#"{"type":"rate_limit_error","code":"budget_reached","message":"limited","token":"at-FAKE","private_account":"acct-PRIVATE"}"#;
    let raw_error = serde_json::json!({"$serde_json::private::RawValue": error}).to_string();
    let raw_code = r#"{"type":"rate_limit_error","code":{"$serde_json::private::RawValue":"\"budget_reached\""},"message":"limited","token":"at-FAKE","private_account":"acct-PRIVATE"}"#;
    let mut cases = Vec::new();
    for (label, e) in [
        ("object", error.to_owned()),
        ("raw error", raw_error),
        ("raw code", raw_code.to_owned()),
    ] {
        cases.push((
            label.to_owned(),
            format!(r#"{{"type":"error","status":400,"error":{e}}}"#),
        ));
        if label != "raw code" {
            cases.push((format!("{label} array"), format!(r#"["error",400,{e}]"#)));
        }
    }
    for (stage, before) in [("first", None), ("after content", Some(NESTED_DELTA))] {
        for path in NATIVE_PATHS {
            let socket = path.2;
            for (label, event) in &cases {
                let case = format!("{} {stage}, {label}: {event}", path.0);
                let frames: Vec<&str> = before.into_iter().chain([event.as_str(), NESTED_END]).collect();
                let got = native_turn(path, &frames, NESTED_END).await;
                assert!(!discloses(&got), "{case}: {got:?}");
                assert!(!got.frames.iter().any(|f| f == event), "{case}");
                let array = label.ends_with("array");
                // The upstream socket reads every form as the wrapped 400; HTTP decodes only
                // the objects, as the 429 their error is.
                let fails = socket || !array;
                let lead: Vec<String> = before.iter().map(|d| (*d).to_owned()).collect();
                if fails {
                    assert_eq!(got.frames, lead, "{case}: {got:?}");
                    assert_eq!(got.records[0]["failed"], true, "{case}: {got:?}");
                    let status = if socket { 400 } else { 429 };
                    assert_eq!(got.records[0]["fail"]["status_code"], status, "{case}: {got:?}");
                    if socket {
                        assert_eq!(got.cooldowns, 0, "{case}: a 400 cools nothing");
                    }
                } else {
                    let mut want = lead;
                    want.push(NESTED_END.to_owned());
                    assert_eq!(got.frames, want, "{case}: {got:?}");
                    assert_eq!(got.records[0]["failed"], false, "{case}: {got:?}");
                    assert_eq!(tokens(&got.records[0]), [4, 1, 5], "{case}");
                    assert_eq!(got.cooldowns, 0, "{case}");
                }
            }
        }
    }
}

/// #6 (review finding 2) on a later response of a steering connection: the first
/// response completes, the next create is answered with the explicit-400 rate limit. It
/// never reaches the client as an event: the connection ends, the create's record fails
/// with the wrapped 400, and nothing cools. The same create answered with a plain 400 is
/// the recoverable event the client may correct (the control).
#[tokio::test]
async fn a_later_steering_failure_that_reads_as_an_account_failure_is_withheld() {
    const CREATED: &str = r#"{"type":"response.created","response":{"id":"steer_first","output":[]}}"#;
    const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"steer_first","output":[],"usage":{"input_tokens":3,"output_tokens":0,"total_tokens":3}}}"#;
    const ACCOUNT: &str = r#"{"type":"error","status":400,"response_id":"rejected","error":{"type":"rate_limit_error","code":"budget_reached","message":"limited","token":"at-FAKE","private_account":"acct-PRIVATE"}}"#;
    const PLAIN: &str = r#"{"type":"error","status":400,"response_id":"rejected","error":{"type":"invalid_request_error","message":"correct this create"}}"#;
    let create = client_frame("ws-create-first", "gpt-5.5");
    let next = r#"{"type":"response.create","model":"gpt-5.5","previous_response_id":"steer_first","input":[]}"#;
    for (rejection, withheld) in [(ACCOUNT, true), (PLAIN, false)] {
        let mut upstream = Gated::start(
            vec![
                vec![CREATED.to_owned(), COMPLETED.to_owned()],
                vec![rejection.to_owned()],
            ],
            false,
        )
        .await;
        let proxy = proxy_with(Script::default(), STEERING, |_| {
            vec![credential(Profile::Chatgpt, &upstream.base_url, true)]
        })
        .await;
        proxy.rt.usage_queue().configure(
            true,
            &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
        );
        let mut socket = connect(&proxy).await;
        socket.send(Message::text(create.clone())).await.unwrap();
        upstream.read(1).await;
        upstream.release.notify_one();
        let first = read(&mut socket, &TERMINAL).await;
        assert_eq!(first.frames.last().map(String::as_str), Some(COMPLETED));
        assert_eq!(records(&proxy, 1).await[0]["failed"], false);
        socket.send(Message::text(next)).await.unwrap();
        upstream.read(2).await;
        let got = read(&mut socket, &["error"]).await;
        assert!(
            !got.frames
                .iter()
                .any(|f| f.contains("at-FAKE") || f.contains("acct-PRIVATE")),
            "{rejection}: {got:?}"
        );
        let rejected = records(&proxy, 1).await;
        assert_eq!(rejected[0]["failed"], true, "{rejection}");
        assert_eq!(rejected[0]["fail"]["status_code"], 400, "{rejection}");
        if withheld {
            assert!(got.frames.is_empty(), "{rejection}: {got:?}");
            assert!(got.closed.is_some(), "{rejection}: the connection ends");
        } else {
            assert_eq!(got.frames, [PLAIN], "{rejection}");
            assert_eq!(got.closed, None, "{rejection}");
        }
        let credential = proxy.rt.store().snapshot()[0].id.clone();
        assert!(proxy.rt.store().cooldowns(&credential).is_empty(), "{rejection}");
    }
}

/// #25 (review finding 3): an explicit status that the pinned mapping turns into no error
/// is a notice, whatever spells it: zero, `-0`, a negative or too large number, a string,
/// a private number, or `status_code`. With a private raw value as `error.code` the frame
/// is no pinned wrapped event and the pinned `error` branch goes on (`x` is no terminal
/// provider code), as it does for the same frame with the code resolved. On the three
/// upstream-socket paths, first and after content, the notice reaches the client and the
/// real completion follows, with its usage and no cooldown. A logical rate limit is
/// withheld without failing; a logical terminal provider code still fails; a missing
/// status keeps the status-less failure reading. A turn served over HTTP keeps its own
/// eager reading of every decoded `error` event, unchanged.
#[tokio::test]
async fn explicit_zero_status_notices_keep_the_real_completion() {
    let frame = |member: &str, code: &str, private: bool| {
        let code = if private {
            format!(r#"{{"$serde_json::private::RawValue":"\"{code}\""}}"#)
        } else {
            format!(r#""{code}""#)
        };
        let member = if member.is_empty() {
            String::new()
        } else {
            format!("{member},")
        };
        format!(r#"{{"type":"error",{member}"error":{{"code":{code},"message":"notice"}}}}"#)
    };
    let statuses = [
        r#""status":0"#,
        r#""status":-0"#,
        r#""status":-1"#,
        r#""status":70000"#,
        r#""status":"0""#,
        r#""status":{"$serde_json::private::Number":"0"}"#,
        r#""status_code":0"#,
    ];
    for (stage, before) in [("first", None), ("after content", Some(NESTED_DELTA))] {
        for path in NATIVE_PATHS {
            let lead: Vec<String> = before.iter().map(|d| (*d).to_owned()).collect();
            for status in statuses {
                for private in [true, false] {
                    let notice = frame(status, "x", private);
                    let limited = frame(status, "rate_limit_exceeded", private);
                    let terminal = frame(status, "budget_reached", private);
                    let turn = |event: &str| {
                        let frames: Vec<String> = lead
                            .iter()
                            .cloned()
                            .chain([event.to_owned(), NESTED_END.to_owned()])
                            .collect();
                        async move {
                            let frames: Vec<&str> = frames.iter().map(String::as_str).collect();
                            native_turn(path, &frames, NESTED_END).await
                        }
                    };
                    let case = format!("{} {stage}, {status}, private={private}", path.0);
                    let got = turn(&notice).await;
                    if path.2 {
                        let mut want = lead.clone();
                        want.extend([notice.clone(), NESTED_END.to_owned()]);
                        assert_eq!(got.frames, want, "{case}: notice");
                        assert_eq!(got.records[0]["failed"], false, "{case}: {got:?}");
                        assert_eq!(tokens(&got.records[0]), [4, 1, 5], "{case}");
                        assert_eq!(got.cooldowns, 0, "{case}");
                        let got = turn(&limited).await;
                        let mut want = lead.clone();
                        want.push(NESTED_END.to_owned());
                        assert_eq!(got.frames, want, "{case}: withheld rate limit");
                        assert_eq!(got.records[0]["failed"], false, "{case}: {got:?}");
                        assert_eq!(got.cooldowns, 0, "{case}");
                    } else {
                        // The HTTP-backed turn's eager reading: every decoded `error` fails.
                        assert_eq!(got.records[0]["failed"], true, "{case}: {got:?}");
                    }
                    let got = turn(&terminal).await;
                    assert_eq!(
                        got.records[0]["failed"], true,
                        "{case}: terminal provider code: {got:?}"
                    );
                    assert!(!got.frames.iter().any(|f| f == NESTED_END), "{case}");
                }
            }
            let statusless = frame("", "x", true);
            let frames: Vec<&str> = before.into_iter().chain([statusless.as_str(), NESTED_END]).collect();
            let got = native_turn(path, &frames, NESTED_END).await;
            assert_eq!(
                got.records[0]["failed"], true,
                "{} {stage}: status-less: {got:?}",
                path.0
            );
        }
    }
}

/// The pinned readings of `group` in the exec fixture `codex_pinned_notice_readings.json`
/// (its `provenance` names the oracle): (name, frame, the pinned WebSocket decision).
fn notice_readings(group: &str) -> Vec<(String, String, String)> {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../cpa-exec/tests/fixtures/codex_pinned_notice_readings.json"
    ))
    .unwrap();
    fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["group"] == group)
        .map(|c| {
            let field = |k: &str| c[k].as_str().unwrap().to_owned();
            (field("name"), field("raw"), field("ws"))
        })
        .collect()
}

/// Whether a `refused` fixture frame reads as an account failure by position: all but
/// the safe, empty and opaque controls.
fn refused_account(name: &str) -> bool {
    !name.contains("safe") && !name.contains("empty") && !name.starts_with("opaque")
}

/// A turn on `path` whose upstream sends `lead`, `event` and the real completion.
async fn turn_with(path: (&str, &str, bool), lead: &[String], event: &str) -> Turned {
    let frames: Vec<&str> = lead.iter().map(String::as_str).chain([event, NESTED_END]).collect();
    native_turn(path, &frames, NESTED_END).await
}

/// #25 criterion 7 and #6 (review findings 1 and 2, every frame of the `refused` group
/// of the pinned notice readings). A failure slot the pinned reader refuses to decode
/// still carries what the upstream wrote, and the client (whose key is `client-key`)
/// never receives the selected credential's token `at-FAKE` or the account in it, as
/// sent, escaped, or inside a private raw value; the upstream received `Bearer at-FAKE`.
///
/// - A `response.failed` whose `response.error` array the `Error` struct refuses (three,
///   four or seven members, a string `resets_at`, an escaped token, a private raw value
///   as its message), or whose own `error` is such an array, is the pinned generic stream
///   error, and an `error` event whose `error` is such an array keeps the status-less
///   failure reading: each fails as the safe refused array does (502, the same close,
///   cooldowns, attempts and usage), but the client gets nothing of it, and no rendering
///   of its error.
/// - A wrapped array neither pinned struct accepts (a private raw value as its code or
///   status, five members) is skipped, as the pinned reader skips it: it never reaches the
///   client, the real completion follows, the turn succeeds with its usage and nothing
///   cools.
/// - The safe refused array, the safe wrapped array and opaque arrays reach the client as
///   sent, with their own outcomes.
///
/// Five native paths, first and after content.
#[tokio::test]
async fn refused_failure_slots_never_reach_the_client() {
    let cases = notice_readings("refused");
    assert_eq!(cases.len(), 21);
    let safe = cases.iter().find(|c| c.0 == "refused-nested-safe").unwrap().1.clone();
    for (stage, before) in [("first", None), ("after content", Some(NESTED_DELTA))] {
        for path in NATIVE_PATHS {
            let lead: Vec<String> = before.iter().map(|d| (*d).to_owned()).collect();
            let control = turn_with(path, &lead, &safe).await;
            let mut want = lead.clone();
            want.push(safe.clone());
            assert_eq!(control.frames, want, "{} {stage}: the safe failure as sent", path.0);
            assert_eq!(control.records[0]["failed"], true, "{} {stage}", path.0);
            assert_eq!(control.records[0]["fail"]["status_code"], 502, "{} {stage}", path.0);
            for (name, raw, ws) in &cases {
                let case = format!("{} {stage}, {name}: {raw}", path.0);
                let got = turn_with(path, &lead, raw).await;
                assert!(
                    !discloses(&got),
                    "{case}: the token or account reached the client: {got:?}"
                );
                assert_eq!(got.authorization, ["Bearer at-FAKE"], "{case}: the selected credential");
                // The group's read-on frames are decoded `error` events without a status,
                // which the proxy's documented status-less reading fails.
                if ws == "read-on" {
                    assert!(raw.starts_with(r#"{"type":"error","error":"#), "{case}");
                }
                let ws = if ws == "read-on" { "fail" } else { ws.as_str() };
                match (ws, refused_account(name)) {
                    ("fail", account) => {
                        let mut want = lead.clone();
                        if !account {
                            want.push(raw.clone());
                        }
                        assert_eq!(got.frames, want, "{case}");
                        assert_eq!(got.closed, control.closed, "{case}");
                        assert_eq!(got.cooldowns, control.cooldowns, "{case}");
                        assert_eq!(got.attempts, control.attempts, "{case}");
                        assert_eq!(got.records[0]["failed"], true, "{case}");
                        assert_eq!(got.records[0]["fail"]["status_code"], 502, "{case}: {got:?}");
                        assert_eq!(tokens(&got.records[0]), tokens(&control.records[0]), "{case}");
                    }
                    ("skip", account) => {
                        let mut want = lead.clone();
                        if !account {
                            want.push(raw.clone());
                        }
                        want.push(NESTED_END.to_owned());
                        assert_eq!(got.frames, want, "{case}");
                        assert_eq!(got.closed, None, "{case}");
                        assert_eq!(got.records[0]["failed"], false, "{case}: {got:?}");
                        assert_eq!(tokens(&got.records[0]), [4, 1, 5], "{case}");
                        assert_eq!(got.cooldowns, 0, "{case}");
                        assert_eq!(got.attempts, 1, "{case}");
                    }
                    (ws, _) => panic!("{case}: pinned {ws}"),
                }
            }
        }
    }
}

/// #25 criterion 7 and #6 on a later response of a steering connection: after a first
/// response that completes, the next create is answered with a refused failure slot. The
/// refused `response.failed` array that reads as a rate limit by position is withheld:
/// the connection ends, the create's record fails with the generic 502, and the client
/// gets neither the frame nor the token. The refused wrapped array is skipped: the real
/// completion of that response follows and succeeds. The safe refused array is the
/// recoverable event the client may correct (the control).
#[tokio::test]
async fn later_steering_responses_keep_refused_slots_from_the_client() {
    const CREATED: &str = r#"{"type":"response.created","response":{"id":"steer_first","output":[]}}"#;
    const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"steer_first","output":[],"usage":{"input_tokens":3,"output_tokens":0,"total_tokens":3}}}"#;
    const NEXT_CREATED: &str = r#"{"type":"response.created","response":{"id":"steer_second","output":[]}}"#;
    const NEXT_COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"steer_second","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    let refused = notice_readings("refused");
    let raw = |name: &str| refused.iter().find(|c| c.0 == name).unwrap().1.clone();
    let create = client_frame("ws-create-first", "gpt-5.5");
    let next = r#"{"type":"response.create","model":"gpt-5.5","previous_response_id":"steer_first","input":[]}"#;
    for name in [
        "refused-nested-three",
        "refused-nested-bad-resets",
        "refused-nested-escaped",
        "refused-wrapped-private-code-message",
        "refused-wrapped-escaped",
        "refused-nested-safe",
    ] {
        let event = raw(name);
        let skipped = name.starts_with("refused-wrapped");
        let second = if skipped {
            vec![NEXT_CREATED.to_owned(), event.clone(), NEXT_COMPLETED.to_owned()]
        } else {
            vec![NEXT_CREATED.to_owned(), event.clone()]
        };
        let mut upstream = Gated::start(vec![vec![CREATED.to_owned(), COMPLETED.to_owned()], second], false).await;
        let proxy = proxy_with(Script::default(), STEERING, |_| {
            vec![credential(Profile::Chatgpt, &upstream.base_url, true)]
        })
        .await;
        proxy.rt.usage_queue().configure(
            true,
            &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
        );
        let mut socket = connect(&proxy).await;
        socket.send(Message::text(create.clone())).await.unwrap();
        upstream.read(1).await;
        upstream.release.notify_one();
        let first = read(&mut socket, &TERMINAL).await;
        assert_eq!(first.frames.last().map(String::as_str), Some(COMPLETED), "{name}");
        assert_eq!(records(&proxy, 1).await[0]["failed"], false, "{name}");
        socket.send(Message::text(next)).await.unwrap();
        upstream.read(2).await;
        let mut got = read(&mut socket, &TERMINAL).await;
        assert!(!got.frames.iter().any(|f| carries_secret(f)), "{name}: {got:?}");
        // A later create's model-only metadata comes ahead of its first event (the duplex's
        // own frame, not the upstream's).
        assert_eq!(
            got.frames
                .first()
                .map(|f| cpa_common::rawjson::get(f, "type").str().to_owned()),
            Some("response.metadata".to_owned()),
            "{name}: {got:?}"
        );
        got.frames.remove(0);
        let settled = records(&proxy, 1).await;
        if skipped {
            assert_eq!(got.frames, [NEXT_CREATED, NEXT_COMPLETED], "{name}");
            assert_eq!(got.closed, None, "{name}");
            assert_eq!(settled[0]["failed"], false, "{name}: {settled:?}");
            assert_eq!(tokens(&settled[0]), [4, 1, 5], "{name}");
            let credential = proxy.rt.store().snapshot()[0].id.clone();
            assert!(proxy.rt.store().cooldowns(&credential).is_empty(), "{name}");
        } else if refused_account(name) {
            assert_eq!(got.frames, [NEXT_CREATED], "{name}: withheld");
            assert!(got.closed.is_some(), "{name}: the connection ends");
            assert_eq!(settled[0]["failed"], true, "{name}");
            assert_eq!(settled[0]["fail"]["status_code"], 502, "{name}");
        } else {
            assert_eq!(got.frames, [NEXT_CREATED, event.as_str()], "{name}: as sent");
            assert_eq!(got.closed, None, "{name}: recoverable");
            assert_eq!(settled[0]["failed"], true, "{name}");
            assert_eq!(settled[0]["fail"]["status_code"], 502, "{name}");
        }
        assert_eq!(proxy.upstream.sockets().len(), 0, "{name}: the gated upstream alone");
    }
}

/// #25 (review finding 3, the `status` group of the pinned notice readings): a status
/// the wrapped struct does not read as a `u16`, or a status the struct refuses as a
/// duplicate field, makes no wrapped status, and the pinned `error` branch reads no
/// status: with a code that is no terminal provider code the reader goes on. On the three
/// upstream-socket paths, first and after content, the turn goes on to the real
/// completion, succeeds with its usage, makes one attempt and cools nothing, whether or
/// not Go's integer reading takes the status as 429 (`"429"`, `" 429"`, `"+429"`,
/// `429.0`, `4.29e2`, `429e0`, a private number or raw value, `status_code` so spelled,
/// both members, a repeated `status`). The notice itself reaches the client as sent, or
/// is withheld when a reading of it is an account failure: it is never changed. A
/// wrapped status the struct reads (`"status":429`, an escaped `status` key) still fails
/// the turn by it (the controls). A turn served over HTTP keeps its own eager reading.
#[tokio::test]
async fn invalid_status_notices_keep_the_real_completion() {
    let cases = notice_readings("status");
    let mut checked = [0usize; 2];
    for (name, raw, ws) in &cases {
        // Every spelling with the safe code `x` and with a logical rate limit, plain and
        // private; the terminal provider codes fail, as `codex_ws_tests` checks per frame.
        let code = ["-x-", "-rate_limit_exceeded-"].iter().any(|c| name.contains(c));
        if !code || ws != "read-on" || name.contains("dup-null-null") {
            continue;
        }
        for (stage, before) in [("first", None), ("after content", Some(NESTED_DELTA))] {
            for path in NATIVE_PATHS.iter().filter(|p| p.2) {
                let lead: Vec<String> = before.iter().map(|d| (*d).to_owned()).collect();
                let case = format!("{} {stage}, {name}: {raw}", path.0);
                let got = turn_with(*path, &lead, raw).await;
                let mut withheld = lead.clone();
                withheld.push(NESTED_END.to_owned());
                let mut delivered = lead.clone();
                delivered.extend([raw.clone(), NESTED_END.to_owned()]);
                assert!(got.frames == withheld || got.frames == delivered, "{case}: {got:?}");
                checked[usize::from(got.frames == withheld)] += 1;
                assert_eq!(got.closed, None, "{case}");
                assert_eq!(got.records[0]["failed"], false, "{case}: {got:?}");
                assert_eq!(tokens(&got.records[0]), [4, 1, 5], "{case}");
                assert_eq!(got.cooldowns, 0, "{case}");
                assert_eq!(got.attempts, 1, "{case}");
            }
        }
    }
    // Both outcomes occur: `"status":" 429"` is no integer to Go either, `"429"` is.
    assert!(checked[0] > 50 && checked[1] > 50, "{checked:?}");
    for raw in [
        r#"{"type":"error","status":429,"error":{"code":"x","message":"notice"}}"#.to_owned(),
        format!(
            r#"{{"type":"error","st{}u0061tus":429,"error":{{"code":"x","message":"notice"}}}}"#,
            '\\'
        ),
    ] {
        for path in NATIVE_PATHS.iter().filter(|p| p.2) {
            let got = turn_with(*path, &[], &raw).await;
            assert_eq!(got.records[0]["failed"], true, "{} {raw}: {got:?}", path.0);
            assert_eq!(got.records[0]["fail"]["status_code"], 429, "{} {raw}", path.0);
            assert!(!got.frames.iter().any(|f| f == NESTED_END), "{} {raw}", path.0);
        }
    }
}

// #25 acceptance criterion 7, the outbound credential guard (`cpa_exec::codex_secret`):
// the selected credential is the fixture's `at-FAKE`, which every attempt sends upstream
// as `Bearer at-FAKE`, while the client's own key is `client-key`. Generated frames, no
// recorded provider traffic.

/// The selected credential's token, as every attempt of [`native_turn`] carries it.
const SELECTED: &str = "at-FAKE";
/// The same shape of text that is no credential this proxy sent: the control.
const LOOKALIKE: &str = "at-OTHER";

/// `text` with each character of its token written as a JSON `\u` escape.
fn unicode_escaped(token: &str) -> String {
    token.chars().map(|c| format!("\\u{:04x}", u32::from(c))).collect()
}

/// Whether `text` carries `token` as written or in a spelling the guard cases use.
fn carries_token(text: &str, token: &str) -> bool {
    let escaped_dash = token.replacen('-', "\\u002d", 1);
    text.contains(token) || text.contains(&unicode_escaped(token)) || text.contains(&escaped_dash)
}

/// The cases of the 115 residual conversations and their neighbours. `{T}` is the token.
/// `account` marks a pinned account kind, which a native WebSocket client never gets as
/// sent, token or not (`codex_ws::pinned_account_kind`).
const FAILURES: [(&str, &str, bool); 9] = [
    (
        "accepted Error, slow_down (pinned RateLimitExceeded)",
        r#"{"type":"response.failed","response":{"id":"r1","error":["rate_limit_error","slow_down","limited {T}",null,null]}}"#,
        true,
    ),
    (
        "accepted Error, insufficient_quota (pinned QuotaExceeded)",
        r#"{"type":"response.failed","response":{"id":"r1","error":["insufficient_quota","insufficient_quota","quota {T}",null,null,null]}}"#,
        true,
    ),
    (
        "object usage_not_included (pinned UsageNotIncluded)",
        r#"{"type":"response.failed","response":{"id":"r1","error":{"code":"usage_not_included","message":"plan {T}"}}}"#,
        true,
    ),
    (
        "accepted Error, invalid_prompt",
        r#"{"type":"response.failed","response":{"id":"r1","error":["invalid_request_error","invalid_prompt","bad {T}",null,null]}}"#,
        false,
    ),
    (
        "generic Retryable",
        r#"{"type":"response.failed","response":{"id":"r1","error":{"code":"x","message":"retry {T}"}}}"#,
        false,
    ),
    (
        "earlier duplicate error, last one safe",
        r#"{"type":"response.failed","response":{"id":"r1","error":{"code":"x","message":"{T}"},"error":{"code":"x","message":"safe"}}}"#,
        false,
    ),
    (
        "response is an array",
        r#"{"type":"response.failed","response":["r1",{"message":"{T}"}]}"#,
        false,
    ),
    (
        "escaped token",
        r#"{"type":"response.failed","response":{"id":"r1","error":{"code":"x","message":"{E}"}}}"#,
        false,
    ),
    (
        "private raw value",
        r#"{"type":"response.failed","response":{"id":"r1","error":{"$serde_json::private::RawValue":"{\"code\":\"x\",\"message\":\"{T}\"}"}}}"#,
        false,
    ),
];

/// Frames the pinned reader skips, ignores or reads as content: the real completion
/// follows each.
const PASSED: [(&str, &str); 5] = [
    (
        "malformed JSON",
        r#"{"type":"error","error":{"code":"x","message":"{T}""#,
    ),
    (
        "wrapped notice",
        r#"{"type":"error","status":200,"error":{"code":"x","message":"notice {T}"}}"#,
    ),
    ("unknown event", r#"{"type":"response.future","note":["{T}",1.50]}"#),
    (
        "escaped delta",
        r#"{"type":"response.output_text.delta","response_id":"r1","delta":"{E}"}"#,
    ),
    (
        "repeated member, earlier one carries it",
        r#"{"type":"error","status":200,"error":{"message":"{T}"},"error":{"code":"x","message":"safe"}}"#,
    ),
];

fn with_token(frame: &str, token: &str) -> String {
    frame.replace("{T}", token).replace("{E}", &unicode_escaped(token))
}

/// #25 criterion 7 on every native WebSocket path, first and after content: no frame or
/// close carries the selected credential. Each case also runs with [`LOOKALIKE`], text no
/// request carried, and the two runs differ only in what the client receives: the same
/// record (failed or not, status, tokens), cooldowns and attempts. The control keeps its
/// bytes: a failure that is no pinned account kind reaches the client as sent, and a
/// passed frame reaches it before the real completion. A failure that carries the token
/// is withheld (silent close, its classified status kept); a passed frame that carries
/// it is dropped and, on an upstream socket, the real completion follows, with no
/// cooldown.
#[tokio::test]
async fn the_selected_credential_never_reaches_a_websocket_client() {
    const REAL_END: &str = r#"{"type":"response.completed","response":{"id":"resp_real","status":"completed","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    const DELTA: &str = r#"{"type":"response.output_text.delta","response_id":"r1","delta":"hello"}"#;
    for path in NATIVE_PATHS {
        for after in [false, true] {
            let prefix: Vec<&str> = if after { vec![DELTA] } else { vec![] };
            for (name, frame, account) in FAILURES {
                let case = format!("{} {after} {name}", path.0);
                let selected = with_token(frame, SELECTED);
                let control = with_token(frame, LOOKALIKE);
                let run = |frame: String| {
                    let mut frames: Vec<String> = prefix.iter().map(|f| f.to_string()).collect();
                    frames.push(frame);
                    frames.push(REAL_END.to_owned());
                    async move {
                        let frames: Vec<&str> = frames.iter().map(String::as_str).collect();
                        native_turn(path, &frames, REAL_END).await
                    }
                };
                let got = run(selected.clone()).await;
                let want = run(control.clone()).await;
                assert_eq!(got.authorization, [format!("Bearer {SELECTED}")], "{case}");
                assert!(
                    !got.frames.iter().any(|f| carries_token(f, SELECTED)),
                    "{case}: {got:?}"
                );
                assert_eq!(got.frames, prefix, "{case}: withheld");
                assert!(got.closed.is_some(), "{case}: the turn ends");
                let mut sent = prefix.clone();
                if !account {
                    sent.push(&control);
                }
                assert_eq!(want.frames, sent, "{case}: the control as sent");
                assert_eq!(got.records[0]["failed"], want.records[0]["failed"], "{case}");
                assert_eq!(
                    got.records[0]["fail"]["status_code"], want.records[0]["fail"]["status_code"],
                    "{case}"
                );
                assert_eq!(got.records[0]["failed"], true, "{case}: {got:?}");
                assert_eq!(tokens(&got.records[0]), tokens(&want.records[0]), "{case}");
                assert_eq!((got.cooldowns, got.attempts), (want.cooldowns, want.attempts), "{case}");
            }
            for (name, frame) in PASSED {
                let case = format!("{} {after} {name}", path.0);
                let selected = with_token(frame, SELECTED);
                let control = with_token(frame, LOOKALIKE);
                let mut frames = prefix.clone();
                frames.extend([selected.as_str(), REAL_END]);
                let got = native_turn(path, &frames, REAL_END).await;
                let mut frames = prefix.clone();
                frames.extend([control.as_str(), REAL_END]);
                let want = native_turn(path, &frames, REAL_END).await;
                assert_eq!(got.authorization, [format!("Bearer {SELECTED}")], "{case}");
                // Privacy changes delivery only: the control's frames without the control
                // frame, and the same record, cooldowns, attempts and end.
                let mut expected = want.frames.clone();
                expected.retain(|f| *f != control);
                assert_eq!(got.frames, expected, "{case}");
                assert!(want.frames.contains(&control), "{case}: the control as sent: {want:?}");
                assert_eq!(got.closed.is_some(), want.closed.is_some(), "{case}");
                assert_eq!(got.records[0]["failed"], want.records[0]["failed"], "{case}");
                assert_eq!(
                    got.records[0]["fail"]["status_code"], want.records[0]["fail"]["status_code"],
                    "{case}"
                );
                assert_eq!(tokens(&got.records[0]), tokens(&want.records[0]), "{case}");
                assert_eq!((got.cooldowns, got.attempts), (want.cooldowns, want.attempts), "{case}");
                // On an upstream socket the pinned reader skips or ignores each of them: the
                // real completion follows. A turn over HTTP keeps its eager failure policy
                // for a decoded `error` that carries a status.
                if path.2 {
                    let mut kept = prefix.clone();
                    kept.push(REAL_END);
                    assert_eq!(got.frames, kept, "{case}: dropped, completion follows");
                    assert_eq!(got.closed, None, "{case}");
                    assert_eq!(got.records[0]["failed"], false, "{case}: {got:?}");
                    assert_eq!(tokens(&got.records[0]), [4, 1, 5], "{case}");
                    assert_eq!(got.cooldowns, 0, "{case}");
                    assert_eq!(got.attempts, 1, "{case}");
                }
            }
        }
    }
}

/// #6 by the pinned client's own failure kinds (`codex-api/src/sse/responses_error.rs`
/// and `api_bridge.rs` at 44ccf394573c): a `response.failed` whose error is
/// `RateLimitExceeded`, `QuotaExceeded` or `UsageNotIncluded` describes the pooled
/// account, whatever status Go's table gives it, so a native WebSocket client never gets
/// it as sent, with no token in it. The scheduler keeps Go's classification (the same
/// status and cooldowns as a safe request fault of that status would get is not asserted;
/// only that withholding changes nothing but delivery). Safe request faults (an invalid
/// prompt, a context window, a generic retryable error) reach the client as sent.
#[tokio::test]
async fn pinned_account_kinds_are_withheld_without_a_token() {
    const REAL_END: &str = r#"{"type":"response.completed","response":{"id":"resp_real","status":"completed","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    let failed = |code: &str| {
        format!(
            r#"{{"type":"response.failed","response":{{"id":"r1","error":{{"code":"{code}","message":"limit reached for org-PUBLIC","plan_type":"plus"}}}}}}"#
        )
    };
    for path in NATIVE_PATHS {
        for code in [
            "slow_down",
            "rate_limit_exceeded",
            "insufficient_quota",
            "credit_balance_exhausted",
            "organization_spend_limit_exceeded",
            "project_spend_limit_exceeded",
            "usage_not_included",
        ] {
            let frame = failed(code);
            let got = native_turn(path, &[&frame, REAL_END], REAL_END).await;
            assert!(got.frames.is_empty(), "{} {code}: {got:?}", path.0);
            assert!(got.closed.is_some(), "{} {code}", path.0);
            assert_eq!(got.records[0]["failed"], true, "{} {code}", path.0);
        }
        for code in ["invalid_prompt", "x", "server_error"] {
            let frame = failed(code);
            let got = native_turn(path, &[&frame, REAL_END], REAL_END).await;
            assert_eq!(got.frames, [frame.as_str()], "{} {code}: as sent", path.0);
            assert_eq!(got.records[0]["failed"], true, "{} {code}", path.0);
        }
    }
}

/// #25 criterion 7 on a steering connection's later responses: after a first response
/// that completes, the next create's failure carries the selected credential. The
/// response's record fails with its classified status, and the connection ends without
/// the frame (the client would otherwise wait for a response that ended). A later
/// skipped notice that carries it is dropped and that response's real completion
/// follows. The lookalike controls reach the client as sent: the recoverable failure
/// keeps the connection open.
#[tokio::test]
async fn later_steering_responses_never_carry_the_selected_credential() {
    const CREATED: &str = r#"{"type":"response.created","response":{"id":"steer_first","output":[]}}"#;
    const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"steer_first","output":[],"usage":{"input_tokens":3,"output_tokens":0,"total_tokens":3}}}"#;
    const NEXT_CREATED: &str = r#"{"type":"response.created","response":{"id":"steer_second","output":[]}}"#;
    const NEXT_COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"steer_second","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    let create = client_frame("ws-create-first", "gpt-5.5");
    let next = r#"{"type":"response.create","model":"gpt-5.5","previous_response_id":"steer_first","input":[]}"#;
    let failure = r#"{"type":"response.failed","response":{"id":"steer_second","error":{"code":"x","message":"retry {T}"},"error":{"code":"x","message":"safe"}}}"#;
    let notice = r#"{"type":"error","status":200,"error":{"code":"x","message":"{E}"}}"#;
    let terminal = r#"{"type":"response.completed","response":{"id":"steer_second","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5},"note":"{T}"}}"#;
    for (name, frame, kind) in [
        ("failure", failure, 0),
        ("notice", notice, 1),
        ("terminal", terminal, 2),
    ] {
        for token in [SELECTED, LOOKALIKE] {
            let case = format!("{name} {token}");
            let event = with_token(frame, token);
            let second = match kind {
                0 => vec![NEXT_CREATED.to_owned(), event.clone()],
                1 => vec![NEXT_CREATED.to_owned(), event.clone(), NEXT_COMPLETED.to_owned()],
                _ => vec![NEXT_CREATED.to_owned(), event.clone()],
            };
            let mut upstream = Gated::start(vec![vec![CREATED.to_owned(), COMPLETED.to_owned()], second], false).await;
            let proxy = proxy_with(Script::default(), STEERING, |_| {
                vec![credential(Profile::Chatgpt, &upstream.base_url, true)]
            })
            .await;
            proxy.rt.usage_queue().configure(
                true,
                &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
            );
            let mut socket = connect(&proxy).await;
            socket.send(Message::text(create.clone())).await.unwrap();
            upstream.read(1).await;
            upstream.release.notify_one();
            let first = read(&mut socket, &TERMINAL).await;
            assert_eq!(first.frames.last().map(String::as_str), Some(COMPLETED), "{case}");
            assert_eq!(records(&proxy, 1).await[0]["failed"], false, "{case}");
            socket.send(Message::text(next)).await.unwrap();
            upstream.read(2).await;
            let mut got = read(&mut socket, &TERMINAL).await;
            assert!(
                !got.frames.iter().any(|f| carries_token(f, SELECTED)),
                "{case}: {got:?}"
            );
            assert_eq!(
                got.frames
                    .first()
                    .map(|f| cpa_common::rawjson::get(f, "type").str().to_owned()),
                Some("response.metadata".to_owned()),
                "{case}: {got:?}"
            );
            got.frames.remove(0);
            let settled = records(&proxy, 1).await;
            let selected = token == SELECTED;
            match (kind, selected) {
                (0, true) => {
                    assert_eq!(got.frames, [NEXT_CREATED], "{case}: withheld");
                    assert!(got.closed.is_some(), "{case}: the connection ends");
                    assert_eq!(settled[0]["failed"], true, "{case}");
                    assert_eq!(settled[0]["fail"]["status_code"], 502, "{case}");
                }
                (0, false) => {
                    assert_eq!(got.frames, [NEXT_CREATED, event.as_str()], "{case}: as sent");
                    assert_eq!(got.closed, None, "{case}: recoverable");
                    assert_eq!(settled[0]["failed"], true, "{case}");
                    assert_eq!(settled[0]["fail"]["status_code"], 502, "{case}");
                }
                (1, _) => {
                    let want: Vec<&str> = if selected {
                        vec![NEXT_CREATED, NEXT_COMPLETED]
                    } else {
                        vec![NEXT_CREATED, event.as_str(), NEXT_COMPLETED]
                    };
                    assert_eq!(got.frames, want, "{case}");
                    assert_eq!(got.closed, None, "{case}");
                    assert_eq!(settled[0]["failed"], false, "{case}");
                    assert_eq!(tokens(&settled[0]), [4, 1, 5], "{case}");
                }
                (_, true) => {
                    // The response completed upstream: its record keeps its usage; the
                    // client gets no completion that carries the token, and the connection
                    // ends instead of leaving it waiting.
                    assert_eq!(got.frames, [NEXT_CREATED], "{case}: withheld");
                    assert!(got.closed.is_some(), "{case}: the connection ends");
                    assert_eq!(settled[0]["failed"], false, "{case}");
                    assert_eq!(tokens(&settled[0]), [4, 1, 5], "{case}");
                }
                (_, false) => {
                    assert_eq!(got.frames, [NEXT_CREATED, event.as_str()], "{case}: as sent");
                    assert_eq!(got.closed, None, "{case}");
                    assert_eq!(settled[0]["failed"], false, "{case}");
                }
            }
            let credential = proxy.rt.store().snapshot()[0].id.clone();
            let cooled = proxy.rt.store().cooldowns(&credential).len();
            assert_eq!(cooled, 0, "{case}: generic 502 and success never cool here");
        }
    }
}

/// One `ws-create-first` turn on `socket`: the frames up to its terminal event.
async fn first_turn(socket: &mut WebSocket) -> Vec<String> {
    let frame = client_frame("ws-create-first", "gpt-5.5");
    socket.send(Message::text(frame)).await.unwrap();
    read(socket, &TERMINAL).await.frames
}

/// The guard follows the values each socket's own handshake carried. A config-backed API
/// key serves a turn on socket A (`key-OLD`); the key then changes in memory (as a
/// refresh would). The next turn reuses socket A, whose upstream only knows `key-OLD`:
/// a frame quoting it is withheld. Socket A closes; the next turn dials socket B with
/// `key-NEW`: a frame quoting `key-NEW` is withheld there, while one quoting `key-OLD`,
/// which socket B never carried, reaches the client as sent. No other account's or
/// configured value is a secret of either socket.
#[tokio::test]
async fn each_socket_guards_the_values_its_handshake_carried() {
    const DONE: &str = r#"{"type":"response.completed","response":{"id":"resp_real","status":"completed","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    let notice =
        |text: &str| format!(r#"{{"type":"error","status":200,"error":{{"code":"x","message":"echo {text}"}}}}"#);
    let (old_a, old_b, new_b) = (notice("key-OLD"), notice("key-OLD"), notice("key-NEW"));
    let upstream = CaptureUpstream::start(Script {
        ws: vec![
            vec![Bytes::from(old_a.clone()), Bytes::from(DONE)],
            vec![Bytes::from(old_a.clone()), Bytes::from(DONE)],
            vec![
                Bytes::from(old_b.clone()),
                Bytes::from(new_b.clone()),
                Bytes::from(DONE),
            ],
        ],
        ws_close: vec![1],
        ..Script::default()
    })
    .await;
    let auth_dir = AuthDir::new();
    let cfg = Config::parse(&format!(
        "auth-dir: {}\naccess:\n  api-keys: [client-key]\ncodex-api-key:\n  - api-key: key-OLD\n    base-url: {}\n    websockets: true\n",
        auth_dir.0.display(),
        upstream.base_url
    ))
    .unwrap();
    let all = cpa_core::config::credentials::from_config(&cfg);
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        meta: Default::default(),
        xai: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(cfg, all, executors));
    let proxy = Proxy {
        addr: serve(cpa_server::router(rt.clone())).await,
        upstream,
        rt,
        _auth_dir: auth_dir,
    };
    let mut socket = connect(&proxy).await;
    assert_eq!(first_turn(&mut socket).await, [DONE], "socket A, its own key withheld");
    for credential in proxy.rt.store().snapshot() {
        let mut changed = Credential::clone(&credential);
        changed.attributes.insert("api_key".into(), "key-NEW".into());
        proxy
            .rt
            .store()
            .replace_config_backed(changed, credential.revision)
            .unwrap();
    }
    assert_eq!(
        first_turn(&mut socket).await,
        [DONE],
        "socket A reused: its own key withheld"
    );
    assert!(
        read(&mut socket, &[]).await.closed.is_some(),
        "socket A closed upstream"
    );
    let mut socket = connect(&proxy).await;
    assert_eq!(
        first_turn(&mut socket).await,
        [old_b.as_str(), DONE],
        "socket B: the old key is no value it carried; the new one is withheld"
    );
    let sockets = proxy.upstream.sockets();
    assert_eq!(sockets.len(), 2);
    let auth = |i: usize| sockets[i].headers["authorization"].to_str().unwrap().to_owned();
    assert_eq!(
        (auth(0), auth(1)),
        ("Bearer key-OLD".to_owned(), "Bearer key-NEW".to_owned())
    );
    assert!(
        sockets
            .iter()
            .all(|s| !format!("{:?}", s.headers).contains("client-key"))
    );
}

/// A non-direct upgrade rejection still supplies headers to the subscription-limit
/// formatter. Only the header carrying the serving request's credential is withheld.
#[tokio::test]
async fn subscription_limit_headers_cannot_echo_the_selected_credential() {
    use cpa_server::testing::codex_capture::Reply;
    for value in ["at-FAKE", "at-OTHER"] {
        let mut rejected = Reply::json(429, r#"{"error":{"type":"usage_limit_reached","message":"limited"}}"#);
        rejected.headers.push(("x-codex-active-limit".into(), value.into()));
        let proxy = proxy(
            Profile::Chatgpt,
            Script {
                ws_reject: Some(rejected),
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
        let got = read(&mut socket, &[]).await;
        let sockets = proxy.upstream.sockets();
        assert!(!sockets.is_empty());
        assert!(sockets.iter().all(|s| s.headers["authorization"] == "Bearer at-FAKE"));
        assert!(got.closed.is_some(), "{value}: {got:?}");
        if value == "at-FAKE" {
            assert!(
                got.frames.is_empty(),
                "confidential header must not become a limit event: {got:?}"
            );
        } else {
            assert_eq!(
                got.frames.len(),
                1,
                "safe control keeps its subscription-limit event: {got:?}"
            );
            let event: serde_json::Value = serde_json::from_str(&got.frames[0]).unwrap();
            assert_eq!(event["status"], 429);
            assert_eq!(event["headers"]["x-codex-active-limit"], value);
        }
    }
}

/// An account failure the official Codex client would get as a subscription-limit event,
/// with `{T}` in a member no reading of the failure keeps (`echo`, and the same value
/// nested). Its classification and the limit event's allowlisted fields come from
/// `error` alone.
const ECHOED_LIMITS: [(&str, &str); 3] = [
    (
        "ignored member",
        r#"{"type":"error","status":429,"error":{"type":"usage_limit_reached","message":"The usage limit has been reached.","resets_at":2000000000},"echo":"{T}"}"#,
    ),
    (
        "escaped ignored member",
        r#"{"type":"error","status":429,"error":{"type":"usage_limit_reached","message":"The usage limit has been reached.","resets_at":2000000000},"echo":"{E}"}"#,
    ),
    (
        "nested ignored member",
        r#"{"type":"error","status":429,"echo":{"headers":{"authorization":"Bearer {T}"}},"error":{"type":"usage_limit_reached","message":"The usage limit has been reached.","resets_at":2000000000}}"#,
    ),
];

/// Whether a client frame is the proxy's subscription-limit event for [`ECHOED_LIMITS`].
fn limit_event(frame: &str) -> bool {
    frame.contains("usage_limit_reached") && frame.contains("2000000000") && !frame.contains("echo")
}

/// #25 criterion 7 against #6 (hosted review on PR #63, `codex_ws.rs` original line
/// 1545): an account failure is withheld from a native WebSocket client, and the official
/// Codex client gets the allowlisted subscription-limit event instead. When the original
/// frame carries the credential the serving socket or request sent, in a member no
/// reading keeps, the failure is Confidential: no limit event, a silent close. On every
/// native path (ordinary socket, buffering, a steering connection's first response, a
/// turn over HTTP and its buffering), first and after content. Each case runs against
/// [`LOOKALIKE`], a value no request carried, which keeps its limit event; the two differ
/// only in that event: the same record (429, no tokens), cooldowns and attempts. Every
/// case runs before the verdict, which names the cases that delivered a derived event.
#[tokio::test]
async fn withheld_account_failures_check_their_original_frame_for_the_credential() {
    const DELTA: &str = r#"{"type":"response.output_text.delta","response_id":"r1","delta":"hello"}"#;
    let (mut cases, mut derived) = (0usize, Vec::new());
    for path in NATIVE_PATHS {
        for after in [false, true] {
            let prefix: Vec<String> = if after { vec![DELTA.to_owned()] } else { vec![] };
            for (name, frame) in ECHOED_LIMITS {
                let case = format!("{} after={after} {name}", path.0);
                let run = |token: &str| {
                    let mut frames = prefix.clone();
                    frames.push(with_token(frame, token));
                    async move {
                        let until = frames.last().cloned().unwrap_or_default();
                        let frames: Vec<&str> = frames.iter().map(String::as_str).collect();
                        native_turn(path, &frames, &until).await
                    }
                };
                let got = run(SELECTED).await;
                let want = run(LOOKALIKE).await;
                assert_eq!(got.authorization, [format!("Bearer {SELECTED}")], "{case}");
                // The control: the limit event, built from `error` alone, after the prefix.
                let control_limits = want.frames.iter().filter(|f| limit_event(f)).count();
                assert_eq!(control_limits, 1, "{case}: the control keeps its limit event");
                assert_eq!(want.frames[..prefix.len()], prefix[..], "{case}: control prefix");
                assert!(want.closed.is_some(), "{case}: control closes");
                // The selected credential: the prefix alone, then a silent close.
                let leaked = got.frames.iter().filter(|f| carries_token(f, SELECTED)).count();
                assert_eq!(leaked, 0, "{case}: no frame carries the selected credential");
                let limits = got.frames.iter().filter(|f| limit_event(f)).count();
                cases += 1;
                if limits != 0 || got.frames != prefix {
                    derived.push(case.clone());
                }
                assert!(got.closed.is_some(), "{case}: the turn ends");
                // Delivery alone differs.
                assert_eq!(got.records[0]["failed"], true, "{case}");
                assert_eq!(got.records[0]["fail"]["status_code"], 429, "{case}");
                assert_eq!(
                    got.records[0]["fail"]["status_code"], want.records[0]["fail"]["status_code"],
                    "{case}"
                );
                assert_eq!(tokens(&got.records[0]), tokens(&want.records[0]), "{case}");
                assert_eq!((got.cooldowns, got.attempts), (want.cooldowns, want.attempts), "{case}");
                let kept = got.records.iter().any(|r| carries_token(&r.to_string(), SELECTED));
                assert!(!kept, "{case}: no record keeps the selected credential");
            }
        }
    }
    assert_eq!(cases, 30);
    assert!(
        derived.is_empty(),
        "{} of {cases} cases delivered more than the prefix (a derived limit event): {derived:#?}",
        derived.len()
    );
}

/// [`withheld_account_failures_check_their_original_frame_for_the_credential`] on a
/// later response of a steering connection: the first response completes, and the next
/// create is answered with the account failure. The connection ends; with the selected
/// credential in an ignored member it ends silently, while the [`LOOKALIKE`] control gets
/// the limit event. Both fail the create's record with 429 and cool alike. Every case runs
/// before the verdict.
#[tokio::test]
async fn later_steering_account_failures_check_their_original_frame_for_the_credential() {
    const CREATED: &str = r#"{"type":"response.created","response":{"id":"steer_first","output":[]}}"#;
    const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"steer_first","output":[],"usage":{"input_tokens":3,"output_tokens":0,"total_tokens":3}}}"#;
    const NEXT_CREATED: &str = r#"{"type":"response.created","response":{"id":"steer_second","output":[]}}"#;
    let create = client_frame("ws-create-first", "gpt-5.5");
    let next = r#"{"type":"response.create","model":"gpt-5.5","previous_response_id":"steer_first","input":[]}"#;
    let mut derived = Vec::new();
    for started in [false, true] {
        for (name, frame) in ECHOED_LIMITS {
            let mut outcomes = Vec::new();
            for token in [SELECTED, LOOKALIKE] {
                let case = format!("started={started} {name} selected={}", token == SELECTED);
                let rejection = with_token(frame, token);
                let second = if started {
                    vec![NEXT_CREATED.to_owned(), rejection]
                } else {
                    vec![rejection]
                };
                let mut upstream =
                    Gated::start(vec![vec![CREATED.to_owned(), COMPLETED.to_owned()], second], false).await;
                let proxy = proxy_with(Script::default(), STEERING, |_| {
                    vec![credential(Profile::Chatgpt, &upstream.base_url, true)]
                })
                .await;
                proxy.rt.usage_queue().configure(
                    true,
                    &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
                );
                let mut socket = connect(&proxy).await;
                socket.send(Message::text(create.clone())).await.unwrap();
                upstream.read(1).await;
                upstream.release.notify_one();
                let first = read(&mut socket, &TERMINAL).await;
                assert_eq!(first.frames.last().map(String::as_str), Some(COMPLETED), "{case}");
                assert_eq!(records(&proxy, 1).await[0]["failed"], false, "{case}");
                socket.send(Message::text(next)).await.unwrap();
                upstream.read(2).await;
                let got = read(&mut socket, &[]).await;
                let leaked = got.frames.iter().filter(|f| carries_token(f, SELECTED)).count();
                assert_eq!(leaked, 0, "{case}: no frame carries the selected credential");
                assert!(got.closed.is_some(), "{case}: the connection ends");
                let limits = got.frames.iter().filter(|f| limit_event(f)).count();
                let rejected = records(&proxy, 1).await;
                assert_eq!(rejected[0]["failed"], true, "{case}");
                assert_eq!(rejected[0]["fail"]["status_code"], 429, "{case}");
                let kept = carries_token(&rejected[0].to_string(), SELECTED);
                assert!(!kept, "{case}: the record keeps no selected credential");
                let credential = proxy.rt.store().snapshot()[0].id.clone();
                let cooled = proxy.rt.store().cooldowns(&credential).len();
                outcomes.push((limits, cooled));
            }
            let case = format!("started={started} {name}");
            assert_eq!(outcomes[1].0, 1, "{case}: the control keeps its limit event");
            assert_eq!(outcomes[0].1, outcomes[1].1, "{case}: the same cooldowns");
            if outcomes[0].0 != 0 {
                derived.push(case);
            }
        }
    }
    assert!(
        derived.is_empty(),
        "{} of 6 cases derived a limit event from a frame carrying the credential: {derived:#?}",
        derived.len()
    );
}

/// The original-frame check is bound to the socket that read the frame, not to the
/// credential as configured now. A config-backed key serves a turn on socket A
/// (`key-OLD`); the key then changes in memory to `key-NEW`, which socket A never
/// carried. The next turn reuses socket A and is answered with the account failure
/// quoting one of the two in an ignored member: `key-OLD` makes it Confidential (silent
/// close), while `key-NEW` keeps the official client's limit event. Every case runs
/// before the verdict.
#[tokio::test]
async fn withheld_account_failures_use_the_serving_sockets_values() {
    const DONE: &str = r#"{"type":"response.completed","response":{"id":"resp_real","status":"completed","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    let mut wrong = Vec::new();
    for (name, frame) in ECHOED_LIMITS {
        let mut limits = Vec::new();
        for echoed in ["key-OLD", "key-NEW"] {
            let case = format!("{name} echo={echoed}");
            let rejection = with_token(frame, echoed);
            let proxy = proxy_with(
                Script {
                    ws: vec![vec![Bytes::from(DONE)], vec![Bytes::from(rejection)]],
                    ..Script::default()
                },
                "",
                |url| {
                    let cfg = Config::parse(&format!(
                        "codex-api-key:\n  - api-key: key-OLD\n    base-url: {url}\n    websockets: true\n"
                    ))
                    .unwrap();
                    cpa_core::config::credentials::from_config(&cfg)
                },
            )
            .await;
            let mut socket = connect(&proxy).await;
            assert_eq!(first_turn(&mut socket).await, [DONE], "{case}");
            for credential in proxy.rt.store().snapshot() {
                let mut changed = Credential::clone(&credential);
                changed.attributes.insert("api_key".into(), "key-NEW".into());
                proxy
                    .rt
                    .store()
                    .replace_config_backed(changed, credential.revision)
                    .unwrap();
            }
            socket
                .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
                .await
                .unwrap();
            let got = read(&mut socket, &[]).await;
            assert!(got.closed.is_some(), "{case}: the turn ends");
            let leaked = got.frames.iter().filter(|f| carries_token(f, "key-OLD")).count();
            assert_eq!(leaked, 0, "{case}: no frame carries the sent key");
            limits.push(got.frames.iter().filter(|f| limit_event(f)).count());
            let sockets = proxy.upstream.sockets();
            assert_eq!(sockets.len(), 1, "{case}: socket A served both turns");
            assert_eq!(sockets[0].headers["authorization"], "Bearer key-OLD", "{case}");
        }
        if limits != [0, 1] {
            wrong.push(format!("{name}: limit events {limits:?} for [key-OLD, key-NEW]"));
        }
    }
    assert!(
        wrong.is_empty(),
        "only the sent key makes the failure Confidential: {wrong:#?}"
    );
}

/// A reused socket's request-scoped bootstrap error quotes a new config value that
/// its handshake never carried. It must retain its as-sent delivery.
#[tokio::test]
async fn bootstrap_errors_use_only_the_serving_sockets_credential() {
    const DONE: &str =
        r#"{"type":"response.completed","response":{"id":"resp_real","status":"completed","output":[]}}"#;
    const ERROR: &str = r#"{"type":"error","status":400,"error":{"code":"invalid_prompt","message":"key-NEW is ordinary text on this socket"}}"#;
    let proxy = proxy_with(
        Script {
            ws: vec![
                vec![Bytes::from_static(DONE.as_bytes())],
                vec![Bytes::from_static(ERROR.as_bytes())],
            ],
            ..Script::default()
        },
        "codex:\n  stream-bootstrap-buffering: true\n",
        |url| {
            let cfg = Config::parse(&format!(
                "codex-api-key:\n  - api-key: key-OLD\n    base-url: {url}\n    websockets: true\n"
            ))
            .unwrap();
            cpa_core::config::credentials::from_config(&cfg)
        },
    )
    .await;
    let mut socket = connect(&proxy).await;
    assert_eq!(first_turn(&mut socket).await, [DONE]);
    for credential in proxy.rt.store().snapshot() {
        let mut changed = Credential::clone(&credential);
        changed.attributes.insert("api_key".into(), "key-NEW".into());
        proxy
            .rt
            .store()
            .replace_config_backed(changed, credential.revision)
            .unwrap();
    }
    socket
        .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
        .await
        .unwrap();
    let got = read(&mut socket, &["error"]).await;
    assert_eq!(
        got.frames,
        [ERROR],
        "the new key was never sent on this socket: {got:?}"
    );
    let sockets = proxy.upstream.sockets();
    assert_eq!(sockets.len(), 1);
    assert_eq!(sockets[0].headers["authorization"], "Bearer key-OLD");
}

/// The middle body.error is account-bearing but contains no selected token: the
/// ambiguity policy must withhold it independently of the credential byte guard.
#[tokio::test]
async fn ambiguous_body_errors_do_not_expose_a_pooled_account() {
    const END: &str = r#"{"type":"response.completed","response":{"id":"resp_real","status":"completed","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    const ACCOUNT: &str = r#"{"type":"error","type":"error","status":400,"body":{"error":{"type":"invalid_request_error"}},"body":{"error":{"type":"usage_limit_reached","message":"acct-PRIVATE"}},"body":{"error":{"type":"invalid_request_error"}}}"#;
    const SAFE: &str = r#"{"type":"error","type":"error","status":400,"body":{"error":{"type":"invalid_request_error"}},"body":{"error":{"type":"invalid_request_error","message":"safe"}},"body":{"error":{"type":"invalid_request_error"}}}"#;
    for path in NATIVE_PATHS {
        let got = native_turn(path, &[ACCOUNT, END], END).await;
        let control = native_turn(path, &[SAFE, END], END).await;
        assert!(
            !got.frames.iter().any(|f| f.contains("acct-PRIVATE")),
            "{}: {got:?}",
            path.0
        );
        assert_eq!(got.frames, [END], "{}: the rejected event is skipped", path.0);
        assert!(control.frames.iter().any(|f| f == SAFE), "{}: {control:?}", path.0);
        assert_eq!(tokens(&got.records[0]), [4, 1, 5], "{}: {got:?}", path.0);
        assert_eq!(
            (got.cooldowns, got.attempts),
            (control.cooldowns, control.attempts),
            "{}",
            path.0
        );
    }
}

#[tokio::test]
async fn quoted_cookie_values_are_guarded_but_empty_cookies_preserve_events() {
    const END: &str = r#"{"type":"response.completed","response":{"id":"real","status":"completed","output":[]}}"#;
    for (cookie, event, withheld) in [
        (
            r#"sid="cookie-QUOTED""#,
            r#"{"type":"response.future","echo":"echo cookie-QUOTED"}"#,
            true,
        ),
        (r#"sid="""#, r#"{"type":"response.future","empty":""}"#, false),
    ] {
        for (config, websockets) in [("", true), (STEERING, true), ("", false)] {
            use cpa_server::testing::codex_capture::Reply;
            let proxy = proxy_with(
                Script {
                    ws: vec![vec![
                        Bytes::copy_from_slice(event.as_bytes()),
                        Bytes::from_static(END.as_bytes()),
                    ]],
                    http: vec![Reply::sse(format!("data: {event}\n\ndata: {END}\n\n"))],
                    ..Script::default()
                },
                config,
                |url| {
                    let mut c = credential(Profile::Chatgpt, url, websockets);
                    c.attributes.insert("header:cookie".into(), cookie.into());
                    vec![c]
                },
            )
            .await;
            let mut socket = connect(&proxy).await;
            let got = first_turn(&mut socket).await;
            let expected = if withheld { vec![END] } else { vec![event, END] };
            assert_eq!(got, expected, "{cookie}, websockets={websockets}, {config}");
            let headers = if websockets {
                proxy.upstream.sockets()[0].headers.clone()
            } else {
                proxy.upstream.http()[0].headers.clone()
            };
            assert_eq!(headers["cookie"], cookie);
            assert_eq!(headers["authorization"], "Bearer at-FAKE");
        }
    }
}

// #25 privacy is processing-neutral (`docs/codex/CONTRACT.md` "Credential isolation"),
// also when the guard leaves out every frame a turn would deliver, and the selected
// credential reaches no request log or usage record. The second account's token is
// `at-SECOND`; generated frames, loopback upstreams and fake tokens only.

/// The token [`native_turn_on`] gives its second account.
const SECOND: &str = "at-SECOND";
/// What the upstream answers a second attempt: the client and the records see it only
/// after a failover.
const SECOND_END: &str = r#"{"type":"response.completed","response":{"id":"resp_second","status":"completed","output":[],"usage":{"input_tokens":9,"output_tokens":9,"total_tokens":18}}}"#;

/// What processing decided for each record: failed, status, tokens, whether usage was
/// observed, and the account.
fn outcomes(records: &[serde_json::Value]) -> Vec<String> {
    records
        .iter()
        .map(|r| {
            format!(
                "{} {} {:?} {} {}",
                r["failed"],
                r["fail"]["status_code"],
                tokens(r),
                r["usage_observed"],
                r["auth_index"]
            )
        })
        .collect()
}

/// One turn with two accounts available, whose upstream sends `frames` with `token` for
/// `{T}`, and the second account's completion to a second attempt.
async fn served(path: (&str, &str, bool), frames: &[&str], token: &str, ws_close: &[usize]) -> Turned {
    let frames: Vec<String> = frames.iter().map(|f| with_token(f, token)).collect();
    let frames: Vec<&str> = frames.iter().map(String::as_str).collect();
    let until = frames.last().copied().unwrap_or_default();
    native_turn_on(
        path,
        &[&frames, &[SECOND_END]],
        until,
        &[SELECTED, SECOND],
        ws_close,
        false,
    )
    .await
}

/// #25: a frame the credential guard leaves out was served. The executor processed it, and
/// the guard drops only its delivery, so the stream commits on it as on a delivered frame.
/// A turn whose every frame is left out, its terminal event included, keeps the outcome
/// processing gave it on every native path: one execution, on the selected account; one
/// successful record with the usage the upstream reported (none when it reported none);
/// no cooldown; and no second execution on the other account available. A failure or a
/// truncation after a left-out prefix stays a failure, an account failure (401, 429)
/// included: on a steering connection the duplex leaves such a prefix out, and it must
/// still count as served, or the failure would run the request again on the other account.
/// No record keeps the token: not the model or the service tier the terminal reported.
/// Each case runs against its `at-OTHER` control: the two differ only in the frames the
/// client receives.
#[tokio::test]
async fn withheld_frames_keep_the_served_outcome_without_a_replay() {
    const CREATED: &str = r#"{"type":"response.created","response":{"id":"resp_real","note":"{T}"}}"#;
    const DELTA: &str = r#"{"type":"response.output_text.delta","response_id":"resp_real","delta":"{T}"}"#;
    const SAFE_CREATED: &str = r#"{"type":"response.created","response":{"id":"resp_real"}}"#;
    const SAFE_DELTA: &str = r#"{"type":"response.output_text.delta","response_id":"resp_real","delta":"hello"}"#;
    const END: &str = r#"{"type":"response.completed","response":{"id":"resp_real","model":"gpt-5.5 {T}","service_tier":"echo {T}","status":"completed","output":[],"note":"{T}","usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    const ESCAPED_END: &str = r#"{"type":"response.completed","response":{"id":"resp_real","model":"gpt-5.5 {E}","service_tier":"echo {E}","status":"completed","output":[],"note":"{E}","usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    const NO_USAGE_END: &str =
        r#"{"type":"response.completed","response":{"id":"resp_real","status":"completed","output":[],"note":"{T}"}}"#;
    const FAILED: &str =
        r#"{"type":"response.failed","response":{"id":"resp_real","error":{"code":"x","message":"retry later"}}}"#;
    // Account failures that carry no token: only the prefix before them is withheld.
    const LIMITED: &str =
        r#"{"type":"error","status":429,"error":{"code":"usage_limit_reached","message":"safe-account-error"}}"#;
    const UNAUTHORIZED: &str =
        r#"{"type":"error","status":401,"error":{"code":"invalid_api_key","message":"safe-account-error"}}"#;
    // A name, the upstream frames, the client frames after which the socket closes, for a
    // served terminal the usage its one successful record keeps, and whether one
    // execution serves it on every path. A failure or truncation right after a frame that
    // bootstrap buffering still holds served nothing yet, so it may fail over, as its
    // control does.
    type Case<'a> = (&'a str, &'a [&'a str], &'a [usize], Option<[i64; 3]>, bool);
    let cases: [Case; 9] = [
        ("sole terminal", &[END], &[], Some([4, 1, 5]), true),
        ("sole escaped terminal", &[ESCAPED_END], &[], Some([4, 1, 5]), true),
        (
            "sole terminal without usage",
            &[NO_USAGE_END],
            &[],
            Some([0, 0, 0]),
            true,
        ),
        (
            "withheld prefix and terminal",
            &[CREATED, DELTA, END],
            &[],
            Some([4, 1, 5]),
            true,
        ),
        (
            "safe prefix, withheld terminal",
            &[SAFE_CREATED, SAFE_DELTA, END],
            &[],
            Some([4, 1, 5]),
            true,
        ),
        ("withheld prefix, then a failure", &[CREATED, FAILED], &[], None, false),
        ("withheld prefix, then a truncation", &[CREATED], &[0], None, false),
        (
            "withheld prefix, then a 429",
            &[CREATED, DELTA, LIMITED],
            &[],
            None,
            true,
        ),
        (
            "withheld prefix, then a 401",
            &[CREATED, DELTA, UNAUTHORIZED],
            &[],
            None,
            true,
        ),
    ];
    // Whether a control's record kept the model and the tier a withheld terminal reported.
    let (mut model_kept, mut tier_kept) = (false, false);
    for path in NATIVE_PATHS {
        for (name, frames, ws_close, served_usage, once) in cases {
            let case = format!("{} {name}", path.0);
            let got = served(path, frames, SELECTED, ws_close).await;
            let want = served(path, frames, LOOKALIKE, ws_close).await;
            // Each execution carries its own account's token, and there are as many as the
            // control's.
            assert_eq!(got.authorization, want.authorization, "{case}: {got:?}");
            if once {
                assert_eq!(
                    got.authorization,
                    [format!("Bearer {SELECTED}")],
                    "{case}: one execution, no replay"
                );
            }
            assert!(
                !got.frames.iter().any(|f| carries_token(f, SELECTED)),
                "{case}: {got:?}"
            );
            let delivered: Vec<String> = want
                .frames
                .iter()
                .filter(|f| !f.contains(LOOKALIKE) && !carries_token(f, LOOKALIKE))
                .cloned()
                .collect();
            assert_eq!(got.frames, delivered, "{case}: only the withheld frames are missing");
            assert_eq!(outcomes(&got.records), outcomes(&want.records), "{case}: {got:?}");
            assert_eq!(got.cooldowns, want.cooldowns, "{case}");
            // The records keep no spelling of the token, and otherwise the control's
            // upstream text (the model the terminal reported).
            for (got, want) in got.records.iter().zip(&want.records) {
                assert!(!carries_token(&got.to_string(), SELECTED), "{case}: {got}");
                let model = want["response_model"]
                    .as_str()
                    .map(|m| m.replace(LOOKALIKE, "[REDACTED]"));
                assert_eq!(got["response_model"].as_str(), model.as_deref(), "{case}");
                model_kept |= model.is_some_and(|m| m.contains("[REDACTED]"));
                let tier = want["response_service_tier"]
                    .as_str()
                    .map(|t| t.replace(LOOKALIKE, "[REDACTED]"));
                assert_eq!(got["response_service_tier"].as_str(), tier.as_deref(), "{case}");
                tier_kept |= tier.is_some_and(|t| t.contains("[REDACTED]"));
            }
            if let Some(usage) = served_usage {
                assert_eq!(got.authorization, [format!("Bearer {SELECTED}")], "{case}: no replay");
                assert!(!got.frames.iter().any(|f| f == SECOND_END), "{case}: {got:?}");
                assert_eq!(got.records.len(), 1, "{case}: {:?}", got.records);
                assert_eq!(got.records[0]["failed"], false, "{case}: {:?}", got.records);
                assert_eq!(tokens(&got.records[0]), usage, "{case}");
                assert_eq!(got.cooldowns, 0, "{case}");
            }
        }
    }
    assert!(model_kept, "a record keeps the reported model, scrubbed");
    assert!(tier_kept, "a record keeps the reported service tier, scrubbed");
}

/// #25 and AGENTS.md: an upstream credential is never logged. The request log and the
/// usage record are operator-side, and they keep what the upstream answered: an upstream
/// that quotes the selected credential (`Bearer at-FAKE`, sent upstream; the client's key
/// is `client-key`) gets it scrubbed from the log file (its spooled sections included)
/// and from the queued failure record, as written or escaped, on every native path,
/// before and after output. The rest of each entry stays: the record's status, tokens
/// and account, and the log's events, with `[REDACTED]` where the value was. Delivery is
/// unchanged. The `at-OTHER` controls keep every byte in both.
#[tokio::test]
async fn logs_and_usage_records_never_keep_the_selected_credential() {
    const DELTA: &str = r#"{"type":"response.output_text.delta","response_id":"r1","delta":"hello"}"#;
    const REAL_END: &str = r#"{"type":"response.completed","response":{"id":"resp_real","status":"completed","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    let failure = r#"{"type":"response.failed","response":{"id":"r1","error":{"type":"invalid_request_error","code":"bad_input","message":"quoted {T}"}}}"#;
    let escaped = failure.replace("{T}", "{E}");
    let notice = r#"{"type":"response.future","note":"{T}","n":1.50}"#;
    // Over the 64 KiB a log section keeps in memory: its bytes go through a spool file.
    let spooled = failure.replace("{T}", &format!("{{T}} {}", "x".repeat(70_000)));
    let mut cases: Vec<(&str, Vec<&str>, bool)> = vec![
        ("failure", vec![failure], true),
        ("escaped failure", vec![&escaped], false),
        ("failure after output", vec![DELTA, failure], true),
        ("skipped notice", vec![notice, REAL_END], true),
    ];
    for path in NATIVE_PATHS {
        if path.0 == "socket" {
            cases.push(("spooled failure", vec![&spooled], true));
        }
        for (name, frames, literal) in &cases {
            let case = format!("{} {name}", path.0);
            let run = |token: &str| {
                let frames: Vec<String> = frames.iter().map(|f| with_token(f, token)).collect();
                async move {
                    let frames: Vec<&str> = frames.iter().map(String::as_str).collect();
                    native_turn_on(path, &[&frames], REAL_END, &[SELECTED], &[], true).await
                }
            };
            let got = run(SELECTED).await;
            let want = run(LOOKALIKE).await;
            assert_eq!(got.authorization, [format!("Bearer {SELECTED}")], "{case}");
            // The log: no spelling of the token, the event still there.
            assert!(!carries_token(&got.log, SELECTED), "{case}: {}", got.log);
            assert!(!got.log.contains("client-key"), "{case}: {}", got.log);
            assert!(got.log.contains("[REDACTED"), "{case}: {}", got.log);
            assert!(
                carries_token(&want.log, LOOKALIKE),
                "{case}: the control keeps its bytes"
            );
            assert!(!want.log.contains("[REDACTED"), "{case}: nothing else is scrubbed");
            // The record: the control's, the token scrubbed.
            assert_eq!(outcomes(&got.records), outcomes(&want.records), "{case}");
            assert_eq!(got.cooldowns, want.cooldowns, "{case}");
            for (got, want) in got.records.iter().zip(&want.records) {
                let body = got["fail"]["body"].as_str().unwrap_or_default();
                let control = want["fail"]["body"].as_str().unwrap_or_default();
                assert!(!carries_token(body, SELECTED), "{case}: {body}");
                if *literal {
                    assert_eq!(body, control.replace(LOOKALIKE, "[REDACTED]"), "{case}");
                } else if !control.is_empty() {
                    assert!(body.starts_with("[REDACTED"), "{case}: {body}");
                }
            }
        }
        cases.retain(|(name, ..)| *name != "spooled failure");
    }
}

/// The log and the record follow the values each socket's own handshake carried, as the
/// delivery guard does. A config-backed API key serves a turn on socket A (`key-OLD`); the
/// key then changes in memory. The next turn reuses socket A, whose upstream only knows
/// `key-OLD`: its failure quoting both keys is logged and recorded with `key-OLD`
/// scrubbed and `key-NEW`, never sent on that socket, kept. Socket A closes; a new
/// connection dials socket B with `key-NEW`, and the same failure keeps `key-OLD` and
/// loses `key-NEW`.
#[tokio::test]
async fn logs_follow_the_values_each_socket_carried() {
    const DONE: &str = r#"{"type":"response.completed","response":{"id":"resp_real","status":"completed","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    const FAILED: &str = r#"{"type":"response.failed","response":{"id":"r1","error":{"type":"invalid_request_error","code":"bad_input","message":"quoted key-OLD and key-NEW"}}}"#;
    let mut proxy = proxy_with(
        Script {
            ws: vec![
                vec![Bytes::from_static(DONE.as_bytes())],
                vec![Bytes::from_static(FAILED.as_bytes())],
                vec![Bytes::from_static(FAILED.as_bytes())],
            ],
            ws_close: vec![1],
            ..Script::default()
        },
        "request-log: true\n",
        |url| {
            let cfg = Config::parse(&format!(
                "codex-api-key:\n  - api-key: key-OLD\n    base-url: {url}\n    websockets: true\n"
            ))
            .unwrap();
            cpa_core::config::credentials::from_config(&cfg)
        },
    )
    .await;
    let (dir, _management) = log_requests(&mut proxy);
    proxy.rt.usage_queue().configure(
        true,
        &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
    );
    let mut socket = connect(&proxy).await;
    assert_eq!(first_turn(&mut socket).await, [DONE]);
    for credential in proxy.rt.store().snapshot() {
        let mut changed = Credential::clone(&credential);
        changed.attributes.insert("api_key".into(), "key-NEW".into());
        proxy
            .rt
            .store()
            .replace_config_backed(changed, credential.revision)
            .unwrap();
    }
    assert!(first_turn(&mut socket).await.is_empty(), "socket A's own key: withheld");
    drop(socket);
    let first = finished_logs(&dir, 1).await.remove(0);
    let mut socket = connect(&proxy).await;
    assert!(first_turn(&mut socket).await.is_empty(), "socket B's own key: withheld");
    drop(socket);
    let second = finished_logs(&dir, 2).await.remove(1);
    let records = records(&proxy, 3).await;
    let sockets = proxy.upstream.sockets();
    let auth = |i: usize| sockets[i].headers["authorization"].to_str().unwrap().to_owned();
    assert_eq!(
        (sockets.len(), auth(0), auth(1)),
        (2, "Bearer key-OLD".to_owned(), "Bearer key-NEW".to_owned())
    );
    assert!(!first.contains("key-OLD"), "{first}");
    assert!(first.contains("quoted [REDACTED] and key-NEW"), "{first}");
    assert!(!second.contains("key-NEW"), "{second}");
    assert!(second.contains("quoted key-OLD and [REDACTED]"), "{second}");
    let bodies: Vec<&str> = records
        .iter()
        .map(|r| r["fail"]["body"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(records[0]["failed"], false);
    assert!(bodies[1].contains("quoted [REDACTED] and key-NEW"), "{bodies:?}");
    assert!(bodies[2].contains("quoted key-OLD and [REDACTED]"), "{bodies:?}");
}

/// A lost upstream socket whose close reason quotes the selected credential: the log keeps
/// nothing of it. Before output the turn's failure is logged, and the control's reason
/// shows it; an idle socket's loss after a completed turn writes nothing to the log (Go's
/// disconnect notifier), for the control too.
#[tokio::test]
async fn a_lost_socket_logs_no_credential() {
    use axum::extract::ws::{CloseFrame, Message as Upstream, WebSocketUpgrade};
    const CREATED: &str = r#"{"type":"response.created","response":{"id":"r1"}}"#;
    const DONE: &str = r#"{"type":"response.completed","response":{"id":"r1","status":"completed","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    for after in [false, true] {
        let mut logs = Vec::new();
        for token in [SELECTED, LOOKALIKE] {
            let reason = format!("reason {token}");
            let app = axum::Router::new().route(
                "/responses",
                axum::routing::get(move |upgrade: WebSocketUpgrade| async move {
                    upgrade.on_upgrade(move |mut socket| async move {
                        let _ = socket.recv().await;
                        if after {
                            let _ = socket.send(Upstream::Text(CREATED.into())).await;
                            let _ = socket.send(Upstream::Text(DONE.into())).await;
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                        let _ = socket
                            .send(Upstream::Close(Some(CloseFrame {
                                code: 1011,
                                reason: reason.into(),
                            })))
                            .await;
                    })
                }),
            );
            let upstream = format!("http://{}", serve(app).await);
            let mut proxy = proxy_with(Script::default(), "request-log: true\n", |_| {
                vec![credential(Profile::Chatgpt, &upstream, true)]
            })
            .await;
            let (dir, _management) = log_requests(&mut proxy);
            let mut socket = connect(&proxy).await;
            socket
                .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
                .await
                .unwrap();
            if after {
                assert_eq!(read(&mut socket, &TERMINAL).await.frames, [CREATED, DONE]);
            }
            let got = read(&mut socket, &[]).await;
            assert!(got.closed.is_some(), "{after} {token}: {got:?}");
            assert!(!got.frames.iter().any(|f| carries_token(f, SELECTED)), "{got:?}");
            drop(socket);
            logs.push(finished_logs(&dir, 1).await.remove(0));
        }
        assert!(!carries_token(&logs[0], SELECTED), "after={after}: {}", logs[0]);
        assert_eq!(
            logs[1].contains("reason at-OTHER"),
            !after,
            "after={after}: {}",
            logs[1]
        );
        if !after {
            assert!(logs[0].contains("reason [REDACTED]"), "{}", logs[0]);
        }
    }
}

/// Every record of one connection is scrubbed of the values its socket carried, published
/// before or after the others (#25, AGENTS.md). A steering connection's socket sent
/// `Bearer at-FAKE`, and its first response publishes its record. Then a later response
/// on the same socket, an explicit create or an automatic successor, reports a model or a
/// failure quoting the token, written as sent or escaped. Its record keeps the outcome
/// (9/2/11 tokens, or a 400 without tokens), its own execution ID and the connection's
/// account identity, and no spelling of the token; the request log and the client get
/// none either. The `at-OTHER` controls keep every byte.
#[tokio::test]
async fn later_responses_keep_the_socket_values_after_the_first_publishes() {
    const CREATED: &str = r#"{"type":"response.created","response":{"id":"real"}}"#;
    const DONE: &str = r#"{"type":"response.completed","response":{"id":"real","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    const NEXT_CREATED: &str = r#"{"type":"response.created","response":{"id":"next","previous_response_id":"real"}}"#;
    const MODEL: &str = r#"{"type":"response.completed","response":{"id":"next","model":"served {T}","output":[],"usage":{"input_tokens":9,"output_tokens":2,"total_tokens":11}}}"#;
    const FAILURE: &str = r#"{"type":"response.failed","response":{"id":"next","error":{"type":"invalid_request_error","code":"bad_input","message":"benign diagnostic {T}"}}}"#;
    for automatic in [false, true] {
        for (kind, terminal) in [
            ("model", MODEL),
            ("escaped model", &*MODEL.replace("{T}", "{E}")),
            ("failure", FAILURE),
            ("escaped failure", &*FAILURE.replace("{T}", "{E}")),
        ] {
            for token in [SELECTED, LOOKALIKE] {
                let case = format!("automatic={automatic} {kind} {token}");
                let terminal = with_token(terminal, token);
                let failed = kind.contains("failure");
                let bytes = |frames: &[&str]| frames.iter().map(|f| Bytes::from(f.to_string())).collect::<Vec<_>>();
                let ws = if automatic {
                    vec![bytes(&[CREATED, DONE, NEXT_CREATED, &terminal])]
                } else {
                    vec![bytes(&[CREATED, DONE]), bytes(&[NEXT_CREATED, &terminal])]
                };
                let mut proxy = proxy_with(
                    Script {
                        ws,
                        ..Script::default()
                    },
                    &format!("request-log: true\n{STEERING}"),
                    |url| vec![credential(Profile::Chatgpt, url, true)],
                )
                .await;
                let (dir, _management) = log_requests(&mut proxy);
                proxy.rt.usage_queue().configure(
                    true,
                    &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
                );
                let mut socket = connect(&proxy).await;
                assert_eq!(first_turn(&mut socket).await, [CREATED, DONE], "{case}");
                if !automatic {
                    socket
                        .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
                        .await
                        .unwrap();
                }
                let later = read(&mut socket, &TERMINAL).await;
                drop(socket);
                let records = records(&proxy, 2).await;
                tokio::time::sleep(Duration::from_millis(100)).await;
                assert!(
                    proxy.rt.usage_queue().pop_oldest(10).is_empty(),
                    "{case}: one record each"
                );
                let log = finished_logs(&dir, 1).await.remove(0);
                let sockets = proxy.upstream.sockets();
                assert_eq!(sockets.len(), 1, "{case}: one socket");
                assert_eq!(
                    sockets[0].headers["authorization"],
                    format!("Bearer {SELECTED}"),
                    "{case}"
                );
                assert_eq!(sockets[0].frames.len(), if automatic { 1 } else { 2 }, "{case}");
                let credential = proxy.rt.store().snapshot()[0].id.clone();
                assert!(proxy.rt.store().cooldowns(&credential).is_empty(), "{case}");
                let (first, next) = (&records[0], &records[1]);
                assert_ne!(first["execution_id"], next["execution_id"], "{case}");
                for field in ["auth_index", "access_token_sha256", "source", "auth_type"] {
                    assert_eq!(first[field], next[field], "{case}: {field}");
                }
                assert_eq!(tokens(first), [4, 1, 5], "{case}");
                assert_eq!(next["failed"], failed, "{case}: {next}");
                assert_eq!(tokens(next), if failed { [0, 0, 0] } else { [9, 2, 11] }, "{case}");
                if failed {
                    assert_eq!(next["fail"]["status_code"], 400, "{case}");
                }
                if token == SELECTED {
                    for record in &records {
                        assert!(!carries_token(&record.to_string(), SELECTED), "{case}: {record}");
                    }
                    assert!(!carries_token(&log, SELECTED), "{case}: {log}");
                    assert!(!later.frames.iter().any(|f| carries_token(f, SELECTED)), "{case}");
                    match kind {
                        "model" | "escaped model" => assert_eq!(next["response_model"], "served [REDACTED]", "{case}"),
                        "failure" => assert!(
                            next["fail"]["body"]
                                .as_str()
                                .unwrap()
                                .contains("benign diagnostic [REDACTED]"),
                            "{case}: {next}"
                        ),
                        _ => assert!(
                            next["fail"]["body"].as_str().unwrap().starts_with("[REDACTED: "),
                            "{case}: {next}"
                        ),
                    }
                } else {
                    assert!(
                        later.frames.contains(&terminal),
                        "{case}: the control's bytes: {later:?}"
                    );
                    assert!(log.contains(&terminal), "{case}: the control's log");
                    let kept = if failed {
                        next["fail"]["body"].as_str().unwrap().to_owned()
                    } else {
                        next["response_model"].as_str().unwrap().to_owned()
                    };
                    assert!(carries_token(&kept, LOOKALIKE), "{case}: {next}");
                }
            }
        }
    }
}

/// An initial account failure (a wrapped 401) on an ordinary upstream socket retires that
/// socket, and the session reports its loss. The turn's request was not served yet: it
/// fails over to the other account, whose dial is still in progress when the loss is
/// reported, and completes there. One failed 401 record for the first account and one
/// successful 4/1/5 record for the second, after two actual dials, with and without
/// bootstrap buffering, whether or not the failure quotes the token. A client that leaves
/// during that second dial abandons an attempt the upstream never answered: its record is
/// the canceled failure (499, no tokens), never a success.
#[tokio::test]
async fn an_initial_account_failure_fails_over_while_its_socket_loss_is_reported() {
    use axum::extract::ws::{Message as Upstream, WebSocketUpgrade};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    const CREATED: &str = r#"{"type":"response.created","response":{"id":"real"}}"#;
    const DONE: &str = r#"{"type":"response.completed","response":{"id":"real","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    for buffering in [false, true] {
        for token in [SELECTED, LOOKALIKE] {
            for leave in [false, true] {
                let case = format!("buffering={buffering} {token} leave={leave}");
                let rejected = format!(
                    r#"{{"type":"error","status":401,"error":{{"code":"invalid_api_key","message":"rejected {token}"}}}}"#
                );
                let auths = Arc::new(Mutex::new(Vec::<String>::new()));
                let dials = Arc::new(AtomicUsize::new(0));
                let app = {
                    let (auths, dials) = (auths.clone(), dials.clone());
                    axum::Router::new().route(
                        "/responses",
                        axum::routing::get(move |headers: axum::http::HeaderMap, upgrade: WebSocketUpgrade| {
                            let (auths, dials, rejected) = (auths.clone(), dials.clone(), rejected.clone());
                            async move {
                                let authorization = headers["authorization"].to_str().unwrap().to_owned();
                                auths.lock().unwrap().push(authorization);
                                let dial = dials.fetch_add(1, Ordering::SeqCst);
                                if dial > 0 {
                                    // The first socket's loss is reported meanwhile.
                                    tokio::time::sleep(Duration::from_millis(400)).await;
                                }
                                upgrade.on_upgrade(move |mut socket| async move {
                                    let _ = socket.recv().await;
                                    let replies = if dial == 0 {
                                        vec![rejected]
                                    } else {
                                        vec![CREATED.into(), DONE.into()]
                                    };
                                    for reply in replies {
                                        let _ = socket.send(Upstream::Text(reply.into())).await;
                                    }
                                    while let Some(Ok(_)) = socket.recv().await {}
                                })
                            }
                        }),
                    )
                };
                let upstream = format!("http://{}", serve(app).await);
                let config = if buffering {
                    "codex:\n  stream-bootstrap-buffering: true\n"
                } else {
                    ""
                };
                let proxy = proxy_with(Script::default(), config, |_| {
                    [SELECTED, SECOND]
                        .iter()
                        .enumerate()
                        .map(|(i, token)| {
                            let mut c = credential(Profile::Chatgpt, &upstream, true);
                            if i > 0 {
                                c.id = format!("codex-{i}.json");
                                c.metadata.insert("access_token".into(), serde_json::json!(token));
                            }
                            c.attributes.insert("priority".into(), (9 - i).to_string());
                            c
                        })
                        .collect()
                })
                .await;
                proxy.rt.usage_queue().configure(
                    true,
                    &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
                );
                let mut socket = connect(&proxy).await;
                socket
                    .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
                    .await
                    .unwrap();
                let got = if leave {
                    // Leave once the second dial is waiting.
                    tokio::time::timeout(Duration::from_secs(5), async {
                        while dials.load(Ordering::SeqCst) < 2 {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                    })
                    .await
                    .unwrap();
                    drop(socket);
                    None
                } else {
                    Some(read(&mut socket, &TERMINAL).await)
                };
                let records = records(&proxy, 2).await;
                tokio::time::sleep(Duration::from_millis(600)).await;
                assert!(proxy.rt.usage_queue().pop_oldest(10).is_empty(), "{case}: two records");
                let auths = auths.lock().unwrap().clone();
                assert_eq!(
                    auths.get(..2),
                    Some(&[format!("Bearer {SELECTED}"), format!("Bearer {SECOND}")][..]),
                    "{case}: the failover dials the second account: {auths:?}"
                );
                let (rejected, second) = (&records[0], &records[1]);
                assert_eq!(rejected["failed"], true, "{case}: {rejected}");
                assert_eq!(rejected["fail"]["status_code"], 401, "{case}");
                assert_eq!(tokens(rejected), [0, 0, 0], "{case}");
                assert!(!carries_token(&rejected.to_string(), SELECTED), "{case}: {rejected}");
                assert_ne!(
                    rejected["access_token_sha256"], second["access_token_sha256"],
                    "{case}: the second account"
                );
                match got {
                    Some(got) => {
                        assert_eq!(got.frames, [CREATED, DONE], "{case}: the failover completes: {got:?}");
                        assert_eq!(second["failed"], false, "{case}: {second}");
                        assert_eq!(tokens(second), [4, 1, 5], "{case}");
                    }
                    None => {
                        assert_eq!(second["failed"], true, "{case}: never a success: {second}");
                        assert_eq!(second["fail"]["status_code"], 499, "{case}");
                        assert_eq!(second["fail"]["body"], "context canceled", "{case}");
                        assert_eq!(tokens(second), [0, 0, 0], "{case}");
                    }
                }
            }
        }
    }
}

/// #25 R3: each upstream socket owns its loss. On an ordinary socket the turn fails over
/// from the accounts that reject it (none, a safe 429 with a subscription reset, a 401,
/// or a 401 then that 429) to the next one, whose handshake waits until the rejected
/// socket's loss was reported. The serving socket completes a response; then, idle after
/// a `previous_response_id` continuation completed, or active after the next response's
/// `response.created` and a real delta, it closes 1011 with its own bearer in the reason.
/// The client is closed promptly in both cases: the rejected socket's loss, which the
/// turn let pass while failing over, does not hide the serving socket's. An active loss
/// carries the serving socket's credential, so it is Confidential: the client gets no
/// subscription-limit event (not the rejected account's either) and no frame after the
/// delta. Dials go to each account in order; the records are each rejection, the
/// completed responses with 4/1/5 tokens, and for an active loss a failed record
/// without tokens. No bearer reaches the client.
#[tokio::test]
async fn a_replacement_socket_owns_its_loss_after_failover() {
    use axum::extract::ws::{CloseFrame, Message as Upstream, WebSocketUpgrade};
    use std::sync::Mutex;
    const TOKENS: [&str; 3] = [SELECTED, SECOND, "at-THIRD"];
    const CREATED: &str = r#"{"type":"response.created","response":{"id":"real"}}"#;
    const DONE: &str = r#"{"type":"response.completed","response":{"id":"real","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    const NEXT_CREATED: &str = r#"{"type":"response.created","response":{"id":"next"}}"#;
    const NEXT_DELTA: &str = r#"{"type":"response.output_text.delta","response_id":"next","delta":"visible"}"#;
    const NEXT_DONE: &str = r#"{"type":"response.completed","response":{"id":"next","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    const LIMITED: &str = r#"{"type":"error","status":429,"error":{"type":"usage_limit_reached","message":"The usage limit has been reached.","resets_at":2000000000}}"#;
    const UNAUTHORIZED: &str =
        r#"{"type":"error","status":401,"error":{"code":"invalid_api_key","message":"safe-account-rejection"}}"#;
    let continuation = r#"{"type":"response.create","model":"gpt-5.5","previous_response_id":"real","input":[]}"#;
    let sequences: [&[(&str, i64)]; 4] = [
        &[],
        &[(LIMITED, 429)],
        &[(UNAUTHORIZED, 401)],
        &[(UNAUTHORIZED, 401), (LIMITED, 429)],
    ];
    for buffering in [false, true] {
        for rejections in sequences {
            for active in [false, true] {
                let case = format!(
                    "buffering={buffering} rejections={:?} active={active}",
                    rejections.iter().map(|(_, s)| s).collect::<Vec<_>>()
                );
                let serving = rejections.len();
                let auths = Arc::new(Mutex::new(Vec::<String>::new()));
                let release = Arc::new(tokio::sync::Semaphore::new(0));
                let app = {
                    let (auths, release) = (auths.clone(), release.clone());
                    let rejected: Vec<String> = rejections.iter().map(|(f, _)| f.to_string()).collect();
                    axum::Router::new().route(
                        "/responses",
                        axum::routing::get(move |headers: axum::http::HeaderMap, upgrade: WebSocketUpgrade| {
                            let (auths, release, rejected) = (auths.clone(), release.clone(), rejected.clone());
                            async move {
                                let authorization = headers["authorization"].to_str().unwrap().to_owned();
                                auths.lock().unwrap().push(authorization.clone());
                                let account = TOKENS
                                    .iter()
                                    .position(|t| authorization == format!("Bearer {t}"))
                                    .unwrap();
                                if account > 0 && account == rejected.len() {
                                    // The rejected socket's loss is reported meanwhile.
                                    tokio::time::sleep(Duration::from_millis(250)).await;
                                }
                                upgrade.on_upgrade(move |mut socket| async move {
                                    let _ = socket.recv().await;
                                    if let Some(rejection) = rejected.get(account) {
                                        let _ = socket.send(Upstream::Text(rejection.clone().into())).await;
                                        while let Some(Ok(_)) = socket.recv().await {}
                                        return;
                                    }
                                    for frame in [CREATED, DONE] {
                                        let _ = socket.send(Upstream::Text(frame.into())).await;
                                    }
                                    let _ = socket.recv().await;
                                    let next = if active { NEXT_DELTA } else { NEXT_DONE };
                                    for frame in [NEXT_CREATED, next] {
                                        let _ = socket.send(Upstream::Text(frame.into())).await;
                                    }
                                    release.acquire().await.unwrap().forget();
                                    let reason = format!("lost Bearer {}", TOKENS[account]);
                                    let _ = socket
                                        .send(Upstream::Close(Some(CloseFrame {
                                            code: 1011,
                                            reason: reason.into(),
                                        })))
                                        .await;
                                    while let Some(Ok(_)) = socket.recv().await {}
                                })
                            }
                        }),
                    )
                };
                let upstream = format!("http://{}", serve(app).await);
                let config = if buffering {
                    "codex:\n  stream-bootstrap-buffering: true\n"
                } else {
                    ""
                };
                let proxy = proxy_with(Script::default(), config, |_| {
                    TOKENS
                        .iter()
                        .enumerate()
                        .map(|(i, token)| {
                            let mut c = credential(Profile::Chatgpt, &upstream, true);
                            if i > 0 {
                                c.id = format!("codex-{i}.json");
                                c.metadata.insert("access_token".into(), serde_json::json!(token));
                            }
                            c.attributes.insert("priority".into(), (9 - i).to_string());
                            c
                        })
                        .collect()
                })
                .await;
                proxy.rt.usage_queue().configure(
                    true,
                    &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
                );
                let mut socket = connect(&proxy).await;
                socket
                    .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
                    .await
                    .unwrap();
                let first = read(&mut socket, &TERMINAL).await;
                assert_eq!(
                    (first.frames, first.closed),
                    (vec![CREATED.to_owned(), DONE.to_owned()], None),
                    "{case}"
                );
                socket.send(Message::text(continuation)).await.unwrap();
                let until: &[&str] = if active {
                    &["response.output_text.delta"]
                } else {
                    &TERMINAL
                };
                let next = read(&mut socket, until).await;
                let expected = if active { NEXT_DELTA } else { NEXT_DONE };
                assert_eq!(next.frames, [NEXT_CREATED, expected], "{case}");
                release.add_permits(1);
                let end = tokio::time::timeout(Duration::from_secs(2), read(&mut socket, &[]))
                    .await
                    .unwrap_or_else(|_| panic!("{case}: the serving socket's loss did not close the client"));
                assert!(end.closed.is_some(), "{case}: {end:?}");
                assert!(
                    end.frames.is_empty(),
                    "{case}: no event after the loss, not the rejected account's limit: {end:?}"
                );
                let records = records(&proxy, serving + 2).await;
                tokio::time::sleep(Duration::from_millis(100)).await;
                assert!(
                    proxy.rt.usage_queue().pop_oldest(10).is_empty(),
                    "{case}: no other record"
                );
                let auths = auths.lock().unwrap().clone();
                let dialed: Vec<String> = TOKENS[..=serving].iter().map(|t| format!("Bearer {t}")).collect();
                assert_eq!(auths, dialed, "{case}: one dial per account, in order");
                for (record, (_, status)) in records.iter().zip(rejections.iter()) {
                    assert_eq!(record["failed"], true, "{case}: {record}");
                    assert_eq!(record["fail"]["status_code"], *status, "{case}");
                    assert_eq!(tokens(record), [0, 0, 0], "{case}");
                }
                let served = &records[serving..];
                assert_eq!(served[0]["failed"], false, "{case}: {served:?}");
                assert_eq!(tokens(&served[0]), [4, 1, 5], "{case}");
                if active {
                    assert_eq!(served[1]["failed"], true, "{case}: {served:?}");
                    assert_eq!(tokens(&served[1]), [0, 0, 0], "{case}");
                } else {
                    assert_eq!(served[1]["failed"], false, "{case}: {served:?}");
                    assert_eq!(tokens(&served[1]), [4, 1, 5], "{case}");
                }
                let seen = format!("{:?}{:?}{:?}", next.frames, end, records);
                for token in TOKENS {
                    assert!(!seen.contains(&format!("Bearer {token}")), "{case}: {seen}");
                }
                for record in served {
                    assert!(!carries_token(&record.to_string(), TOKENS[serving]), "{case}: {record}");
                }
            }
        }
    }
}

/// #25 R3: a failover that ends without a serving socket renders its own last failure.
/// The first account answers a safe 429 with a subscription reset and loses its socket.
/// When a second account is available, its handshake is refused (400): the turn ends with
/// that refusal, and the client never gets the first account's limit event, whose socket
/// no longer serves the session. With the first account alone, its limit is the turn's
/// own last failure, and the client gets that event. Records: the 429, then the 400.
#[tokio::test]
async fn a_failover_that_ends_without_a_socket_renders_its_last_failure() {
    use axum::extract::ws::{Message as Upstream, WebSocketUpgrade};
    use axum::response::IntoResponse;
    use std::sync::Mutex;
    const LIMITED: &str = r#"{"type":"error","status":429,"error":{"type":"usage_limit_reached","message":"The usage limit has been reached.","resets_at":2000000000}}"#;
    for buffering in [false, true] {
        for second in [true, false] {
            let case = format!("buffering={buffering} second account={second}");
            let auths = Arc::new(Mutex::new(Vec::<String>::new()));
            let app = {
                let auths = auths.clone();
                axum::Router::new().route(
                    "/responses",
                    axum::routing::get(move |headers: axum::http::HeaderMap, upgrade: WebSocketUpgrade| {
                        let auths = auths.clone();
                        async move {
                            let authorization = headers["authorization"].to_str().unwrap().to_owned();
                            auths.lock().unwrap().push(authorization.clone());
                            if authorization != format!("Bearer {SELECTED}") {
                                // The first socket's loss is reported meanwhile.
                                tokio::time::sleep(Duration::from_millis(250)).await;
                                return (axum::http::StatusCode::BAD_REQUEST, "safe-second-rejection").into_response();
                            }
                            upgrade.on_upgrade(|mut socket| async move {
                                let _ = socket.recv().await;
                                let _ = socket.send(Upstream::Text(LIMITED.into())).await;
                                while let Some(Ok(_)) = socket.recv().await {}
                            })
                        }
                    }),
                )
            };
            let upstream = format!("http://{}", serve(app).await);
            let config = if buffering {
                "codex:\n  stream-bootstrap-buffering: true\n"
            } else {
                ""
            };
            let proxy = proxy_with(Script::default(), config, |_| {
                let mut accounts = vec![credential(Profile::Chatgpt, &upstream, true)];
                accounts[0].attributes.insert("priority".into(), "9".into());
                if second {
                    let mut c = credential(Profile::Chatgpt, &upstream, true);
                    c.id = "codex-1.json".into();
                    c.metadata.insert("access_token".into(), serde_json::json!(SECOND));
                    c.attributes.insert("priority".into(), "8".into());
                    accounts.push(c);
                }
                accounts
            })
            .await;
            proxy.rt.usage_queue().configure(
                true,
                &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
            );
            let mut socket = connect(&proxy).await;
            socket
                .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
                .await
                .unwrap();
            let got = read(&mut socket, &[]).await;
            assert!(got.closed.is_some(), "{case}: {got:?}");
            let records = records(&proxy, if second { 2 } else { 1 }).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(
                proxy.rt.usage_queue().pop_oldest(10).is_empty(),
                "{case}: no other record"
            );
            let auths = auths.lock().unwrap().clone();
            assert_eq!(auths.len(), if second { 2 } else { 1 }, "{case}: {auths:?}");
            assert_eq!(records[0]["fail"]["status_code"], 429, "{case}");
            let limited = got.frames.iter().any(|f| f.contains("usage_limit_reached"));
            if second {
                assert_eq!(records[1]["failed"], true, "{case}");
                assert_eq!(records[1]["fail"]["status_code"], 400, "{case}");
                assert!(
                    !limited,
                    "{case}: the first account's limit is not the turn's failure: {got:?}"
                );
            } else {
                assert!(
                    limited && got.frames.iter().any(|f| f.contains("2000000000")),
                    "{case}: the account's own limit: {got:?}"
                );
            }
            for record in &records {
                assert_eq!(tokens(record), [0, 0, 0], "{case}");
            }
        }
    }
}

/// #25 R1: a lost socket's failure belongs to the attempt that ran on it. The first
/// account's socket answers a safe 429 with a subscription reset and is lost; the turn
/// fails over to an account served over HTTP, which fails too: synchronously, with the
/// request-scoped 500 of an invalid configured URL (a Codex client then gets a silent
/// close), or with a real HTTP 400. The client gets that second failure's own rendering,
/// never the first account's limit event. One socket was dialed; the records are the 429
/// and the second failure, without tokens; with and without bootstrap buffering.
#[tokio::test]
async fn an_http_attempt_after_a_lost_socket_fails_with_its_own_error() {
    use axum::extract::ws::{Message as Upstream, WebSocketUpgrade};
    use std::sync::atomic::{AtomicUsize, Ordering};
    const LIMITED: &str = r#"{"type":"error","status":429,"error":{"type":"usage_limit_reached","message":"The usage limit has been reached.","resets_at":2000000000}}"#;
    const REJECTED: &str =
        r#"{"error":{"type":"invalid_request_error","code":"bad_input","message":"safe second rejection"}}"#;
    for buffering in [false, true] {
        for valid in [false, true] {
            let case = format!("buffering={buffering} valid http url={valid}");
            let (dials, posts) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
            let app = {
                let (dials, posts) = (dials.clone(), posts.clone());
                axum::Router::new().route(
                    "/responses",
                    axum::routing::get(move |upgrade: WebSocketUpgrade| {
                        dials.fetch_add(1, Ordering::SeqCst);
                        async move {
                            upgrade.on_upgrade(|mut socket| async move {
                                let _ = socket.recv().await;
                                let _ = socket.send(Upstream::Text(LIMITED.into())).await;
                                while let Some(Ok(_)) = socket.recv().await {}
                            })
                        }
                    })
                    .post(move || {
                        posts.fetch_add(1, Ordering::SeqCst);
                        async move {
                            (
                                axum::http::StatusCode::BAD_REQUEST,
                                [("content-type", "application/json")],
                                REJECTED,
                            )
                        }
                    }),
                )
            };
            let upstream = format!("http://{}", serve(app).await);
            let config = if buffering {
                "codex:\n  stream-bootstrap-buffering: true\n"
            } else {
                ""
            };
            let proxy = proxy_with(Script::default(), config, |_| {
                let mut first = credential(Profile::Chatgpt, &upstream, true);
                first.attributes.insert("priority".into(), "9".into());
                let http = if valid { upstream.as_str() } else { "http://[invalid" };
                let mut second = credential(Profile::Chatgpt, http, false);
                second.id = "codex-1.json".into();
                second.metadata.insert("access_token".into(), serde_json::json!(SECOND));
                second.attributes.insert("priority".into(), "0".into());
                vec![first, second]
            })
            .await;
            proxy.rt.usage_queue().configure(
                true,
                &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
            );
            let mut socket = connect(&proxy).await;
            socket
                .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
                .await
                .unwrap();
            let got = read(&mut socket, &[]).await;
            let records = records(&proxy, 2).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(
                proxy.rt.usage_queue().pop_oldest(10).is_empty(),
                "{case}: no other record"
            );
            assert!(got.closed.is_some(), "{case}: {got:?}");
            assert!(
                !got.frames
                    .iter()
                    .any(|f| f.contains("usage_limit_reached") || f.contains("2000000000")),
                "{case}: the first account's limit is not the turn's failure: {got:?}"
            );
            assert_eq!(dials.load(Ordering::SeqCst), 1, "{case}: one socket");
            assert_eq!(
                posts.load(Ordering::SeqCst),
                usize::from(valid),
                "{case}: HTTP requests"
            );
            assert_eq!(records[0]["fail"]["status_code"], 429, "{case}");
            assert_eq!(records[1]["failed"], true, "{case}: {records:?}");
            assert_eq!(
                records[1]["fail"]["status_code"],
                if valid { 400 } else { 500 },
                "{case}"
            );
            for record in &records {
                assert_eq!(tokens(record), [0, 0, 0], "{case}");
            }
            if valid {
                assert!(
                    got.frames.iter().any(|f| f.contains("safe second rejection")),
                    "{case}: the HTTP attempt's own failure: {got:?}"
                );
            } else {
                assert!(got.frames.is_empty(), "{case}: a silent close: {got:?}");
            }
        }
    }
}

/// #25 R1: a socket's loss stands in only for the failure of the attempt that acquired
/// that socket. Both accounts take WebSocket turns. The first account's socket answers a
/// safe 429 with a subscription reset and is lost; the turn fails over to the second
/// account, configured with a thinking-suffixed upstream model (`gpt-5.5(high)`), which
/// the native route refuses with a request-scoped 400 before it acquires or dials any
/// socket. The client gets that refusal, never the first account's limit event. Control:
/// when the second account does serve on its own socket and that socket closes 1009,
/// the client gets that socket's own loss (close 1009), still never the first account's
/// limit. Bootstrap buffering off and on; one record per attempt, without tokens.
#[tokio::test]
async fn a_refusal_before_any_socket_fails_with_its_own_error() {
    use axum::extract::ws::{CloseFrame, Message as Upstream, WebSocketUpgrade};
    use std::sync::Mutex;
    const LIMITED: &str = r#"{"type":"error","status":429,"error":{"type":"usage_limit_reached","message":"abandoned account","resets_at":2000000000}}"#;
    for buffering in [false, true] {
        for refused in [true, false] {
            let case = format!("buffering={buffering} refused before a socket={refused}");
            let dials = Arc::new(Mutex::new(Vec::<String>::new()));
            let app = {
                let dials = dials.clone();
                axum::Router::new().route(
                    "/responses",
                    axum::routing::get(move |headers: axum::http::HeaderMap, upgrade: WebSocketUpgrade| {
                        let authorization = headers["authorization"].to_str().unwrap().to_owned();
                        dials.lock().unwrap().push(authorization.clone());
                        async move {
                            upgrade.on_upgrade(move |mut socket| async move {
                                let _ = socket.recv().await;
                                if authorization == "Bearer key-OLD" {
                                    let _ = socket.send(Upstream::Text(LIMITED.into())).await;
                                } else {
                                    let _ = socket
                                        .send(Upstream::Close(Some(CloseFrame {
                                            code: 1009,
                                            reason: "too big".into(),
                                        })))
                                        .await;
                                }
                                while let Some(Ok(_)) = socket.recv().await {}
                            })
                        }
                    }),
                )
            };
            let upstream = format!("http://{}", serve(app).await);
            let second_model = if refused { "gpt-5.5(high)" } else { "gpt-5.5" };
            let config = format!(
                "codex:\n  stream-bootstrap-buffering: {buffering}\ncodex-api-key:\n  - api-key: key-OLD\n    base-url: {upstream}\n    priority: 9\n    websockets: true\n    models: [{{name: gpt-5.5, alias: shared}}]\n  - api-key: key-CURRENT\n    base-url: {upstream}\n    priority: 0\n    websockets: true\n    models: [{{name: '{second_model}', alias: shared}}]\n"
            );
            let proxy = proxy_with(Script::default(), &config, |_| Vec::new()).await;
            proxy.rt.usage_queue().configure(
                true,
                &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
            );
            let mut socket = connect(&proxy).await;
            socket
                .send(Message::text(client_frame("ws-create-first", "shared")))
                .await
                .unwrap();
            let got = read(&mut socket, &[]).await;
            let records = records(&proxy, 2).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(
                proxy.rt.usage_queue().pop_oldest(10).is_empty(),
                "{case}: no other record"
            );
            let dials = dials.lock().unwrap().clone();
            assert!(
                !got.frames
                    .iter()
                    .any(|f| f.contains("usage_limit_reached") || f.contains("2000000000")),
                "{case}: the first account's limit is not the turn's failure: {got:?}"
            );
            assert_eq!(records[0]["fail"]["status_code"], 429, "{case}");
            assert_eq!(records[1]["failed"], true, "{case}: {records:?}");
            for record in &records {
                assert_eq!(tokens(record), [0, 0, 0], "{case}");
            }
            if refused {
                assert_eq!(dials, ["Bearer key-OLD"], "{case}: the refusal dials nothing");
                assert_eq!(records[1]["fail"]["status_code"], 400, "{case}");
                assert!(
                    got.frames.len() == 1 && got.frames[0].contains("model suffix"),
                    "{case}: the refusal's own error: {got:?}"
                );
            } else {
                assert_eq!(dials, ["Bearer key-OLD", "Bearer key-CURRENT"], "{case}");
                assert_eq!(got.closed, Some(1009), "{case}: its own socket's loss: {got:?}");
            }
        }
    }
}

/// #25 R3: a retained socket's loss does not end a turn that runs elsewhere. A first
/// create completes on account A's socket (4/1/5), which the connection keeps. The next
/// create names a model only HTTP account B serves; B streams a real delta, and its
/// completion waits. Then A's idle socket closes 1011, its own bearer in the reason. The
/// client keeps its connection, gets B's completion, and B's record is a success with
/// its 4/1/5 tokens, not a cancellation. The control has A's socket stay open. One WS
/// dial for A, one HTTP request for B; with and without bootstrap buffering.
#[tokio::test]
async fn a_retained_socket_loss_does_not_end_a_turn_served_elsewhere() {
    use axum::extract::ws::{CloseFrame, Message as Upstream, WebSocketUpgrade};
    use std::sync::Mutex;
    const DONE: &str = r#"{"type":"response.completed","response":{"id":"first","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    const DELTA: &str =
        r#"{"type":"response.output_text.delta","response_id":"current","delta":"real current prefix"}"#;
    const CURRENT_DONE: &str = r#"{"type":"response.completed","response":{"id":"current","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    for buffering in [false, true] {
        for close_old in [false, true] {
            let case = format!("buffering={buffering} old socket closes={close_old}");
            let seen = Arc::new(Mutex::new(Vec::<String>::new()));
            let (old_gate, tail_gate) = (
                Arc::new(tokio::sync::Semaphore::new(0)),
                Arc::new(tokio::sync::Semaphore::new(0)),
            );
            let old_closed = Arc::new(tokio::sync::Notify::new());
            let app = {
                let (ws_seen, http_seen) = (seen.clone(), seen.clone());
                let (old_gate, tail_gate, old_closed) = (old_gate.clone(), tail_gate.clone(), old_closed.clone());
                axum::Router::new().route(
                    "/responses",
                    axum::routing::get(move |headers: axum::http::HeaderMap, upgrade: WebSocketUpgrade| {
                        ws_seen
                            .lock()
                            .unwrap()
                            .push(format!("WS {}", headers["authorization"].to_str().unwrap()));
                        let (gate, closed) = (old_gate.clone(), old_closed.clone());
                        async move {
                            upgrade.on_upgrade(move |mut socket| async move {
                                let _ = socket.recv().await;
                                let _ = socket.send(Upstream::Text(DONE.into())).await;
                                gate.acquire().await.unwrap().forget();
                                if close_old {
                                    let _ = socket
                                        .send(Upstream::Close(Some(CloseFrame {
                                            code: 1011,
                                            reason: "old idle loss key-OLD".into(),
                                        })))
                                        .await;
                                    closed.notify_one();
                                }
                                while let Some(Ok(_)) = socket.recv().await {}
                            })
                        }
                    })
                    .post(move |headers: axum::http::HeaderMap| {
                        http_seen
                            .lock()
                            .unwrap()
                            .push(format!("HTTP {}", headers["authorization"].to_str().unwrap()));
                        let tail = tail_gate.clone();
                        async move {
                            let first = futures_util::stream::iter([Ok::<_, std::io::Error>(Bytes::from(format!(
                                "data: {DELTA}\n\n"
                            )))]);
                            let last = futures_util::stream::once(async move {
                                tail.acquire().await.unwrap().forget();
                                Ok(Bytes::from(format!("data: {CURRENT_DONE}\n\n")))
                            });
                            (
                                [("content-type", "text/event-stream")],
                                axum::body::Body::from_stream(first.chain(last)),
                            )
                        }
                    }),
                )
            };
            let upstream = format!("http://{}", serve(app).await);
            let config = format!(
                "codex:\n  stream-bootstrap-buffering: {buffering}\ncodex-api-key:\n  - api-key: key-OLD\n    base-url: {upstream}\n    websockets: true\n    models: [{{name: gpt-5.5, alias: socket-model}}]\n  - api-key: key-CURRENT\n    base-url: {upstream}\n    websockets: false\n    models: [{{name: gpt-5.5, alias: http-model}}]\n"
            );
            let proxy = proxy_with(Script::default(), &config, |_| Vec::new()).await;
            proxy.rt.usage_queue().configure(
                true,
                &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
            );
            let mut socket = connect(&proxy).await;
            socket
                .send(Message::text(client_frame("ws-create-first", "socket-model")))
                .await
                .unwrap();
            assert_eq!(read(&mut socket, &TERMINAL).await.frames, [DONE], "{case}");
            socket
                .send(Message::text(client_frame("ws-create-first", "http-model")))
                .await
                .unwrap();
            let delta = read(&mut socket, &["response.output_text.delta"]).await;
            assert_eq!((delta.frames, delta.closed), (vec![DELTA.to_owned()], None), "{case}");
            old_gate.add_permits(1);
            if close_old {
                tokio::time::timeout(Duration::from_secs(2), old_closed.notified())
                    .await
                    .unwrap();
            }
            let early = tokio::time::timeout(Duration::from_millis(180), socket.next()).await;
            assert!(
                early.is_err(),
                "{case}: the turn served elsewhere ended early: {early:?}"
            );
            tail_gate.add_permits(1);
            let rest = read(&mut socket, &TERMINAL).await;
            assert_eq!(
                (rest.frames, rest.closed),
                (vec![CURRENT_DONE.to_owned()], None),
                "{case}"
            );
            let records = records(&proxy, 2).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(
                proxy.rt.usage_queue().pop_oldest(10).is_empty(),
                "{case}: no other record"
            );
            assert_eq!(
                seen.lock().unwrap().clone(),
                ["WS Bearer key-OLD", "HTTP Bearer key-CURRENT"],
                "{case}"
            );
            for record in &records {
                assert_eq!(record["failed"], false, "{case}: {record}");
                assert_eq!(tokens(record), [4, 1, 5], "{case}");
            }
        }
    }
}

/// #25: a continuation belongs to the socket that holds its previous response, whatever
/// transport the connection's last turn used. A first create completes on account A's
/// socket (response `first`, 4/1/5). The next create runs on HTTP-only account B and
/// completes (4/1/5); meanwhile A's idle socket closes 1011, or stays open in the
/// control. A third create then names `previous_response_id: first`:
/// - A's socket was lost: the replay close (1012), no new dial, nothing sent upstream,
///   and one failed record without tokens;
/// - A's socket is still open: the continuation goes on that same socket and completes;
/// - a fresh create without a previous response, after the loss, dials a new socket and
///   completes.
/// With and without bootstrap buffering.
#[tokio::test]
async fn a_continuation_needs_the_socket_that_holds_its_response() {
    use axum::extract::ws::{CloseFrame, Message as Upstream, WebSocketUpgrade};
    use std::sync::Mutex;
    const FIRST: &str = r#"{"type":"response.completed","response":{"id":"first","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    const CURRENT: &str = r#"{"type":"response.completed","response":{"id":"current","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    const NEXT: &str = r#"{"type":"response.completed","response":{"id":"next","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    let continuation = r#"{"type":"response.create","model":"socket-model","previous_response_id":"first","input":[]}"#;
    for buffering in [false, true] {
        for (case_name, close_old, continues) in [
            ("lost socket, continuation", true, true),
            ("lost socket, fresh create", true, false),
            ("live socket, continuation", false, true),
        ] {
            let case = format!("buffering={buffering} {case_name}");
            let seen = Arc::new(Mutex::new(Vec::<String>::new()));
            let frames = Arc::new(Mutex::new(Vec::<String>::new()));
            let old_gate = Arc::new(tokio::sync::Semaphore::new(0));
            let old_closed = Arc::new(tokio::sync::Notify::new());
            let app = {
                let (ws_seen, http_seen, frames) = (seen.clone(), seen.clone(), frames.clone());
                let (old_gate, old_closed) = (old_gate.clone(), old_closed.clone());
                axum::Router::new().route(
                    "/responses",
                    axum::routing::get(move |headers: axum::http::HeaderMap, upgrade: WebSocketUpgrade| {
                        let dial = {
                            let mut seen = ws_seen.lock().unwrap();
                            seen.push(format!("WS {}", headers["authorization"].to_str().unwrap()));
                            seen.iter().filter(|s| s.starts_with("WS")).count()
                        };
                        let (gate, closed, frames) = (old_gate.clone(), old_closed.clone(), frames.clone());
                        async move {
                            upgrade.on_upgrade(move |mut socket| async move {
                                let mut replies = if dial == 1 { vec![FIRST, NEXT] } else { vec![NEXT] }.into_iter();
                                while let Some(Ok(message)) = socket.recv().await {
                                    let Upstream::Text(text) = message else { continue };
                                    frames.lock().unwrap().push(text.to_string());
                                    let Some(reply) = replies.next() else { continue };
                                    let _ = socket.send(Upstream::Text(reply.into())).await;
                                    if dial == 1 && reply == FIRST {
                                        gate.acquire().await.unwrap().forget();
                                        if close_old {
                                            let _ = socket
                                                .send(Upstream::Close(Some(CloseFrame {
                                                    code: 1011,
                                                    reason: "old idle loss".into(),
                                                })))
                                                .await;
                                            closed.notify_one();
                                            return;
                                        }
                                    }
                                }
                            })
                        }
                    })
                    .post(move |headers: axum::http::HeaderMap| {
                        http_seen
                            .lock()
                            .unwrap()
                            .push(format!("HTTP {}", headers["authorization"].to_str().unwrap()));
                        async move { ([("content-type", "text/event-stream")], format!("data: {CURRENT}\n\n")) }
                    }),
                )
            };
            let upstream = format!("http://{}", serve(app).await);
            let config = format!(
                "codex:\n  stream-bootstrap-buffering: {buffering}\ncodex-api-key:\n  - api-key: key-OLD\n    base-url: {upstream}\n    websockets: true\n    models: [{{name: gpt-5.5, alias: socket-model}}]\n  - api-key: key-CURRENT\n    base-url: {upstream}\n    websockets: false\n    models: [{{name: gpt-5.5, alias: http-model}}]\n"
            );
            let proxy = proxy_with(Script::default(), &config, |_| Vec::new()).await;
            proxy.rt.usage_queue().configure(
                true,
                &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
            );
            let mut socket = connect(&proxy).await;
            socket
                .send(Message::text(client_frame("ws-create-first", "socket-model")))
                .await
                .unwrap();
            assert_eq!(read(&mut socket, &TERMINAL).await.frames, [FIRST], "{case}");
            socket
                .send(Message::text(client_frame("ws-create-first", "http-model")))
                .await
                .unwrap();
            old_gate.add_permits(1);
            if close_old {
                tokio::time::timeout(Duration::from_secs(2), old_closed.notified())
                    .await
                    .unwrap();
            }
            let second = read(&mut socket, &TERMINAL).await;
            assert_eq!(
                (second.frames, second.closed),
                (vec![CURRENT.to_owned()], None),
                "{case}"
            );
            assert!(
                tokio::time::timeout(Duration::from_millis(70), socket.next())
                    .await
                    .is_err(),
                "{case}: the connection stays open"
            );
            let third = if continues {
                continuation.to_owned()
            } else {
                client_frame("ws-create-first", "socket-model")
            };
            socket.send(Message::text(third)).await.unwrap();
            let got = read(&mut socket, &TERMINAL).await;
            let records = records(&proxy, 3).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(
                proxy.rt.usage_queue().pop_oldest(10).is_empty(),
                "{case}: no other record"
            );
            let seen = seen.lock().unwrap().clone();
            let frames = frames.lock().unwrap().clone();
            for record in &records[..2] {
                assert_eq!(record["failed"], false, "{case}: {record}");
                assert_eq!(tokens(record), [4, 1, 5], "{case}");
            }
            match (close_old, continues) {
                (true, true) => {
                    assert_eq!(got.closed, Some(1012), "{case}: the replay close: {got:?}");
                    assert!(got.frames.is_empty(), "{case}: {got:?}");
                    assert_eq!(
                        seen,
                        ["WS Bearer key-OLD", "HTTP Bearer key-CURRENT"],
                        "{case}: no new dial"
                    );
                    assert_eq!(frames.len(), 1, "{case}: nothing more sent upstream: {frames:?}");
                    assert_eq!(records[2]["failed"], true, "{case}: {}", records[2]);
                    assert_eq!(records[2]["fail"]["status_code"], 426, "{case}");
                    assert_eq!(tokens(&records[2]), [0, 0, 0], "{case}");
                }
                (true, false) => {
                    assert_eq!((got.frames, got.closed), (vec![NEXT.to_owned()], None), "{case}");
                    assert_eq!(
                        seen,
                        ["WS Bearer key-OLD", "HTTP Bearer key-CURRENT", "WS Bearer key-OLD"],
                        "{case}: a new socket"
                    );
                    assert!(!frames[1].contains("previous_response_id"), "{case}: {frames:?}");
                    assert_eq!(records[2]["failed"], false, "{case}");
                }
                _ => {
                    assert_eq!((got.frames, got.closed), (vec![NEXT.to_owned()], None), "{case}");
                    assert_eq!(
                        seen,
                        ["WS Bearer key-OLD", "HTTP Bearer key-CURRENT"],
                        "{case}: the same socket"
                    );
                    assert!(
                        frames[1].contains(r#""previous_response_id":"first""#),
                        "{case}: {frames:?}"
                    );
                    assert_eq!(records[2]["failed"], false, "{case}");
                    assert_eq!(tokens(&records[2]), [4, 1, 5], "{case}");
                }
            }
        }
    }
}

// N1 (#111): assertions the native-protection audit found missing
// (docs/codex/N1-NATIVE-PROTECTIONS.md). Each test pins a protection the
// proxy already has; none of them changes behaviour. Frames are hand-authored in the
// shapes of the pinned first-party decoders cited on each test.

/// One upstream socket as the N1 recording upstream saw it.
#[derive(Debug, Clone, Default)]
struct Dialed {
    bearer: String,
    frames: Vec<String>,
    /// The proxy closed the socket or dropped its connection.
    ended: bool,
}

/// A loopback Codex upstream for the N1 tests. Socket `dial` (in dial order) answers its
/// `n`th text frame with `answer(dial, n)`: the frames to send, then whether to drop the
/// connection without a close frame. It records each socket's bearer, every text frame
/// and whether the proxy ended the socket.
async fn recording_upstream(
    answer: impl Fn(usize, usize) -> (Vec<&'static str>, bool) + Clone + Send + Sync + 'static,
) -> (String, Arc<std::sync::Mutex<Vec<Dialed>>>) {
    use axum::extract::ws::{Message as Upstream, WebSocketUpgrade};
    let log = Arc::new(std::sync::Mutex::new(Vec::<Dialed>::new()));
    let app = {
        let log = log.clone();
        axum::Router::new().route(
            "/responses",
            axum::routing::get(move |headers: axum::http::HeaderMap, upgrade: WebSocketUpgrade| {
                let (log, answer) = (log.clone(), answer.clone());
                async move {
                    let dial = {
                        let mut log = log.lock().unwrap();
                        log.push(Dialed {
                            bearer: headers["authorization"].to_str().unwrap().to_owned(),
                            ..Dialed::default()
                        });
                        log.len() - 1
                    };
                    upgrade.on_upgrade(move |mut socket| async move {
                        let mut n = 0;
                        loop {
                            match socket.recv().await {
                                Some(Ok(Upstream::Text(text))) => {
                                    log.lock().unwrap()[dial].frames.push(text.as_str().to_owned());
                                    let (frames, drop_after) = answer(dial, n);
                                    n += 1;
                                    for frame in frames {
                                        let _ = socket.send(Upstream::Text(frame.into())).await;
                                    }
                                    if drop_after {
                                        // The proxy did not end this socket: the upstream did.
                                        return;
                                    }
                                }
                                Some(Ok(Upstream::Close(_))) | Some(Err(_)) | None => break,
                                Some(Ok(_)) => {}
                            }
                        }
                        log.lock().unwrap()[dial].ended = true;
                    })
                }
            }),
        )
    };
    (format!("http://{}", serve(app).await), log)
}

/// Two ChatGPT accounts on `url`, tried in order: `at-FAKE`, then `at-SECOND`.
fn two_accounts(url: &str) -> Vec<Credential> {
    (0..2)
        .map(|i| {
            let mut c = credential(Profile::Chatgpt, url, true);
            if i > 0 {
                c.id = "codex-second.json".into();
                c.metadata.insert("access_token".into(), "at-SECOND".into());
            }
            c.attributes.insert("priority".into(), (9 - i).to_string());
            c
        })
        .collect()
}

/// Waits until `check` holds for the recording upstream's log, or panics after 5 s.
async fn upstream_until(log: &std::sync::Mutex<Vec<Dialed>>, what: &str, check: impl Fn(&[Dialed]) -> bool) {
    for _ in 0..500 {
        if check(&log.lock().unwrap()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{what}: {:?}", log.lock().unwrap());
}

const N1_CREATED: &str = r#"{"type":"response.created","response":{"id":"resp_n1"}}"#;
const N1_DONE: &str = r#"{"type":"response.completed","response":{"id":"resp_n1","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;

/// N1 per-turn ownership: each turn is admitted on its own. An account disabled between
/// two turns of one connection serves nothing more: a full `response.create` is admitted
/// on the next account and opens that account's socket, and a continuation, which only
/// the disabled account's socket could serve (first-party `core/src/client.rs:1675-1706`
/// and 2354-2376 at 44ccf394573c send `previous_response_id` only on the connection that
/// holds the previous response), closes with 1012 and writes nothing upstream. Nothing
/// more is written to the disabled account's socket in either case
/// (`docs/codex/CONTRACT.md` "Socket and account ownership").
#[tokio::test]
async fn an_account_disabled_between_turns_serves_no_later_turn() {
    for continuation in [false, true] {
        let case = format!("continuation={continuation}");
        let (url, log) = recording_upstream(|_, _| (vec![N1_CREATED, N1_DONE], false)).await;
        let proxy = proxy_with(Script::default(), "", |_| two_accounts(&url)).await;
        let mut socket = connect(&proxy).await;
        let first = client_frame("ws-create-first", "gpt-5.5");
        socket.send(Message::text(first.clone())).await.unwrap();
        assert_eq!(
            read(&mut socket, &TERMINAL).await.frames,
            [N1_CREATED, N1_DONE],
            "{case}"
        );

        let mut disabled = proxy
            .rt
            .store()
            .snapshot()
            .into_iter()
            .find(|c| c.id != "codex-second.json" && c.metadata.get("access_token").is_some())
            .map(|c| Credential::clone(&c))
            .expect("the first account");
        disabled.disabled = true;
        proxy.rt.store().upsert(disabled);

        let next = if continuation {
            client_frame("ws-create-continuation", "gpt-5.5")
        } else {
            client_frame("ws-create-first", "gpt-5.5")
        };
        socket.send(Message::text(next.clone())).await.unwrap();
        let got = read(&mut socket, &TERMINAL).await;
        let dialed = log.lock().unwrap().clone();
        assert_eq!(dialed[0].bearer, "Bearer at-FAKE", "{case}");
        assert_eq!(
            dialed[0].frames,
            std::slice::from_ref(&first),
            "{case}: the disabled account's socket got nothing more"
        );
        if continuation {
            assert_eq!(got.frames, Vec::<String>::new(), "{case}: {got:?}");
            assert_eq!(got.closed, Some(1012), "{case}: the replay close");
            assert_eq!(dialed.len(), 1, "{case}: no socket for the continuation: {dialed:?}");
        } else {
            assert_eq!(got.frames, [N1_CREATED, N1_DONE], "{case}");
            assert_eq!(got.closed, None, "{case}");
            assert_eq!(dialed.len(), 2, "{case}: {dialed:?}");
            assert_eq!(dialed[1].bearer, "Bearer at-SECOND", "{case}");
            assert_eq!(
                dialed[1].frames,
                [next],
                "{case}: the create, as sent, on the next account"
            );
        }
    }
}

/// N1 early overload: a `response.failed` with `server_is_overloaded` as the first
/// upstream event (the shape first-party Codex reads as `ServerOverloaded`,
/// `sse/responses_error.rs:90` and `api_bridge.rs:70` at 44ccf394573c) fails the turn over
/// to the next account before anything reaches the client, with and without
/// `stream-bootstrap-buffering`. The client sees only the serving account's events and
/// no error, its connection stays open, and its continuation runs on the serving socket;
/// the overloaded socket receives nothing after its create
/// (`docs/codex/CONTRACT.md` "Responses and failures").
#[tokio::test]
async fn an_early_overload_fails_over_and_the_continuation_stays_on_the_serving_socket() {
    const OVERLOADED: &str = r#"{"type":"response.failed","response":{"id":"resp_over","error":{"code":"server_is_overloaded","message":null}}}"#;
    for buffering in [false, true] {
        let case = format!("buffering={buffering}");
        let (url, log) = recording_upstream(|dial, _| {
            if dial == 0 {
                (vec![OVERLOADED], false)
            } else {
                (vec![N1_CREATED, N1_DONE], false)
            }
        })
        .await;
        let config = if buffering {
            "codex:\n  stream-bootstrap-buffering: true\n"
        } else {
            ""
        };
        let proxy = proxy_with(Script::default(), config, |_| two_accounts(&url)).await;
        let mut socket = connect(&proxy).await;
        let first = client_frame("ws-create-first", "gpt-5.5");
        socket.send(Message::text(first.clone())).await.unwrap();
        let got = read(&mut socket, &TERMINAL).await;
        assert_eq!(
            got.frames,
            [N1_CREATED, N1_DONE],
            "{case}: no error before the serving answer"
        );
        assert_eq!(got.closed, None, "{case}");

        let continuation = client_frame("ws-create-continuation", "gpt-5.5");
        socket.send(Message::text(continuation.clone())).await.unwrap();
        let next = read(&mut socket, &TERMINAL).await;
        assert_eq!(next.frames, [N1_CREATED, N1_DONE], "{case}");
        let dialed = log.lock().unwrap().clone();
        assert_eq!(dialed.len(), 2, "{case}: {dialed:?}");
        assert_eq!(
            (dialed[0].bearer.as_str(), dialed[0].frames.as_slice()),
            ("Bearer at-FAKE", std::slice::from_ref(&first)),
            "{case}: the overloaded socket got only its create"
        );
        assert_eq!(
            (dialed[1].bearer.as_str(), dialed[1].frames.as_slice()),
            ("Bearer at-SECOND", &[first, continuation][..]),
            "{case}: the create, then the continuation, on the serving socket"
        );
    }
}

/// N1 uncertain loss, the protected half: once the client has received an upstream event
/// of the turn, losing the upstream socket ends the client connection and the create is
/// never sent to another account. First-party Codex ends the turn with "websocket closed
/// by server before response.completed" (`endpoint/responses_websocket.rs:1002-1005` at
/// 44ccf394573c) and resends on its own. Without `stream-bootstrap-buffering` the
/// `response.created` is delivered at once, so the loss comes after delivery.
///
/// A loss before anything was delivered is the unprotected half: the audit records a
/// bounded witness for it instead of an assertion here.
#[tokio::test]
async fn a_loss_after_delivery_never_replays_the_create_on_another_account() {
    let (url, log) = recording_upstream(|_, _| (vec![N1_CREATED], true)).await;
    let proxy = proxy_with(Script::default(), "", |_| two_accounts(&url)).await;
    let mut socket = connect(&proxy).await;
    let first = client_frame("ws-create-first", "gpt-5.5");
    socket.send(Message::text(first.clone())).await.unwrap();
    let got = read(&mut socket, &TERMINAL).await;
    assert_eq!(got.frames, [N1_CREATED], "the delivered event, as sent");
    assert!(got.closed.is_some(), "the loss reaches the client: {got:?}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let dialed = log.lock().unwrap().clone();
    assert_eq!(dialed.len(), 1, "no second account was dialed: {dialed:?}");
    assert_eq!(dialed[0].frames, [first], "the create was sent once");
}

/// N1 terminal teardown: the proxy releases the upstream socket of a turn that ended.
/// An ordinary turn's failure after the response started reaches the client as sent,
/// then the proxy closes both the client connection and that upstream socket, and writes
/// nothing more to it. A client that leaves after a completed turn, with or without
/// `codex.response-steering`, takes its upstream socket with it: no socket outlives its
/// connection, so no later turn can find it.
#[tokio::test]
async fn a_terminal_end_releases_the_upstream_socket() {
    const STARTED_FAILED: &str = r#"{"type":"response.failed","response":{"id":"resp_n1","status":"failed","error":{"code":"server_error","message":null},"usage":null}}"#;
    let (url, log) = recording_upstream(|_, _| (vec![N1_CREATED, STARTED_FAILED], false)).await;
    let proxy = proxy_with(Script::default(), "", |_| {
        vec![credential(Profile::Chatgpt, &url, true)]
    })
    .await;
    let mut socket = connect(&proxy).await;
    let first = client_frame("ws-create-first", "gpt-5.5");
    socket.send(Message::text(first.clone())).await.unwrap();
    let got = read(&mut socket, &[]).await;
    assert_eq!(got.frames, [N1_CREATED, STARTED_FAILED], "as sent");
    assert!(got.closed.is_some(), "an ordinary turn's failure ends the connection");
    upstream_until(&log, "the failed turn's upstream socket is closed", |d| {
        d.len() == 1 && d[0].ended
    })
    .await;
    assert_eq!(log.lock().unwrap()[0].frames, [first], "nothing more was written");

    for steering in [false, true] {
        let case = format!("steering={steering}");
        let (url, log) = recording_upstream(|_, _| (vec![N1_CREATED, N1_DONE], false)).await;
        let proxy = proxy_with(Script::default(), if steering { STEERING } else { "" }, |_| {
            vec![credential(Profile::Chatgpt, &url, true)]
        })
        .await;
        let mut socket = connect(&proxy).await;
        socket
            .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
            .await
            .unwrap();
        assert_eq!(
            read(&mut socket, &TERMINAL).await.frames,
            [N1_CREATED, N1_DONE],
            "{case}"
        );
        assert!(
            !log.lock().unwrap()[0].ended,
            "{case}: the idle socket stays for the next turn"
        );
        let _ = socket.close(1000, "done").await;
        upstream_until(&log, &format!("{case}: the client's upstream socket is closed"), |d| {
            d.len() == 1 && d[0].ended
        })
        .await;
    }
}

/// FP1 (#115) qualification at the research pin: a `response.create` whose
/// `function_call_output` carries multimodal content items, file-backed audio included
/// (`http-request-tool-output-media`, openai/codex main `5c3ff1dabf64`), reaches the
/// upstream socket byte for byte on both native profiles a loopback can stand for. Over
/// the WebSocket HTTP fallback it is the same body without the frame's `type`. This is
/// source-refresh evidence. The contract pin is unchanged, and endpoint acceptance stays
/// unknown.
#[tokio::test]
async fn tool_output_media_reaches_the_upstream_socket_byte_for_byte() {
    let frame = as_create("http-request-tool-output-media", "gpt-5.5");
    for profile in [Profile::Chatgpt, Profile::Compatible] {
        let proxy = proxy(
            profile,
            Script {
                ws: replies(&["sse-completed"]),
                ..Script::default()
            },
            "",
        )
        .await;
        let mut socket = connect(&proxy).await;
        socket.send(Message::text(frame.clone())).await.unwrap();
        assert_eq!(
            read(&mut socket, &TERMINAL).await.frames,
            events("sse-completed"),
            "{profile:?}"
        );
        assert_eq!(
            upstream_frames(&proxy, 0),
            std::slice::from_ref(&frame),
            "{profile:?}: the frame as sent"
        );
    }

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
    socket.send(Message::text(frame.clone())).await.unwrap();
    assert_eq!(read(&mut socket, &TERMINAL).await.frames, events("sse-completed"));
    let sent = proxy.upstream.http().pop().expect("an HTTP request");
    assert_eq!(
        text(sent.body),
        client_frame("http-request-tool-output-media", "gpt-5.5"),
        "the frame without its tag"
    );
}

// N1 (#111) safety fixes. W1: a request the upstream may already have read is never sent
// again on another account. W3: `response.metadata` is held during bootstrap buffering.
// W2: every send is authorized under the client key the connection was opened with.

const N1_IN_PROGRESS: &str = r#"{"type":"response.in_progress","response":{"id":"resp_n1"}}"#;
/// Prompt-moderation metadata in the shape of Go's upstream capture (Go issue #6856,
/// `9e79552f`): Go evidence, not verified provider behaviour. The pinned client reads
/// such a frame as `TurnModerationMetadata` (`sse/responses.rs:262-271` at 44ccf394573c).
const N1_MODERATION: &str = r#"{"type":"response.metadata","response_id":"resp_n1","sequence_number":2,"metadata":{"openai_chatgpt_moderation_metadata":{"prompt":{"omnimod":{"protection_type":"omnimod"}}}}}"#;
const N1_OVERLOADED: &str =
    r#"{"type":"response.failed","response":{"id":"resp_n1","error":{"code":"server_is_overloaded","message":null}}}"#;

/// How the N1 losing upstream ends its first socket after reading the create.
#[derive(Clone, Copy, Debug)]
enum End {
    Close(u16),
    Drop,
}

/// A loopback upstream whose first socket reads the create, sends `held`, then ends as
/// `end`; every later socket answers [`N1_CREATED`] and [`N1_DONE`]. Records every
/// socket's bearer and frames.
async fn losing_upstream(held: Vec<&'static str>, end: End) -> (String, Arc<std::sync::Mutex<Vec<Dialed>>>) {
    use axum::extract::ws::{CloseFrame, Message as Upstream, WebSocketUpgrade};
    let log = Arc::new(std::sync::Mutex::new(Vec::<Dialed>::new()));
    let app = {
        let log = log.clone();
        axum::Router::new().route(
            "/responses",
            axum::routing::get(move |headers: axum::http::HeaderMap, upgrade: WebSocketUpgrade| {
                let (log, held) = (log.clone(), held.clone());
                async move {
                    let dial = {
                        let mut log = log.lock().unwrap();
                        log.push(Dialed {
                            bearer: headers["authorization"].to_str().unwrap().to_owned(),
                            ..Dialed::default()
                        });
                        log.len() - 1
                    };
                    upgrade.on_upgrade(move |mut socket| async move {
                        while let Some(Ok(message)) = socket.recv().await {
                            let Upstream::Text(text) = message else { continue };
                            log.lock().unwrap()[dial].frames.push(text.as_str().to_owned());
                            if dial == 0 {
                                for frame in &held {
                                    let _ = socket.send(Upstream::Text((*frame).into())).await;
                                }
                                if let End::Close(code) = end {
                                    let _ = socket
                                        .send(Upstream::Close(Some(CloseFrame {
                                            code,
                                            reason: "lost".into(),
                                        })))
                                        .await;
                                }
                                return;
                            }
                            for frame in [N1_CREATED, N1_DONE] {
                                let _ = socket.send(Upstream::Text(frame.into())).await;
                            }
                        }
                    })
                }
            }),
        )
    };
    (format!("http://{}", serve(app).await), log)
}

/// N1 W1: once a create was written and the upstream read it, the upstream may run it.
/// A loss before anything reached the client (close 1000, close 1011 or a dropped
/// connection) ends the client connection, as any lost socket does
/// (`docs/codex/CONTRACT.md` "Socket and account ownership"), and the create is never
/// sent to another account: with bootstrap buffering, not even after the upstream sent
/// `response.created` and `response.metadata`, which stay undelivered. The account is not
/// cooled: the client's next connection is served by it.
#[tokio::test]
async fn an_uncertain_loss_is_never_replayed_on_another_account() {
    for buffering in [false, true] {
        let helds: &[&[&'static str]] = if buffering {
            &[&[], &[N1_CREATED], &[N1_CREATED, N1_IN_PROGRESS, N1_MODERATION]]
        } else {
            // Without buffering an event is delivered at once: the after-delivery control.
            &[&[]]
        };
        for held in helds {
            for end in [End::Close(1000), End::Close(1011), End::Drop] {
                let case = format!("buffering={buffering} held={} end={end:?}", held.len());
                let (url, log) = losing_upstream(held.to_vec(), end).await;
                let config = if buffering {
                    "codex:\n  stream-bootstrap-buffering: true\n"
                } else {
                    ""
                };
                let proxy = proxy_with(Script::default(), config, |_| two_accounts(&url)).await;
                let mut socket = connect(&proxy).await;
                let first = client_frame("ws-create-first", "gpt-5.5");
                socket.send(Message::text(first.clone())).await.unwrap();
                let got = read(&mut socket, &TERMINAL).await;
                assert_eq!(got.frames, Vec::<String>::new(), "{case}: nothing was delivered");
                assert!(got.closed.is_some(), "{case}: the loss reaches the client");
                let dialed = log.lock().unwrap().clone();
                assert_eq!(dialed.len(), 1, "{case}: no other account was dialed: {dialed:?}");
                assert_eq!(
                    dialed[0].frames,
                    std::slice::from_ref(&first),
                    "{case}: the create was sent once"
                );

                let mut socket = connect(&proxy).await;
                socket.send(Message::text(first.clone())).await.unwrap();
                assert_eq!(
                    read(&mut socket, &TERMINAL).await.frames,
                    [N1_CREATED, N1_DONE],
                    "{case}"
                );
                let dialed = log.lock().unwrap().clone();
                assert_eq!(dialed.len(), 2, "{case}");
                assert_eq!(dialed[1].bearer, "Bearer at-FAKE", "{case}: the account was not cooled");
            }
        }
    }
}

/// N1 W3, WebSocket: under `stream-bootstrap-buffering`, an upstream `response.metadata`
/// before an explicit `server_is_overloaded` rejection is held, so the turn still fails
/// over, and the failed attempt's events, its metadata included, never reach the client.
/// The serving attempt's events, its own `response.metadata` included, reach the client
/// as sent and in order.
#[tokio::test]
async fn bootstrap_buffering_holds_response_metadata_and_releases_it_in_order() {
    const SERVED_METADATA: &str = r#"{"type":"response.metadata","response_id":"resp_n1","headers":{"x-codex-turn-state":"ts-served"},"metadata":{"openai_chatgpt_moderation_metadata":{"prompt":{}}},"future":1.50}"#;
    const DELTA: &str = r#"{"type":"response.output_text.delta","response_id":"resp_n1","delta":"served"}"#;
    let (url, log) = recording_upstream(|dial, _| {
        if dial == 0 {
            (vec![N1_CREATED, N1_IN_PROGRESS, N1_MODERATION, N1_OVERLOADED], false)
        } else {
            (vec![N1_CREATED, SERVED_METADATA, N1_IN_PROGRESS, DELTA, N1_DONE], false)
        }
    })
    .await;
    let proxy = proxy_with(
        Script::default(),
        "codex:\n  stream-bootstrap-buffering: true\n",
        |_| two_accounts(&url),
    )
    .await;
    let mut socket = connect(&proxy).await;
    socket
        .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
        .await
        .unwrap();
    let got = read(&mut socket, &TERMINAL).await;
    assert_eq!(
        got.frames,
        [N1_CREATED, SERVED_METADATA, N1_IN_PROGRESS, DELTA, N1_DONE],
        "the serving attempt's events, exactly"
    );
    assert!(
        !got.frames.iter().any(|f| f == N1_MODERATION),
        "no failed-attempt metadata"
    );
    assert_eq!(got.closed, None);
    assert_eq!(log.lock().unwrap().len(), 2, "the overload failed over");
}

/// The 401 `error` event a refused frame gets: no key in it.
fn assert_refused(frame: &str, message: &str) {
    let event: serde_json::Value = serde_json::from_str(frame).unwrap();
    assert_eq!(event["type"], "error", "{frame}");
    assert_eq!(event["status"], 401, "{frame}");
    assert_eq!(event["error"]["message"], message, "{frame}");
    for key in ["client-key", "other-key"] {
        assert!(!frame.contains(key), "{frame}");
    }
}

/// The proxy's config with `keys` as the client keys.
fn keyed(proxy: &Proxy, keys: &str, extra: &str) -> Config {
    Config::parse(&format!(
        "auth-dir: {}\naccess:\n  api-keys: [{keys}]\n{extra}",
        proxy._auth_dir.0.display()
    ))
    .unwrap()
}

/// N1 W2: a client key removed from `api-keys` stops the next create on a connection it
/// opened, on the reused upstream socket of an ordinary connection and on a steering
/// connection's duplex stream alike. The refused create reaches no upstream, the client
/// gets a 401 `error` event that names no key, and the connection stays open: once the
/// key is accepted again, the next create goes to the same socket as sent. An unrelated
/// config change (another key added, the keys reordered) changes nothing.
#[tokio::test]
async fn a_revoked_client_key_admits_no_later_send_on_a_reused_socket() {
    for steering in [false, true] {
        let case = format!("steering={steering}");
        let extra = if steering { STEERING } else { "" };
        let proxy = proxy(
            Profile::Chatgpt,
            Script {
                ws: replies(&["sse-completed", "sse-completed", "sse-completed"]),
                ..Script::default()
            },
            extra,
        )
        .await;
        let mut socket = connect(&proxy).await;
        let create = client_frame("ws-create-first", "gpt-5.5");
        socket.send(Message::text(create.clone())).await.unwrap();
        assert_eq!(
            read(&mut socket, &TERMINAL).await.frames,
            events("sse-completed"),
            "{case}"
        );

        assert!(proxy.rt.publish_config(keyed(&proxy, "extra-key, client-key", extra)));
        let continuation = client_frame("ws-create-continuation", "gpt-5.5");
        socket.send(Message::text(continuation.clone())).await.unwrap();
        assert_eq!(
            read(&mut socket, &TERMINAL).await.frames,
            events("sse-completed"),
            "{case}: an unrelated change keeps the connection's authority"
        );

        assert!(proxy.rt.publish_config(keyed(&proxy, "other-key", extra)));
        let refused = client_frame("ws-create-first", "gpt-5.5").replacen("List the files.", "refused", 1);
        socket.send(Message::text(refused)).await.unwrap();
        let got = read(&mut socket, &["error"]).await;
        assert_eq!(got.frames.len(), 1, "{case}: {got:?}");
        assert_refused(&got.frames[0], "Invalid API key");
        assert_eq!(got.closed, None, "{case}: the connection stays open");
        assert_eq!(
            upstream_frames(&proxy, 0),
            [create.clone(), continuation.clone()],
            "{case}"
        );

        assert!(proxy.rt.publish_config(keyed(&proxy, "client-key", extra)));
        let again = client_frame("ws-create-first", "gpt-5.5");
        socket.send(Message::text(again.clone())).await.unwrap();
        assert_eq!(
            read(&mut socket, &TERMINAL).await.frames,
            events("sse-completed"),
            "{case}"
        );
        assert_eq!(proxy.upstream.sockets().len(), 1, "{case}: the same socket");
        assert_eq!(upstream_frames(&proxy, 0), [create, continuation, again], "{case}");
    }
}

/// A loopback upstream for the W2 barrier tests: its socket answers the first frame with
/// [`N1_CREATED`], waits for `release`, then sends [`N1_DONE`]; it answers later frames
/// with both at once. Records every frame.
async fn barrier_upstream(release: Arc<tokio::sync::Notify>) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    use axum::extract::ws::{Message as Upstream, WebSocketUpgrade};
    let frames = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let app = {
        let frames = frames.clone();
        axum::Router::new().route(
            "/responses",
            axum::routing::get(move |upgrade: WebSocketUpgrade| {
                let (frames, release) = (frames.clone(), release.clone());
                async move {
                    upgrade.on_upgrade(move |mut socket| async move {
                        let mut first = true;
                        while let Some(Ok(message)) = socket.recv().await {
                            let Upstream::Text(text) = message else { continue };
                            frames.lock().unwrap().push(text.as_str().to_owned());
                            let _ = socket.send(Upstream::Text(N1_CREATED.into())).await;
                            if std::mem::take(&mut first) {
                                // Frames the proxy forwards meanwhile are still recorded.
                                let wait = release.notified();
                                tokio::pin!(wait);
                                loop {
                                    tokio::select! {
                                        () = &mut wait => break,
                                        next = socket.recv() => match next {
                                            Some(Ok(Upstream::Text(text))) => {
                                                frames.lock().unwrap().push(text.as_str().to_owned());
                                            }
                                            Some(Ok(_)) => {}
                                            _ => return,
                                        },
                                    }
                                }
                            }
                            let _ = socket.send(Upstream::Text(N1_DONE.into())).await;
                        }
                    })
                }
            }),
        )
    };
    (format!("http://{}", serve(app).await), frames)
}

/// N1 W2, queued work: a create the client sent while its previous turn ran waits in the
/// connection's queue. The key is revoked before that turn ends; the queued create is
/// then refused when its turn would send it, and never reaches the upstream.
/// Deterministic: the upstream holds the first turn open until the revocation is
/// published.
#[tokio::test]
async fn a_create_queued_before_revocation_is_refused_when_it_would_be_sent() {
    let release = Arc::new(tokio::sync::Notify::new());
    let (url, frames) = barrier_upstream(release.clone()).await;
    let proxy = proxy_with(Script::default(), "", |_| {
        vec![credential(Profile::Chatgpt, &url, true)]
    })
    .await;
    let mut socket = connect(&proxy).await;
    let first = client_frame("ws-create-first", "gpt-5.5");
    socket.send(Message::text(first.clone())).await.unwrap();
    assert_eq!(read(&mut socket, &["response.created"]).await.frames, [N1_CREATED]);
    socket
        .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
        .await
        .unwrap();
    assert!(proxy.rt.publish_config(keyed(&proxy, "other-key", "")));
    release.notify_one();
    let got = read(&mut socket, &["error"]).await;
    assert_eq!(got.frames[0], N1_DONE, "the running turn ends as upstream sent it");
    assert_eq!(got.frames.len(), 2, "{got:?}");
    assert_refused(&got.frames[1], "Invalid API key");
    assert_eq!(
        *frames.lock().unwrap(),
        [first],
        "only the first create reached upstream"
    );
}

/// N1 W2, control frames: a `response.interrupt` sent after the key was revoked reaches
/// no upstream (and would end no local turn); the client gets the 401 event, and the
/// running response goes on to its end.
#[tokio::test]
async fn a_revoked_client_key_cannot_interrupt() {
    let release = Arc::new(tokio::sync::Notify::new());
    let (url, frames) = barrier_upstream(release.clone()).await;
    let proxy = proxy_with(Script::default(), "", |_| {
        vec![credential(Profile::Chatgpt, &url, true)]
    })
    .await;
    let mut socket = connect(&proxy).await;
    let first = client_frame("ws-create-first", "gpt-5.5");
    socket.send(Message::text(first.clone())).await.unwrap();
    assert_eq!(read(&mut socket, &["response.created"]).await.frames, [N1_CREATED]);
    assert!(proxy.rt.publish_config(keyed(&proxy, "other-key", "")));
    let interrupt = r#"{"type":"response.interrupt","response_id":"resp_n1","mode":"discard_partial_items"}"#;
    socket.send(Message::text(interrupt)).await.unwrap();
    let got = read(&mut socket, &["error"]).await;
    assert_eq!(got.frames.len(), 1, "{got:?}");
    assert_refused(&got.frames[0], "Invalid API key");
    release.notify_one();
    assert_eq!(read(&mut socket, &TERMINAL).await.frames, [N1_DONE]);
    assert_eq!(*frames.lock().unwrap(), [first], "the interrupt never reached upstream");
}

/// N1 W2, bound principal: a connection opened with one client key never acts as another
/// key it also presents. When the bound key is revoked while the other stays accepted,
/// the next create is refused as a changed credential instead of moving silently to the
/// other key, as Go refuses a duplex turn (`3e7a21e2`).
#[tokio::test]
async fn another_accepted_key_never_replaces_the_bound_principal() {
    let proxy = proxy(
        Profile::Chatgpt,
        Script {
            ws: replies(&["sse-completed", "sse-completed"]),
            ..Script::default()
        },
        "",
    )
    .await;
    assert!(proxy.rt.publish_config(keyed(&proxy, "client-key, other-key", "")));
    let mut socket = wreq::Client::new()
        .websocket(format!("ws://{}/v1/responses", proxy.addr))
        .header("authorization", "Bearer client-key")
        .header("x-api-key", "other-key")
        .header("user-agent", CODEX_UA)
        .header("originator", "codex_cli_rs")
        .send()
        .await
        .unwrap()
        .into_websocket()
        .await
        .unwrap();
    let create = client_frame("ws-create-first", "gpt-5.5");
    socket.send(Message::text(create.clone())).await.unwrap();
    assert_eq!(read(&mut socket, &TERMINAL).await.frames, events("sse-completed"));
    assert!(proxy.rt.publish_config(keyed(&proxy, "other-key", "")));
    socket.send(Message::text(create.clone())).await.unwrap();
    let got = read(&mut socket, &["error"]).await;
    assert_eq!(got.frames.len(), 1, "{got:?}");
    assert_refused(
        &got.frames[0],
        "client credential changed during the websocket stream; reconnect",
    );
    assert_eq!(upstream_frames(&proxy, 0), [create]);
}

// N1 (#111) review: HTTP send certainty, client authority on every HTTP attempt, and a
// real bootstrap deadline. Codex credentials without `websockets` serve WebSocket turns
// over HTTP (CONTRACT "WebSocket HTTP fallback"); HTTP clients take the native HTTP route.

/// Two accounts on `url` without `websockets`: WebSocket turns run over HTTP.
fn two_http_accounts(url: &str) -> Vec<Credential> {
    two_accounts(url)
        .into_iter()
        .map(|mut c| {
            c.attributes.remove("websockets");
            c
        })
        .collect()
}

/// A raw HTTP/1.1 upstream: the first request is read whole, then answered with nothing
/// (`held` false) or with headers and a `response.created` event but a body cut short
/// (`held` true), and the connection is closed. Later requests get [`N1_DONE`]. Records
/// each request's `Authorization` line.
async fn losing_http_upstream(held: bool) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut conn, _)) = listener.accept().await {
            let log = log.clone();
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                let (start, length) = loop {
                    let mut buf = [0; 4096];
                    let n = conn.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(at) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&bytes[..at]).into_owned();
                        let length = head
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        break (at + 4, length);
                    }
                };
                while bytes.len() < start + length {
                    let mut buf = [0; 4096];
                    let n = conn.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buf[..n]);
                }
                let head = String::from_utf8_lossy(&bytes[..start]).into_owned();
                let bearer = head
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                    .unwrap_or_default()
                    .to_owned();
                let first = {
                    let mut log = log.lock().unwrap();
                    log.push(bearer);
                    log.len() == 1
                };
                let (event, cut) = if first {
                    (format!("data: {N1_CREATED}\n\n"), 999)
                } else {
                    (format!("data: {N1_DONE}\n\n"), 0)
                };
                if !first || held {
                    let answer = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{event}",
                        event.len() + cut
                    );
                    let _ = conn.write_all(answer.as_bytes()).await;
                    let _ = conn.flush().await;
                }
                let _ = conn.shutdown().await;
            });
        }
    });
    (url, seen)
}

/// N1 (#111) review, HTTP: once a request was sent, a lost connection is uncertain: the
/// upstream may have read it. Whether nothing came back or the answer broke off after a
/// held `response.created`, the request is never sent to another account, on the
/// WebSocket HTTP fallback and on the native HTTP route alike, with bootstrap buffering
/// on. The WebSocket client's connection closes, as for a lost upstream socket; the HTTP
/// client gets an error and no events. The account is not cooled.
#[tokio::test]
async fn an_uncertain_http_loss_is_never_replayed_on_another_account() {
    for websocket in [true, false] {
        for held in [false, true] {
            let case = format!("websocket={websocket} held={held}");
            let (url, seen) = losing_http_upstream(held).await;
            let proxy = proxy_with(
                Script::default(),
                "codex:\n  stream-bootstrap-buffering: true\n",
                |_| two_http_accounts(&url),
            )
            .await;
            if websocket {
                let mut socket = connect(&proxy).await;
                socket
                    .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
                    .await
                    .unwrap();
                let got = read(&mut socket, &TERMINAL).await;
                assert!(
                    !got.frames.iter().any(|f| f.contains("response.completed")),
                    "{case}: {got:?}"
                );
                assert!(got.closed.is_some(), "{case}: the client connection closes: {got:?}");
            } else {
                let answer = wreq::Client::new()
                    .post(format!("http://{}/v1/responses", proxy.addr))
                    .bearer_auth("client-key")
                    .header("content-type", "application/json")
                    .body(client_frame("http-request-full", "gpt-5.5"))
                    .send()
                    .await
                    .unwrap();
                let status = answer.status().as_u16();
                let body = answer.text().await.unwrap_or_default();
                assert!(!body.contains("response.completed"), "{case}: {status} {body}");
            }
            let seen = seen.lock().unwrap().clone();
            assert_eq!(seen.len(), 1, "{case}: sent once, to one account: {seen:?}");
            assert!(seen[0].ends_with("at-FAKE"), "{case}: {seen:?}");

            // Not cooled: the next request goes to the same account.
            let mut socket = connect(&proxy).await;
            socket
                .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
                .await
                .unwrap();
            let _ = read(&mut socket, &TERMINAL).await;
        }
    }
}

/// N1 (#111) review, HTTP controls: a request that provably never left (the first
/// account's connection is refused) and an explicit upstream rejection (a 503) still fail
/// over to the next account, which serves the turn.
#[tokio::test]
async fn provably_unsent_and_rejected_http_requests_still_fail_over() {
    let served = format!("data: {N1_CREATED}\n\ndata: {N1_DONE}\n\n");
    let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let app = {
        let (seen, served) = (seen.clone(), served.clone());
        axum::Router::new().route(
            "/responses",
            axum::routing::post(move |headers: axum::http::HeaderMap, _: Bytes| {
                let (seen, served) = (seen.clone(), served.clone());
                async move {
                    let bearer = headers["authorization"].to_str().unwrap().to_owned();
                    seen.lock().unwrap().push(bearer.clone());
                    if bearer.ends_with("at-FAKE") {
                        (
                            axum::http::StatusCode::SERVICE_UNAVAILABLE,
                            [("content-type", "application/json")],
                            r#"{"error":{"message":"overloaded","type":"server_error"}}"#.to_owned(),
                        )
                    } else {
                        (
                            axum::http::StatusCode::OK,
                            [("content-type", "text/event-stream")],
                            served,
                        )
                    }
                }
            }),
        )
    };
    let live = format!("http://{}", serve(app).await);
    let refused = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    for (case, first_url) in [("refused", refused.as_str()), ("rejected", live.as_str())] {
        seen.lock().unwrap().clear();
        let proxy = proxy_with(Script::default(), "", |_| {
            let mut accounts = two_http_accounts(&live);
            accounts[0].attributes.insert("base_url".into(), first_url.into());
            accounts
        })
        .await;
        let mut socket = connect(&proxy).await;
        socket
            .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
            .await
            .unwrap();
        let got = read(&mut socket, &TERMINAL).await;
        assert_eq!(
            got.frames,
            [N1_CREATED, N1_DONE],
            "{case}: the next account serves: {got:?}"
        );
        let seen = seen.lock().unwrap().clone();
        assert_eq!(
            seen.last().map(String::as_str),
            Some("Bearer at-SECOND"),
            "{case}: {seen:?}"
        );
    }
}

/// N1 (#111) review, W2 on HTTP: every HTTP attempt of a WebSocket turn is sent under the
/// key the connection was opened with. Revoked while the first account's attempt is
/// held open (a deterministic barrier), the turn's failover attempt on the next account
/// is refused: nothing reaches that account, and the client gets the 401 refusal.
#[tokio::test]
async fn a_revocation_during_an_http_attempt_stops_the_failover_attempt() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let (arrived, release) = (
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
    );
    let app = {
        let (seen, arrived, release) = (seen.clone(), arrived.clone(), release.clone());
        axum::Router::new().route(
            "/responses",
            axum::routing::post(move |headers: axum::http::HeaderMap, _: Bytes| {
                let (seen, arrived, release) = (seen.clone(), arrived.clone(), release.clone());
                async move {
                    let first = {
                        let mut seen = seen.lock().unwrap();
                        seen.push(headers["authorization"].to_str().unwrap().to_owned());
                        seen.len() == 1
                    };
                    if first {
                        arrived.notify_one();
                        release.notified().await;
                        (
                            axum::http::StatusCode::SERVICE_UNAVAILABLE,
                            [("content-type", "application/json")],
                            r#"{"error":{"message":"overloaded"}}"#.to_owned(),
                        )
                    } else {
                        (
                            axum::http::StatusCode::OK,
                            [("content-type", "text/event-stream")],
                            format!("data: {N1_DONE}\n\n"),
                        )
                    }
                }
            }),
        )
    };
    let url = format!("http://{}", serve(app).await);
    let proxy = proxy_with(Script::default(), "", |_| two_http_accounts(&url)).await;
    let mut socket = connect(&proxy).await;
    socket
        .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), arrived.notified())
        .await
        .unwrap();
    assert!(proxy.rt.publish_config(keyed(&proxy, "other-key", "")));
    release.notify_one();
    let got = read(&mut socket, &["error", "response.completed"]).await;
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "no attempt after the revocation: {got:?}"
    );
    assert_eq!(got.frames.len(), 1, "{got:?}");
    assert_refused(&got.frames[0], "Invalid API key");
}

/// N1 (#111) review, W3: the bootstrap window closes at its deadline, not when the next
/// message arrives. Held `response.metadata` reaches the client unchanged once the
/// configured 100 ms pass, while the upstream still holds the rest of the response; the
/// rest follows as sent. Deterministic: the upstream sends the rest only after the client
/// has the metadata.
#[tokio::test]
async fn held_metadata_is_released_at_the_bootstrap_deadline() {
    use axum::extract::ws::{Message as Upstream, WebSocketUpgrade};
    let release = Arc::new(tokio::sync::Notify::new());
    let url = {
        let release = release.clone();
        format!(
            "http://{}",
            serve(axum::Router::new().route(
                "/responses",
                axum::routing::get(move |upgrade: WebSocketUpgrade| {
                    let release = release.clone();
                    async move {
                        upgrade.on_upgrade(move |mut socket| async move {
                            let _ = socket.recv().await;
                            let _ = socket.send(Upstream::Text(N1_CREATED.into())).await;
                            let _ = socket.send(Upstream::Text(N1_MODERATION.into())).await;
                            release.notified().await;
                            let _ = socket.send(Upstream::Text(N1_DONE.into())).await;
                            while let Some(Ok(_)) = socket.recv().await {}
                        })
                    }
                }),
            ))
            .await
        )
    };
    let proxy = proxy_with(
        Script::default(),
        "codex:\n  stream-bootstrap-buffering: true\n  stream-bootstrap-timeout: 100ms\n",
        |_| vec![credential(Profile::Chatgpt, &url, true)],
    )
    .await;
    let mut socket = connect(&proxy).await;
    socket
        .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
        .await
        .unwrap();
    let early = tokio::time::timeout(Duration::from_secs(3), read(&mut socket, &["response.metadata"]))
        .await
        .expect("the held events are released at the deadline, before the upstream sends more");
    assert_eq!(early.frames, [N1_CREATED, N1_MODERATION], "unchanged and in order");
    release.notify_one();
    assert_eq!(read(&mut socket, &TERMINAL).await.frames, [N1_DONE]);
}

/// N1 (#111) review, W3 bounds: metadata never holds a response past the bootstrap
/// window's frame bound (48 messages). 60 metadata events and the completion all reach
/// the client, unchanged and in order, and nothing is lost when the window closes on the
/// count.
#[tokio::test]
async fn metadata_beyond_the_bootstrap_bound_passes_in_order() {
    let metadata: Vec<String> = (0..60)
        .map(|i| {
            format!(r#"{{"type":"response.metadata","response_id":"resp_n1","sequence_number":{i},"metadata":{{}}}}"#)
        })
        .collect();
    let mut frames = vec![N1_CREATED.to_owned()];
    frames.extend(metadata.iter().cloned());
    frames.push(N1_DONE.to_owned());
    let script: Vec<Bytes> = frames.iter().cloned().map(Bytes::from).collect();
    let proxy = proxy_with(
        Script {
            ws: vec![script],
            ..Script::default()
        },
        "codex:\n  stream-bootstrap-buffering: true\n",
        |url| vec![credential(Profile::Chatgpt, url, true)],
    )
    .await;
    let mut socket = connect(&proxy).await;
    socket
        .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
        .await
        .unwrap();
    assert_eq!(read(&mut socket, &TERMINAL).await.frames, frames);
}

/// Two Codex credentials on `url` with the `websockets` attribute as given: the first
/// (`at-FAKE`) preferred by its priority, the second (`at-FAKE-second`) the failover.
fn preferred_pair(url: &str, websockets: bool) -> Vec<Credential> {
    let mut first = credential(Profile::Chatgpt, url, websockets);
    first.attributes.insert("priority".into(), "9".into());
    let mut second = credential(Profile::Chatgpt, url, websockets);
    second.id = "codex-second.json".into();
    second.metadata.insert("access_token".into(), "at-FAKE-second".into());
    second.attributes.insert("priority".into(), "8".into());
    vec![first, second]
}

/// FP2 (#116) on the native WebSocket route, with a second account available. First-party
/// Codex at research main `5c3ff1dabf644169f4bd74b215719f381e4fdb3f` never retries these
/// rejections, and it reads them on two surfaces the proxy keeps apart:
///
/// - a wrapped status frame is the HTTP error of its status (`responses_websocket.rs:755-806`):
///   the `ws-error-terminal-rejections` fixture, a 403 workspace frame by `error.code`
///   and by `x-error-json` in its `headers`, and the 400 `context_length_exceeded`
///   control of `ws-error-context-length-status`;
/// - every other `error` frame, and every `response.failed`, is read with no status at
///   all (`responses_websocket.rs:906-925`, `sse/responses.rs:379-383`): the
///   `sse-terminal-rejections` fixture's frames, and notices whose status maps to no
///   error (`200`, `1000`) or that the wrapped struct cannot read (`"503"`).
///
/// Each ends the turn on the selected account: one upstream frame on one socket (or one
/// HTTP request when the turn runs over HTTP), no cooldown, and exactly one failed usage
/// record. A rejection classified as 403 describes the account's workspace, so its frame
/// is withheld (#6): the client gets no frame and the connection closes. Any other one
/// reaches the client as sent, unknown fields included.
#[tokio::test]
async fn first_party_terminal_rejections_never_fail_over_on_websocket_turns() {
    let encoded = {
        use base64::Engine;
        let provider = text(fixture("http-error-workspace-provider").bytes());
        base64::engine::general_purpose::STANDARD.encode(provider.trim())
    };
    let wrapped = fixture("ws-error-terminal-rejections").frames();
    let wrapped: Vec<String> = wrapped.into_iter().map(text).collect();
    let mut cases: Vec<(String, bool, bool)> = vec![
        (wrapped[0].clone(), true, true),
        (wrapped[1].clone(), false, true),
        (
            r#"{"type":"error","status":403,"error":{"code":"invalid_workspace_selected"}}"#.to_owned(),
            false,
            true,
        ),
        (
            format!(r#"{{"type":"error","status":403,"headers":{{"x-error-json":"{encoded}"}}}}"#),
            false,
            true,
        ),
        (
            text(fixture("ws-error-context-length-status").frames()[0].clone()),
            true,
            true,
        ),
        (
            r#"{"type":"error","status":200,"error":{"code":"inference_hook_denied","message":"notice"}}"#.to_owned(),
            false,
            true,
        ),
        (
            r#"{"type":"error","status":1000,"error":{"code":"context_length_exceeded","message":"notice"}}"#
                .to_owned(),
            true,
            true,
        ),
        (
            r#"{"type":"error","status":"503","error":{"code":"content_policy_violation","message":"notice"}}"#
                .to_owned(),
            true,
            true,
        ),
    ];
    // The stream frames: the context code is the proxy's existing #5 relay, not FP2.
    for frame in fixture("sse-terminal-rejections").frames().into_iter().map(text) {
        let code = ["error.code", "response.error.code"]
            .iter()
            .map(|p| cpa_common::rawjson::get(&frame, p).str().to_owned())
            .find(|c| !c.is_empty())
            .unwrap();
        if code == "context_length_exceeded" {
            continue;
        }
        let delivered = !matches!(code.as_str(), "inference_hook_denied" | "access_program_not_enabled");
        cases.push((frame, delivered, false));
    }
    assert_eq!(cases.len(), 8 + 10);
    for (frame, delivered, socket_only) in cases {
        for websockets in [true, false] {
            if socket_only && !websockets {
                continue;
            }
            let case = format!("websockets={websockets}: {frame}");
            let script = if websockets {
                Script {
                    ws: vec![vec![Bytes::from(frame.clone())], fixture("sse-completed").frames()],
                    ..Script::default()
                }
            } else {
                Script {
                    http: vec![
                        cpa_server::testing::codex_capture::Reply::sse(format!("data: {frame}\n\n")),
                        cpa_server::testing::codex_capture::Reply::sse(fixture("sse-completed").bytes()),
                    ],
                    ..Script::default()
                }
            };
            let proxy = proxy_with(script, "", |url| preferred_pair(url, websockets)).await;
            proxy.rt.usage_queue().configure(
                true,
                &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
            );
            let mut socket = connect(&proxy).await;
            let create = client_frame("ws-create-first", "gpt-5.5");
            socket.send(Message::text(create.clone())).await.unwrap();
            // Until the close, or another account's completion if the turn failed over.
            let got = read(&mut socket, &["response.completed"]).await;
            if delivered {
                assert_eq!(got.frames, [frame.as_str()], "{case}: the provider's frame");
            } else {
                assert!(
                    got.frames.is_empty(),
                    "{case}: withheld, and no other account's answer: {got:?}"
                );
            }
            assert!(got.closed.is_some(), "{case}: {got:?}");
            if websockets {
                assert_eq!(proxy.upstream.sockets().len(), 1, "{case}: no second socket");
                assert_eq!(
                    proxy.upstream.sockets()[0].headers["authorization"],
                    "Bearer at-FAKE",
                    "{case}"
                );
                assert_eq!(upstream_frames(&proxy, 0), [create], "{case}: sent once");
                assert!(proxy.upstream.http().is_empty(), "{case}");
            } else {
                let sent = proxy.upstream.http();
                assert_eq!(sent.len(), 1, "{case}: one attempt");
                assert_eq!(sent[0].headers["authorization"], "Bearer at-FAKE", "{case}");
                assert!(proxy.upstream.sockets().is_empty(), "{case}");
            }
            for credential in proxy.rt.store().snapshot() {
                assert!(
                    proxy.rt.store().cooldowns(&credential.id).is_empty(),
                    "{case}: {}",
                    credential.id
                );
            }
            let settled = records(&proxy, 1).await;
            assert_eq!(settled[0]["failed"], true, "{case}: {settled:?}");
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert!(
                proxy.rt.usage_queue().pop_oldest(10).is_empty(),
                "{case}: one settlement"
            );
        }
    }
}

/// FP2 (#116): a 403 upgrade refusal naming a workspace denial
/// (`core/tests/suite/websocket_fallback.rs:133-152` at 5c3ff1dabf64, the
/// `http-error-workspace-detail` body; first-party never retries it or falls back to
/// HTTP) ends the turn on the selected account: one upgrade, no other account's socket, no
/// HTTP request, no cooldown, one failed record. Its bytes never reach the client, which
/// gets a silent close (#6). The bare 403 refusal is the control: an account failure that
/// moves to the second account, whose upgrade the script refuses too. `request-retry: 0`
/// keeps both to one round.
#[tokio::test]
async fn a_workspace_upgrade_refusal_stops_on_the_selected_account() {
    use cpa_server::testing::codex_capture::Reply;
    for (body, terminal) in [
        (text(fixture("http-error-workspace-detail").bytes()), true),
        (String::new(), false),
    ] {
        let proxy = proxy_with(
            Script {
                ws_reject: Some(Reply::json(403, body.clone())),
                ..Script::default()
            },
            "routing:\n  retry:\n    request-retry: 0\n",
            |url| preferred_pair(url, true),
        )
        .await;
        proxy.rt.usage_queue().configure(
            true,
            &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
        );
        let mut socket = connect(&proxy).await;
        socket
            .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
            .await
            .unwrap();
        let got = read(&mut socket, &[]).await;
        assert!(got.frames.is_empty(), "{body}: {got:?}");
        assert!(got.closed.is_some(), "{body}: {got:?}");
        let tokens: Vec<_> = proxy
            .upstream
            .sockets()
            .iter()
            .map(|s| s.headers["authorization"].to_str().unwrap().to_owned())
            .collect();
        assert!(proxy.upstream.http().is_empty(), "{body}: no HTTP fallback");
        let cooled = proxy
            .rt
            .store()
            .snapshot()
            .iter()
            .filter(|c| !proxy.rt.store().cooldowns(&c.id).is_empty())
            .count();
        if terminal {
            assert_eq!(
                tokens,
                ["Bearer at-FAKE"],
                "{body}: one upgrade on the selected account"
            );
            assert_eq!(cooled, 0, "{body}");
            let settled = records(&proxy, 1).await;
            assert_eq!(settled[0]["failed"], true, "{body}: {settled:?}");
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert!(
                proxy.rt.usage_queue().pop_oldest(10).is_empty(),
                "{body}: one settlement"
            );
        } else {
            assert_eq!(tokens, ["Bearer at-FAKE", "Bearer at-FAKE-second"], "{body}");
            assert!(cooled > 0, "{body}");
        }
    }
}

/// FP2 (#116): a wrapped `context_length_exceeded` 503 keeps its transient meaning
/// (`responses_websocket.rs:1416-1439` at 5c3ff1dabf64, the fixture's second frame), with
/// or without Anthropic's `invalid_request_error` type. It is not a fabricated
/// context-window failure: the create moves once to the second account before anything
/// was delivered, the client gets that account's answer and never the 503 frame, and only
/// the first account is cooled.
#[tokio::test]
async fn a_context_labelled_503_fails_over_on_websocket_turns() {
    let unavailable = text(fixture("ws-error-context-length-status").frames()[1].clone());
    for frame in [
        unavailable,
        r#"{"type":"error","status":503,"error":{"type":"invalid_request_error","code":"context_length_exceeded","message":null}}"#.to_owned(),
    ] {
        let proxy = proxy_with(
            Script {
                ws: vec![vec![Bytes::from(frame.clone())], fixture("sse-completed").frames()],
                ..Script::default()
            },
            "",
            |url| preferred_pair(url, true),
        )
        .await;
        let mut socket = connect(&proxy).await;
        let create = client_frame("ws-create-first", "gpt-5.5");
        socket.send(Message::text(create.clone())).await.unwrap();
        let got = read(&mut socket, &TERMINAL).await;
        assert_eq!(got.frames, events("sse-completed"), "{frame}");
        let tokens: Vec<_> = proxy
            .upstream
            .sockets()
            .iter()
            .map(|s| s.headers["authorization"].to_str().unwrap().to_owned())
            .collect();
        assert_eq!(tokens, ["Bearer at-FAKE", "Bearer at-FAKE-second"], "{frame}");
        assert_eq!(upstream_frames(&proxy, 0), std::slice::from_ref(&create), "{frame}");
        assert_eq!(upstream_frames(&proxy, 1), [create], "{frame}");
        let cooled: Vec<_> = proxy
            .rt
            .store()
            .snapshot()
            .iter()
            .filter(|c| !proxy.rt.store().cooldowns(&c.id).is_empty())
            .map(|c| c.id.clone())
            .collect();
        assert_eq!(cooled.len(), 1, "{frame}: {cooled:?}");
        assert_ne!(cooled[0], "codex-second.json", "{frame}: the first account is cooled");
    }
}

/// FP2 (#116) review finding 1 on the bootstrap path: with `stream-bootstrap-buffering`,
/// an in-stream `inference_hook_denied` whose message reads as Go's model-capacity
/// overload is still the first-party terminal rejection, never a bootstrap overload: the
/// turn stops on the selected account with no second socket and no cooldown, its 403
/// frame is withheld (#6), and one failed record is kept. A plain capacity failure with
/// no first-party code is the control: it fails over to the second account.
#[tokio::test]
async fn a_capacity_message_never_fails_a_terminal_rejection_over_at_bootstrap() {
    let buffering = "codex:\n  stream-bootstrap-buffering: true\n";
    let terminal = r#"{"type":"response.failed","response":{"id":"resp_fp2","status":"failed","error":{"code":"inference_hook_denied","message":"model is at capacity"}}}"#;
    let control = r#"{"type":"response.failed","response":{"id":"resp_fp2","status":"failed","error":{"code":"server_error","message":"model is at capacity"}}}"#;
    for (frame, stops) in [(terminal, true), (control, false)] {
        let proxy = proxy_with(
            Script {
                ws: vec![vec![Bytes::from(frame)], fixture("sse-completed").frames()],
                ..Script::default()
            },
            buffering,
            |url| preferred_pair(url, true),
        )
        .await;
        proxy.rt.usage_queue().configure(
            true,
            &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
        );
        let mut socket = connect(&proxy).await;
        socket
            .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
            .await
            .unwrap();
        let got = read(&mut socket, &["response.completed"]).await;
        let tokens: Vec<_> = proxy
            .upstream
            .sockets()
            .iter()
            .map(|s| s.headers["authorization"].to_str().unwrap().to_owned())
            .collect();
        let cooled = proxy
            .rt
            .store()
            .snapshot()
            .iter()
            .filter(|c| !proxy.rt.store().cooldowns(&c.id).is_empty())
            .count();
        if stops {
            assert!(
                got.frames.is_empty(),
                "{frame}: withheld, no other account's answer: {got:?}"
            );
            assert!(got.closed.is_some(), "{frame}: {got:?}");
            assert_eq!(
                tokens,
                ["Bearer at-FAKE"],
                "{frame}: one socket on the selected account"
            );
            assert_eq!(cooled, 0, "{frame}");
            let settled = records(&proxy, 1).await;
            assert_eq!(settled[0]["failed"], true, "{frame}: {settled:?}");
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert!(
                proxy.rt.usage_queue().pop_oldest(10).is_empty(),
                "{frame}: one settlement"
            );
        } else {
            assert_eq!(got.frames, events("sse-completed"), "{frame}");
            assert_eq!(tokens, ["Bearer at-FAKE", "Bearer at-FAKE-second"], "{frame}");
            assert!(cooled > 0, "{frame}");
        }
    }
}

/// FP2 (#116) re-review finding 1 (#6): an in-stream `content_policy_violation` that FP2
/// classifies with 400 but whose own error names an account status (an inner 403) and the
/// account is still withheld from a native client, on a socket and on a turn served over
/// HTTP. It stops on the selected account: one upstream frame or request, no second
/// account, no cooldown, one failed record. The same rejection without an account status
/// is the control: delivered as sent.
#[tokio::test]
async fn an_inner_account_status_keeps_a_terminal_rejection_withheld() {
    let leaky = r#"{"type":"response.failed","sequence_number":2,"response":{"id":"resp_fp2","status":"failed","error":{"code":"content_policy_violation","status":403,"message":"workspace org-PRIVATE denied"}}}"#;
    let plain = r#"{"type":"response.failed","sequence_number":2,"response":{"id":"resp_fp2","status":"failed","error":{"code":"content_policy_violation","message":"rejected"}}}"#;
    for (frame, withheld) in [(leaky, true), (plain, false)] {
        for websockets in [true, false] {
            let case = format!("websockets={websockets}: {frame}");
            let script = if websockets {
                Script {
                    ws: vec![vec![Bytes::from(frame)], fixture("sse-completed").frames()],
                    ..Script::default()
                }
            } else {
                Script {
                    http: vec![
                        cpa_server::testing::codex_capture::Reply::sse(format!("data: {frame}\n\n")),
                        cpa_server::testing::codex_capture::Reply::sse(fixture("sse-completed").bytes()),
                    ],
                    ..Script::default()
                }
            };
            let proxy = proxy_with(script, "", |url| preferred_pair(url, websockets)).await;
            proxy.rt.usage_queue().configure(
                true,
                &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
            );
            let mut socket = connect(&proxy).await;
            socket
                .send(Message::text(client_frame("ws-create-first", "gpt-5.5")))
                .await
                .unwrap();
            let got = read(&mut socket, &["response.completed"]).await;
            assert!(!got.frames.iter().any(|f| f.contains("PRIVATE")), "{case}: {got:?}");
            if withheld {
                assert!(got.frames.is_empty(), "{case}: withheld: {got:?}");
            } else {
                assert_eq!(got.frames, [frame], "{case}: delivered as sent");
            }
            assert!(got.closed.is_some(), "{case}: {got:?}");
            let attempts = proxy.upstream.sockets().len() + proxy.upstream.http().len();
            assert_eq!(attempts, 1, "{case}: one attempt on the selected account");
            for credential in proxy.rt.store().snapshot() {
                assert!(
                    proxy.rt.store().cooldowns(&credential.id).is_empty(),
                    "{case}: {}",
                    credential.id
                );
            }
            let settled = records(&proxy, 1).await;
            assert_eq!(settled[0]["failed"], true, "{case}: {settled:?}");
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert!(
                proxy.rt.usage_queue().pop_oldest(10).is_empty(),
                "{case}: one settlement"
            );
        }
    }
}
