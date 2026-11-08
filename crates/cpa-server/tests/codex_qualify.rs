//! #29: the cross-path qualification matrix for the native Codex routes, through the real
//! router against capturing loopback upstreams. Each pinned contract request
//! (`cpa_exec::codex_contract`: first-party source and the unknown-fields probe) plus a
//! large tool-schema body goes over every transport (HTTP streamed, HTTP buffered, a
//! WebSocket turn on an upstream socket and a WebSocket turn over HTTP) to every profile a
//! loopback can stand for (`chatgpt`, `compatible` and Sign in with ChatGPT; the `api-key`
//! profile is `compatible` on `api.openai.com`, which a loopback cannot be). Compaction,
//! an alias, an interrupted response and a truncated stream complete the matrix.
//!
//! Expected bytes come from the fixtures and the client's own input, never from proxy
//! output. A permitted transformation (`docs/codex/CONTRACT.md`) is applied to the
//! client's bytes explicitly where it applies: `stream: true` for a buffered `chatgpt`
//! client and for a WebSocket turn over HTTP, the `type` tag removed for a WebSocket turn
//! over HTTP, the routed `model` for an alias. Every upstream request also carries only
//! the credential's own token: no client key or client cookie crosses the proxy.
//!
//! The per-feature tests stay with the PRs that own them; docs/codex/QUALIFICATION.md
//! maps each acceptance item to its tests. Loopback upstreams, fake tokens and a private
//! `auth-dir` only.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::http::HeaderMap;
use cpa_common::codex_body::{CodexBody, Kind};
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_exec::Executors;
use cpa_exec::codex_contract::fixture;
use cpa_server::testing::codex_capture::{CaptureUpstream, Reply, Script};
use futures_util::StreamExt;
use wreq::ws::message::Message;

const CLIENT_KEY: &str = "client-key-QUALIFY";
const CLIENT_COOKIE: &str = "sid=client-cookie-QUALIFY";
const CODEX_UA: &str = "codex_cli_rs/0.160.0 (Linux; x86_64) xterm-256color";

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
    Chatgpt,
    Compatible,
    Siwc,
}

const PROFILES: [Profile; 3] = [Profile::Chatgpt, Profile::Compatible, Profile::Siwc];

impl Profile {
    /// The upstream `Authorization` the credential's own token gives.
    fn bearer(self) -> &'static str {
        match self {
            Profile::Chatgpt => "Bearer at-FAKE",
            Profile::Compatible => "Bearer key-FAKE",
            Profile::Siwc => "Bearer siwc-at-FAKE",
        }
    }
}

fn credential(profile: Profile, base_url: &str, websockets: bool) -> Credential {
    let mut metadata = serde_json::json!({
        "type": "codex",
        "access_token": "at-FAKE",
        "account_id": "acct-FAKE",
        "expired": "2099-01-01T00:00:00Z",
    });
    if profile == Profile::Siwc {
        metadata["use_siwc"] = true.into();
        metadata["siwc"] = serde_json::json!({"access_token": "siwc-at-FAKE", "expired": "2099-01-01T00:00:00Z"});
    }
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

async fn proxy(script: Script, extra: &str, credentials: impl FnOnce(&str) -> Vec<Credential>) -> Proxy {
    let upstream = CaptureUpstream::start(script).await;
    let auth_dir = AuthDir::new();
    let cfg = Config::parse(&format!(
        "auth-dir: {}\naccess:\n  api-keys: [{CLIENT_KEY}]\n{}",
        auth_dir.0.display(),
        extra.replace("{upstream}", &upstream.base_url)
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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move { axum::serve(listener, cpa_server::router(rt)).await.unwrap() });
    Proxy {
        addr,
        upstream,
        _auth_dir: auth_dir,
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// A fixture body with the matrix's model in place of `gpt-test`: the client's bytes.
fn client_body(name: &str) -> String {
    text(&fixture(name).bytes()).replacen(r#""model":"gpt-test""#, r#""model":"gpt-5.5""#, 1)
}

/// A large tool-schema request: 240 function tools whose schemas hold 30 properties each,
/// with escapes, `null`, `false`, raw number spellings and unknown keywords that any
/// re-serialization would change. About 1 MB.
fn large_schema_body() -> String {
    let mut tools = Vec::with_capacity(240);
    for t in 0..240 {
        let mut properties = Vec::with_capacity(30);
        for p in 0..30 {
            properties.push(format!(
                r#""field_{t}_{p}" : {{"type":"string","description":"caf\u00e9 \"quoted\" {p}","default":null,"x-future":false,"maxLength":1.50e2,"enum":["a","b",null]}}"#
            ));
        }
        tools.push(format!(
            r#"{{"type":"function","name":"tool_{t}","description":"Tool {t}.","strict":false,"parameters":{{"type":"object","additionalProperties":false,"properties":{{{}}},"required":[],"x-future-keyword":{{"n":12345678901234567890}}}}}}"#,
            properties.join(",")
        ));
    }
    format!(
        r#"{{"model":"gpt-5.5","stream":true,"input":[{{"type":"message","role":"user","content":[{{"type":"input_text","text":"use the tools"}}]}}],"tools":[{}],"tool_choice":"auto","parallel_tool_calls":false,"reasoning":null,"store":false,"include":["reasoning.encrypted_content"],"prompt_cache_key":"pck-qualify","prompt_cache_options":{{"ttl":"30m"}}}}"#,
        tools.join(",")
    )
}

/// The native request bodies of the matrix.
fn bodies() -> Vec<(&'static str, String)> {
    vec![
        ("http-request-full", client_body("http-request-full")),
        ("http-request-lite", client_body("http-request-lite")),
        ("http-request-minimal", client_body("http-request-minimal")),
        // First-party compaction v2: an ordinary streamed request ending in a
        // `compaction_trigger` item (core/src/compact_remote_v2_attempt.rs:228).
        ("http-request-compaction", client_body("http-request-compaction")),
        (
            "probe-request-unknown-fields",
            client_body("probe-request-unknown-fields"),
        ),
        ("large-schema", large_schema_body()),
    ]
}

/// `body` with top-level `key` set to `value`: how the matrix builds a client variant or
/// applies a permitted transformation to the client's bytes.
fn with(body: &str, key: &str, value: &str) -> String {
    text(&CodexBody::parse(body.as_bytes()).unwrap().with_member(key, value))
}

fn without(body: &str, key: &str) -> String {
    text(&CodexBody::parse(body.as_bytes()).unwrap().without_member(key))
}

fn is_true(body: &str, key: &str) -> bool {
    CodexBody::parse(body.as_bytes())
        .unwrap()
        .get(key)
        .is_some_and(|m| m.kind == Kind::True)
}

/// The headers of a client that authenticates to the proxy with its own key and cookie.
fn client_request(builder: wreq::RequestBuilder) -> wreq::RequestBuilder {
    builder
        .header("authorization", format!("Bearer {CLIENT_KEY}"))
        .header("x-api-key", CLIENT_KEY)
        .header("cookie", CLIENT_COOKIE)
        .header("user-agent", CODEX_UA)
        .header("originator", "codex_cli_rs")
}

/// Credential isolation: the upstream sees the credential's token and nothing of the
/// client's own key or cookie, under any header name.
fn assert_isolated(name: &str, profile: Profile, headers: &HeaderMap) {
    assert_eq!(
        headers["authorization"],
        profile.bearer(),
        "{name}: the credential's token"
    );
    for (header, value) in headers {
        let value = String::from_utf8_lossy(value.as_bytes());
        assert!(
            !value.contains(CLIENT_KEY) && !value.contains("client-cookie-QUALIFY"),
            "{name}: the client's secret reached the upstream as {header}"
        );
    }
}

async fn post(proxy: &Proxy, path: &str, body: &str) -> (u16, String) {
    let response = client_request(wreq::Client::new().post(format!("http://{}{path}", proxy.addr)))
        .header("content-type", "application/json")
        .body(body.to_owned())
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.text().await.unwrap())
}

/// The terminal event's `response` object of a fixture stream, as the fixture spells it.
fn terminal_response(name: &str) -> String {
    let frames = fixture(name).frames();
    let terminal = CodexBody::parse(frames.last().unwrap()).unwrap();
    text(terminal.get("response").unwrap().raw)
}

/// HTTP, streamed and buffered, on every profile: the client's bytes upstream (with
/// `stream: true` added for a buffered `chatgpt` client, the one permitted body change),
/// the fixture's events or the destination's answer back, unchanged, and only the
/// credential's token upstream.
#[tokio::test]
async fn http_streamed_and_buffered_across_profiles() {
    const JSON: &str =
        r#"{"id":"resp_json","object":"response","status":"completed","output":[],"future_field":[1.50,null,false]}"#;
    for profile in PROFILES {
        for (name, body) in bodies() {
            for stream in [true, false] {
                let client = with(&body, "stream", if stream { "true" } else { "false" });
                let sse_reply = stream || profile == Profile::Chatgpt;
                let reply = if sse_reply {
                    Reply::sse(fixture("sse-completed").bytes())
                } else {
                    Reply::json(200, JSON)
                };
                let proxy = proxy(
                    Script {
                        http: vec![reply],
                        ..Script::default()
                    },
                    "",
                    |url| vec![credential(profile, url, false)],
                )
                .await;
                let case = format!("{profile:?} {name} stream={stream}");
                let (status, answer) = post(&proxy, "/v1/responses", &client).await;
                assert_eq!(status, 200, "{case}: {answer}");
                let want_answer = match (stream, profile) {
                    (true, _) => text(&fixture("sse-completed").bytes()),
                    (false, Profile::Chatgpt) => terminal_response("sse-completed"),
                    (false, _) => JSON.to_owned(),
                };
                assert_eq!(answer, want_answer, "{case}: the client's answer");
                let sent = proxy.upstream.http().pop().expect("an upstream request");
                let want_sent = if !stream && profile == Profile::Chatgpt {
                    with(&client, "stream", "true")
                } else {
                    client.clone()
                };
                assert_eq!(text(&sent.body), want_sent, "{case}: the upstream bytes");
                assert_eq!(sent.uri.path(), "/responses", "{case}");
                assert_isolated(&case, profile, &sent.headers);
            }
        }
    }
}

/// `POST /responses/compact` on every profile (documented for the API; the pinned client
/// uses compaction v2, which the matrices above cover): the client's non-streamed body to
/// `/responses/compact` and the destination's answer back, both unchanged.
#[tokio::test]
async fn compaction_across_profiles() {
    const COMPACTED: &str = r#"{"id":"resp_c","object":"response.compaction","output":[{"type":"compaction","encrypted_content":"OPAQUE-FIXTURE"}],"future":null}"#;
    for profile in PROFILES {
        let proxy = proxy(
            Script {
                http: vec![Reply::json(200, COMPACTED)],
                ..Script::default()
            },
            "",
            |url| vec![credential(profile, url, false)],
        )
        .await;
        let body = with(&client_body("http-request-compaction"), "stream", "false");
        let (status, answer) = post(&proxy, "/v1/responses/compact", &body).await;
        assert_eq!((status, answer.as_str()), (200, COMPACTED), "{profile:?}");
        let sent = proxy.upstream.http().pop().unwrap();
        assert_eq!(sent.uri.path(), "/responses/compact", "{profile:?}");
        assert_eq!(text(&sent.body), body, "{profile:?}: the upstream bytes");
        assert_isolated(&format!("{profile:?} compact"), profile, &sent.headers);
    }
}

async fn websocket(proxy: &Proxy) -> wreq::ws::WebSocket {
    wreq::Client::new()
        .websocket(format!("ws://{}/v1/responses", proxy.addr))
        .header("authorization", format!("Bearer {CLIENT_KEY}"))
        .header("x-api-key", CLIENT_KEY)
        .header("cookie", CLIENT_COOKIE)
        .header("user-agent", CODEX_UA)
        .header("originator", "codex_cli_rs")
        .send()
        .await
        .unwrap()
        .into_websocket()
        .await
        .unwrap()
}

/// The client's frames until a terminal event, or the close code.
async fn turn(socket: &mut wreq::ws::WebSocket) -> (Vec<String>, Option<u16>) {
    let mut frames = Vec::new();
    loop {
        let next = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .unwrap_or_else(|_| panic!("timed out after {frames:?}"));
        match next {
            Some(Ok(Message::Text(t))) => {
                let t = t.as_str().to_owned();
                let done = ["response.completed", "response.incomplete", "response.failed"]
                    .iter()
                    .any(|k| t.contains(&format!("\"type\":\"{k}\"")));
                frames.push(t);
                if done {
                    return (frames, None);
                }
            }
            Some(Ok(Message::Close(frame))) => return (frames, Some(frame.map_or(1005, |f| u16::from(f.code)))),
            Some(Ok(_)) => {}
            Some(Err(_)) | None => return (frames, Some(1006)),
        }
    }
}

/// WebSocket turns on every profile, on an upstream socket and over HTTP: the client's
/// `response.create` reaches the socket as sent, or the HTTP upstream without its tag and
/// with `stream: true`; the client gets each upstream event's payload unchanged, and only
/// the credential's token crosses the proxy.
#[tokio::test]
async fn websocket_turns_across_profiles_and_transports() {
    let events: Vec<String> = fixture("sse-completed").frames().iter().map(|f| text(f)).collect();
    for websockets in [true, false] {
        for profile in PROFILES {
            for (name, body) in bodies() {
                // A turn frame: the HTTP body with the WebSocket tag, `stream` as sent.
                let frame = with(&without(&body, "stream"), "type", r#""response.create""#);
                let script = if websockets {
                    Script {
                        ws: vec![fixture("sse-completed").frames()],
                        ..Script::default()
                    }
                } else {
                    Script {
                        http: vec![Reply::sse(fixture("sse-completed").bytes())],
                        ..Script::default()
                    }
                };
                let proxy = proxy(script, "", |url| vec![credential(profile, url, websockets)]).await;
                let case = format!("websockets={websockets} {profile:?} {name}");
                let mut socket = websocket(&proxy).await;
                socket.send(Message::text(frame.clone())).await.unwrap();
                let (frames, closed) = turn(&mut socket).await;
                assert_eq!(closed, None, "{case}: {frames:?}");
                assert_eq!(frames, events, "{case}: the upstream's events");
                if websockets {
                    let sockets = proxy.upstream.sockets();
                    assert_eq!(text(&sockets[0].frames[0]), frame, "{case}: the client's frame");
                    assert_isolated(&case, profile, &sockets[0].headers);
                } else {
                    let sent = proxy.upstream.http().pop().expect("an HTTP request");
                    let want = with(&without(&frame, "type"), "stream", "true");
                    assert!(!is_true(&frame, "stream"), "{case}: the frame omits stream");
                    assert_eq!(
                        text(&sent.body),
                        want,
                        "{case}: the frame without its tag, stream: true"
                    );
                    assert_isolated(&case, profile, &sent.headers);
                }
            }
        }
    }
}

/// An operator alias routes only `model`, over HTTP and over a WebSocket turn on HTTP:
/// the upstream gets the client's bytes with the model value replaced in place.
#[tokio::test]
async fn an_alias_routes_only_the_model_on_every_transport() {
    let extra = "codex-api-key:\n  - api-key: key-FAKE\n    base-url: {upstream}\n    models:\n      - name: gpt-upstream\n        alias: gpt-alias\n";
    let proxy = proxy(
        Script {
            http: vec![
                Reply::sse(fixture("sse-completed").bytes()),
                Reply::sse(fixture("sse-completed").bytes()),
            ],
            ..Script::default()
        },
        extra,
        |_| Vec::new(),
    )
    .await;
    let body =
        client_body("probe-request-unknown-fields").replacen(r#""model":"gpt-5.5""#, r#""model":"gpt-alias""#, 1);
    let (status, answer) = post(&proxy, "/v1/responses", &body).await;
    assert_eq!(status, 200, "{answer}");
    let sent = proxy.upstream.http().pop().unwrap();
    assert_eq!(
        text(&sent.body),
        with(&body, "model", r#""gpt-upstream""#),
        "HTTP: only the model"
    );
    assert_isolated("alias http", Profile::Compatible, &sent.headers);
    let mut socket = websocket(&proxy).await;
    let frame = with(&body, "type", r#""response.create""#);
    socket.send(Message::text(frame.clone())).await.unwrap();
    let (_, closed) = turn(&mut socket).await;
    assert_eq!(closed, None);
    let sent = proxy.upstream.http().pop().unwrap();
    let want = with(&without(&frame, "type"), "model", r#""gpt-upstream""#);
    assert_eq!(
        text(&sent.body),
        want,
        "WebSocket over HTTP: only the tag and the model"
    );
}

/// An interrupted response and a truncated stream on every profile, over HTTP: the
/// interrupted `response.incomplete` reaches the client unchanged; a stream that ends
/// without a terminal event ends for the client after the events that arrived, with no
/// event the upstream did not send.
#[tokio::test]
async fn interrupted_and_truncated_streams_across_profiles() {
    let incomplete = text(&fixture("sse-incomplete-partial").bytes());
    let completed = text(&fixture("sse-completed").bytes());
    // The first two events of the completed stream, then the end of the body.
    let cut = completed.match_indices("\n\n").nth(1).unwrap().0 + 2;
    let truncated = completed[..cut].to_owned();
    for profile in PROFILES {
        for (name, upstream) in [("interrupted", &incomplete), ("truncated", &truncated)] {
            let proxy = proxy(
                Script {
                    http: vec![Reply::sse(Bytes::from(upstream.clone()))],
                    ..Script::default()
                },
                "",
                |url| vec![credential(profile, url, false)],
            )
            .await;
            let (status, answer) = post(&proxy, "/v1/responses", &client_body("http-request-full")).await;
            assert_eq!(status, 200, "{profile:?} {name}");
            assert_eq!(
                &answer, upstream,
                "{profile:?} {name}: the events that arrived, unchanged"
            );
        }
    }
}

/// Structured failures on every profile, over HTTP: an upstream usage-limit 429 reaches
/// the client with its status and body, and a context-window `response.failed` arrives as
/// the provider's own event, unchanged. Each failure gets its own proxy, because the usage
/// limit cools the only credential.
#[tokio::test]
async fn structured_failures_across_profiles() {
    let limit = text(&fixture("http-error-usage-limit-429").bytes());
    let context = text(&fixture("sse-failed-context-window").bytes());
    for profile in PROFILES {
        let limited = proxy(
            Script {
                http: vec![Reply::json(429, limit.clone())],
                ..Script::default()
            },
            "",
            |url| vec![credential(profile, url, false)],
        )
        .await;
        let (status, answer) = post(&limited, "/v1/responses", &client_body("http-request-full")).await;
        assert_eq!(
            (status, answer.as_str()),
            (429, limit.as_str()),
            "{profile:?}: the usage limit"
        );
        let failed = proxy(
            Script {
                http: vec![Reply::sse(Bytes::from(context.clone()))],
                ..Script::default()
            },
            "",
            |url| vec![credential(profile, url, false)],
        )
        .await;
        let (status, answer) = post(&failed, "/v1/responses", &client_body("http-request-full")).await;
        assert_eq!(
            (status, answer.as_str()),
            (200, context.as_str()),
            "{profile:?}: the context-window event"
        );
    }
}
