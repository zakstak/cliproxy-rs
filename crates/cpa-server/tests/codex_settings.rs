//! #28: no accepted setting rewrites a Codex request, on any route, whatever the client
//! sends. One config turns on every Codex rewrite control Go had: payload `default`,
//! `override` and `filter` rules (scoped to the `codex` protocol and unscoped), the
//! multi-agent v2 and orphan-delegation rewrites, `disable-codex-cloaking`, header
//! defaults, bootstrap buffering, model-level cooling and response steering. Requests
//! from an official Codex client (with the multi-agent subagent and Responses Lite
//! headers), another Responses client and a client without a `User-Agent` then go to
//! every Codex route: native HTTP on the `chatgpt`, `compatible` and Sign in with ChatGPT
//! profiles, a WebSocket turn over HTTP and a native WebSocket turn. Each upstream body
//! is the client's, with only the contract's permitted edits. An OpenAI-compatible
//! provider under the same config still gets the payload rules: the shared settings are
//! scoped to other providers, not dead (docs/codex/CONTRACT.md "Settings").
//!
//! Loopback capture upstreams, fake tokens and a private `auth-dir` only.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_exec::Executors;
use cpa_server::testing::codex_capture::{CaptureUpstream, Reply, Script};
use futures_util::StreamExt;
use wreq::ws::message::Message;

/// Every Codex rewrite control Go had, in the legacy spellings an operator migrating from
/// Go keeps.
const CONTROLS: &str = r#"
payload:
  default:
    - models: [{name: "*"}]
      params: {max_output_tokens: 7}
    - models: [{name: "*", protocol: codex}]
      params: {truncation: auto}
  override:
    - models: [{name: "*"}]
      params: {temperature: 0.42}
    - models: [{name: "*", protocol: codex}]
      params: {instructions: injected, parallel_tool_calls: true}
  filter:
    - models: [{name: "*"}]
      params: [include, prompt_cache_key, service_tier]
codex:
  optimize-multi-agent-v2: true
  orphan-delegation-compatibility: true
  disable-codex-cloaking: true
  stream-bootstrap-buffering: true
  model-level-cooling: true
  response-steering: true
  header-defaults: {beta-features: proxy-beta}
"#;

/// A Codex client's request that each Go rewrite would have changed: a multi-agent
/// namespace, an orphan delegation output, a replayed web search call (Go added a web
/// search tool), `parallel_tool_calls: false`, output replay in `include`, a prompt-cache
/// key and a `flex` tier.
const BODY: &str = r#"{"model":"gpt-5.5","stream":true,"input":[{"type":"function_call_output","call_id":"orphan","name":"create_thread","namespace":"codex_app","output":"thread ok"},{"type":"web_search_call","id":"ws_1","status":"completed","action":{"type":"search","query":"q"}},{"type":"message","role":"user","content":[{"type":"input_text","text":"delegate"}]}],"tools":[{"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"spawn_agent","description":"Spawns an agent.","parameters":{"type":"object","properties":{"message":{"type":"string"}}}}]}],"parallel_tool_calls":false,"include":["reasoning.encrypted_content","output.encrypted_content"],"prompt_cache_key":"pck-1","service_tier":"flex","store":false}"#;

const SSE: &str = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n";
const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"resp_1","object":"response","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#;

/// The client header sets: an official Codex client with the multi-agent subagent and
/// Responses Lite markers, another Responses client, and a client without a User-Agent.
fn clients() -> [Vec<(&'static str, &'static str)>; 3] {
    [
        vec![
            ("user-agent", "codex_cli_rs/0.160.0 (Linux; x86_64) xterm-256color"),
            ("originator", "codex_cli_rs"),
            ("x-openai-subagent", "collab_spawn"),
            ("x-openai-internal-codex-responses-lite", "true"),
            ("x-codex-beta-features", "client-beta"),
        ],
        vec![("user-agent", "OpenAI/Python 2.1.0")],
        vec![],
    ]
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
    Chatgpt,
    Compatible,
    Siwc,
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
        "auth-dir: {}\naccess:\n  api-keys: [client-key]\n{CONTROLS}{}",
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

fn untagged(frame: &str) -> String {
    let body = cpa_common::codex_body::CodexBody::parse(frame.as_bytes()).unwrap();
    String::from_utf8(body.without_member("type")).unwrap()
}

fn frame() -> String {
    let body = cpa_common::codex_body::CodexBody::parse(BODY.as_bytes()).unwrap();
    String::from_utf8(body.with_member("type", r#""response.create""#)).unwrap()
}

/// Native HTTP on every profile: the client's bytes, whichever client sent them.
#[tokio::test]
async fn native_http_ignores_every_rewrite_control_on_every_profile() {
    for profile in [Profile::Chatgpt, Profile::Compatible, Profile::Siwc] {
        for headers in clients() {
            let proxy = proxy(
                Script {
                    http: vec![Reply::sse(SSE)],
                    ..Script::default()
                },
                "",
                |url| vec![credential(profile, url, false)],
            )
            .await;
            let mut request = wreq::Client::new()
                .post(format!("http://{}/v1/responses", proxy.addr))
                .header("authorization", "Bearer client-key")
                .header("content-type", "application/json");
            for (name, value) in &headers {
                request = request.header(*name, *value);
            }
            let response = request.body(BODY).send().await.unwrap();
            let status = response.status().as_u16();
            let text = response.text().await.unwrap();
            assert_eq!(status, 200, "{profile:?} {headers:?}: {text}");
            let sent = proxy.upstream.http().pop().expect("an upstream request");
            assert_eq!(
                String::from_utf8_lossy(&sent.body),
                BODY,
                "{profile:?} {headers:?}: the client's bytes"
            );
        }
    }
}

async fn websocket(proxy: &Proxy, headers: &[(&str, &str)]) -> wreq::ws::WebSocket {
    let mut request = wreq::Client::new()
        .websocket(format!("ws://{}/v1/responses", proxy.addr))
        .header("authorization", "Bearer client-key");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request.send().await.unwrap().into_websocket().await.unwrap()
}

async fn until_completed(socket: &mut wreq::ws::WebSocket) -> Vec<String> {
    let mut frames = Vec::new();
    loop {
        let next = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .unwrap_or_else(|_| panic!("timed out after {frames:?}"));
        match next {
            Some(Ok(Message::Text(text))) => {
                let text = text.as_str().to_owned();
                let done = text.contains("\"response.completed\"");
                frames.push(text);
                if done {
                    return frames;
                }
            }
            Some(Ok(_)) => continue,
            other => panic!("socket ended after {frames:?}: {other:?}"),
        }
    }
}

/// WebSocket turns, over HTTP (no `websockets`) and on an upstream socket, with response
/// steering on: the frame as the client wrote it (without its tag over HTTP), and the
/// upstream's event unchanged.
#[tokio::test]
async fn websocket_turns_ignore_every_rewrite_control() {
    for websockets in [false, true] {
        for profile in [Profile::Chatgpt, Profile::Compatible, Profile::Siwc] {
            for headers in clients() {
                let script = if websockets {
                    Script {
                        ws: vec![vec![axum::body::Bytes::from_static(COMPLETED.as_bytes())]],
                        ..Script::default()
                    }
                } else {
                    Script {
                        http: vec![Reply::sse(SSE)],
                        ..Script::default()
                    }
                };
                let proxy = proxy(script, "", |url| vec![credential(profile, url, websockets)]).await;
                let mut socket = websocket(&proxy, &headers).await;
                let frame = frame();
                socket.send(Message::text(frame.clone())).await.unwrap();
                let frames = until_completed(&mut socket).await;
                let name = format!("websockets={websockets} {profile:?} {headers:?}");
                assert_eq!(frames.last().map(String::as_str), Some(COMPLETED), "{name}");
                if websockets {
                    let sockets = proxy.upstream.sockets();
                    let sent = String::from_utf8_lossy(&sockets[0].frames[0]).into_owned();
                    assert_eq!(sent, frame, "{name}: the client's frame");
                } else {
                    let sent = proxy.upstream.http().pop().expect("an HTTP request");
                    assert_eq!(
                        String::from_utf8_lossy(&sent.body),
                        untagged(&frame),
                        "{name}: the client's frame without its tag"
                    );
                }
            }
        }
    }
}

/// The control: an OpenAI-compatible provider under the same config still gets the
/// shared payload rules, so they are scoped to other providers, not silently dead.
#[tokio::test]
async fn other_providers_keep_the_shared_payload_rules() {
    let chat = r#"{"id":"c1","object":"chat.completion","created":1,"model":"up-model","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
    let proxy = proxy(
        Script {
            http: vec![Reply::json(200, chat)],
            ..Script::default()
        },
        "openai-compatibility:\n  - name: Acme\n    base-url: {upstream}/v1\n    models: [{name: up-model, alias: control-model}]\n    api-key-entries: [{api-key: key-FAKE-acme}]\n",
        |_| Vec::new(),
    )
    .await;
    let response = wreq::Client::new()
        .post(format!("http://{}/v1/chat/completions", proxy.addr))
        .header("authorization", "Bearer client-key")
        .header("content-type", "application/json")
        .body(r#"{"model":"control-model","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    assert_eq!(status, 200, "{text}");
    let sent = proxy.upstream.http().pop().expect("an upstream request");
    let body: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(body["temperature"], 0.42, "the unscoped override applies: {body}");
    assert_eq!(body["max_output_tokens"], 7, "the unscoped default applies: {body}");
    assert!(
        body.get("instructions").is_none(),
        "the codex-scoped rule does not: {body}"
    );
}
