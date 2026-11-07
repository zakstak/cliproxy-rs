//! The Codex translated route through the real router (#27): Claude Messages, Chat
//! Completions, Gemini and Interactions requests served by a Codex credential on a
//! native profile (`docs/codex/CONTRACT.md` "Translated route").
//!
//! Each source request is hand-authored from its provider's documented request shape:
//! Anthropic Messages (`system`, `thinking`, `tools`, `tool_use`/`tool_result` blocks),
//! OpenAI Chat Completions (`messages` with `tool_calls` and `tool` messages, `tools`,
//! `parallel_tool_calls`), Gemini `generateContent` (`systemInstruction`, `contents` with
//! `functionCall`/`functionResponse` parts, `functionDeclarations`, `thinkingConfig`) and
//! Gemini Interactions (`input`). The upstream answer is the #21 first-party fixture
//! `sse-completed` (encrypted reasoning, a message, full usage). The tests check meaning,
//! not Go's bytes: what the caller said reaches the upstream in Responses form, nothing
//! the caller did not say is added, and what Codex cannot express is refused before any
//! upstream request.
//!
//! Loopback capture upstreams, fake tokens and a private `auth-dir` only.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_exec::Executors;
use cpa_exec::codex_contract::fixture;
use cpa_server::testing::codex_capture::{CaptureUpstream, Reply, Script};
use serde_json::{Value, json};

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

/// A private, empty `auth-dir`, removed when dropped.
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

/// A ChatGPT OAuth Codex credential pointing at `base_url`.
fn credential(base_url: &str) -> Credential {
    let metadata = json!({
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
    credential
}

struct Proxy {
    url: String,
    upstream: CaptureUpstream,
    _auth_dir: AuthDir,
}

fn executors() -> Executors {
    Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    }
}

/// A proxy with one Codex credential at a capture upstream answering `replies`.
async fn proxy(replies: Vec<Reply>, extra_config: &str) -> Proxy {
    let upstream = CaptureUpstream::start(Script {
        http: replies,
        ..Script::default()
    })
    .await;
    let auth_dir = AuthDir::new();
    let cfg = Config::parse(&format!(
        "auth-dir: {}\naccess:\n  api-keys: [client-key]\n{extra_config}",
        auth_dir.0.display()
    ))
    .unwrap();
    let rt = Arc::new(cpa_server::testing::runtime(
        cfg,
        vec![credential(&upstream.base_url)],
        executors(),
    ));
    Proxy {
        url: serve(cpa_server::router(rt)).await,
        upstream,
        _auth_dir: auth_dir,
    }
}

fn completed() -> Reply {
    Reply::sse(fixture("sse-completed").bytes())
}

async fn post(url: &str, path: &str, body: &Value) -> (u16, String) {
    let response = wreq::Client::new()
        .post(format!("{url}{path}"))
        .header("authorization", "Bearer client-key")
        .header("x-api-key", "client-key")
        .header("x-goog-api-key", "client-key")
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.text().await.unwrap())
}

/// The one upstream request, as JSON.
fn sent(proxy: &Proxy) -> Value {
    let http = proxy.upstream.http();
    assert_eq!(http.len(), 1, "one upstream request");
    assert_eq!(http[0].uri.path(), "/responses");
    assert_eq!(
        http[0].headers["authorization"], "Bearer at-FAKE",
        "the client key stays local"
    );
    serde_json::from_slice(&http[0].body).unwrap()
}

/// Nothing the caller did not ask for: no `instructions`, prompt-cache key, replayed
/// reasoning or injected tools; the routed model and the transport `stream`.
fn assert_nothing_added(body: &Value, tools: &[&str]) {
    assert!(body.get("instructions").is_none(), "no instructions: {body}");
    assert!(
        body.get("prompt_cache_key").is_none(),
        "no synthesized cache key: {body}"
    );
    assert!(!body.to_string().contains("cpa_codex_replay_turn"), "no replay: {body}");
    assert_eq!(body["model"], "gpt-5.5");
    assert_eq!(body["stream"], true);
    let names: Vec<&str> = body["tools"]
        .as_array()
        .map(|t| t.iter().filter_map(|t| t["name"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(names, tools, "only the caller's tools: {body}");
    let kinds: Vec<&str> = body["tools"]
        .as_array()
        .map(|t| t.iter().filter_map(|t| t["type"].as_str()).collect())
        .unwrap_or_default();
    assert!(
        kinds.iter().all(|k| *k == "function"),
        "no image or web-search tool added: {body}"
    );
}

fn items<'a>(body: &'a Value, kind: &str) -> Vec<&'a Value> {
    body["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["type"] == kind)
        .collect()
}

/// A Codex encrypted-reasoning value as a Claude client carries it back in a thinking
/// block's `signature` (Go's `validCodexReasoningEncryptedContentForTestSeed` shape).
fn codex_signature() -> String {
    use base64::Engine as _;
    let mut payload = vec![0u8; 1 + 8 + 16 + 16 + 32];
    payload[0] = 0x80;
    for (i, byte) in payload.iter_mut().enumerate().skip(9) {
        *byte = 7u8.wrapping_add(i as u8);
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
}

fn claude_request(stream: bool) -> Value {
    json!({
        "model": "gpt-5.5",
        "max_tokens": 1024,
        "stream": stream,
        "system": [{"type": "text", "text": "Be brief."}],
        "thinking": {"type": "enabled", "budget_tokens": 2048},
        "tools": [{
            "name": "get_weather",
            "description": "Weather for a city.",
            "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}
        }],
        "messages": [
            {"role": "user", "content": "Weather in Paris?"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "", "signature": codex_signature()},
                {"type": "tool_use", "id": "toolu_01", "name": "get_weather", "input": {"city": "Paris"}}
            ]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_01", "content": "18C"}]}
        ]
    })
}

/// The Claude request in Responses form: the system prompt as a developer item, the
/// tool call and its result under the caller's ID and name, the prior reasoning as the
/// opaque encrypted item it came from, the caller's tool and its schema, and a reasoning
/// effort from the caller's thinking budget.
fn assert_claude_conversion(body: &Value) {
    assert_nothing_added(body, &["get_weather"]);
    let developer = &body["input"][0];
    assert_eq!(developer["role"], "developer", "{body}");
    assert_eq!(developer["content"][0]["text"], "Be brief.");
    let calls = items(body, "function_call");
    assert_eq!(calls.len(), 1, "{body}");
    assert_eq!(calls[0]["call_id"], "toolu_01");
    assert_eq!(calls[0]["name"], "get_weather");
    assert_eq!(
        serde_json::from_str::<Value>(calls[0]["arguments"].as_str().unwrap()).unwrap(),
        json!({"city": "Paris"})
    );
    let outputs = items(body, "function_call_output");
    assert_eq!(outputs[0]["call_id"], "toolu_01");
    assert_eq!(outputs[0]["output"], "18C");
    let reasoning = items(body, "reasoning");
    assert_eq!(reasoning.len(), 1, "{body}");
    assert_eq!(
        reasoning[0]["encrypted_content"],
        codex_signature(),
        "opaque, unchanged"
    );
    assert_eq!(
        body["tools"][0]["parameters"]["properties"],
        json!({"city": {"type": "string"}})
    );
    assert!(body["reasoning"]["effort"].is_string(), "the caller's thinking: {body}");
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
}

/// Claude, streamed and buffered: the conversion above, and the answer back in Claude
/// form with the encrypted reasoning as the thinking signature and the provider's usage.
/// `message_start` carries no local token estimate in place of provider usage.
#[tokio::test]
async fn claude_round_trips_keep_meaning() {
    for stream in [true, false] {
        let proxy = proxy(vec![completed()], "").await;
        let (status, answer) = post(&proxy.url, "/v1/messages", &claude_request(stream)).await;
        assert_eq!(status, 200, "stream={stream}: {answer}");
        assert_claude_conversion(&sent(&proxy));
        if stream {
            let events: Vec<Value> = answer
                .lines()
                .filter_map(|l| l.strip_prefix("data: "))
                .filter_map(|d| serde_json::from_str(d).ok())
                .collect();
            let start = events.iter().find(|e| e["type"] == "message_start").unwrap();
            assert_eq!(
                start["message"]["usage"]["input_tokens"], 0,
                "no local estimate: {start}"
            );
            assert!(
                events
                    .iter()
                    .any(|e| e["delta"]["signature"] == "ENCRYPTED-OPAQUE-FIXTURE-2"),
                "the encrypted reasoning as the thinking signature: {answer}"
            );
            assert!(answer.contains("Done."), "{answer}");
            let delta = events.iter().rev().find(|e| e["type"] == "message_delta").unwrap();
            assert_eq!(delta["usage"]["output_tokens"], 40, "the provider's usage: {delta}");
        } else {
            let message: Value = serde_json::from_str(&answer).unwrap();
            let thinking = message["content"]
                .as_array()
                .unwrap()
                .iter()
                .find(|b| b["type"] == "thinking");
            assert_eq!(
                thinking.unwrap()["signature"],
                "ENCRYPTED-OPAQUE-FIXTURE-2",
                "{message}"
            );
            assert!(answer.contains("Done."), "{answer}");
            assert_eq!(message["usage"]["output_tokens"], 40, "{message}");
        }
    }
}

/// A Claude request without `thinking` gets no reasoning effort: the model's default
/// applies, where Go sends `medium`.
#[tokio::test]
async fn claude_without_thinking_gets_no_reasoning_default() {
    let proxy = proxy(vec![completed()], "").await;
    let mut request = claude_request(false);
    request.as_object_mut().unwrap().remove("thinking");
    let (status, answer) = post(&proxy.url, "/v1/messages", &request).await;
    assert_eq!(status, 200, "{answer}");
    let body = sent(&proxy);
    assert!(body["reasoning"].get("effort").is_none(), "{body}");
}

/// Content Codex cannot express is refused before any upstream request, with a message
/// that names the part: Claude `redacted_thinking`, a non-PDF document, a Chat `refusal`
/// part, an empty audio part, and Gemini code execution.
#[tokio::test]
async fn unexpressible_content_is_refused_before_the_send() {
    let mut redacted = claude_request(false);
    redacted["messages"][1]["content"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type": "redacted_thinking", "data": "opaque"}));
    let mut document = claude_request(false);
    document["messages"][0] = json!({"role": "user", "content": [
        {"type": "document", "source": {"type": "base64", "media_type": "text/plain", "data": "aGk="}}
    ]});
    let refusal = json!({"model": "gpt-5.5", "messages": [
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": [{"type": "refusal", "refusal": "no"}]},
        {"role": "user", "content": "please"}
    ]});
    let audio = json!({"model": "gpt-5.5", "messages": [
        {"role": "user", "content": [{"type": "input_audio", "input_audio": {"data": "", "format": "wav"}}, {"type": "text", "text": "hi"}]}
    ]});
    let code = json!({"contents": [{"role": "user", "parts": [
        {"text": "run it"},
        {"executableCode": {"language": "PYTHON", "code": "print(1)"}}
    ]}]});
    let cases = [
        ("/v1/messages", redacted, "redacted_thinking"),
        ("/v1/messages", document, "document"),
        ("/v1/chat/completions", refusal, "refusal"),
        ("/v1/chat/completions", audio, "input_audio"),
        ("/v1beta/models/gpt-5.5:generateContent", code, "executableCode"),
    ];
    for (path, body, part) in cases {
        let proxy = proxy(vec![completed()], "").await;
        let (status, answer) = post(&proxy.url, path, &body).await;
        assert_eq!(status, 400, "{path} {part}: {answer}");
        assert!(answer.contains("unsupported conversion"), "{path}: {answer}");
        assert!(answer.contains(part), "{path}: the part is named: {answer}");
        assert!(proxy.upstream.http().is_empty(), "{path} {part}: nothing was sent");
    }
}

fn chat_request(stream: bool) -> Value {
    json!({
        "model": "gpt-5.5",
        "stream": stream,
        "parallel_tool_calls": false,
        "tools": [{"type": "function", "function": {
            "name": "get_weather",
            "description": "Weather for a city.",
            "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
        }}],
        "messages": [
            {"role": "system", "content": "Be brief."},
            {"role": "user", "content": "Weather in Paris?"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_1", "content": "18C"}
        ]
    })
}

/// Chat Completions, streamed and buffered: the system message as a developer item,
/// the tool call and result under the caller's ID, the caller's `parallel_tool_calls:
/// false` (Go forces `true`), no reasoning effort the caller did not choose, and the
/// answer with the provider's usage.
#[tokio::test]
async fn chat_round_trips_keep_meaning() {
    for stream in [true, false] {
        let proxy = proxy(vec![completed()], "").await;
        let (status, answer) = post(&proxy.url, "/v1/chat/completions", &chat_request(stream)).await;
        assert_eq!(status, 200, "stream={stream}: {answer}");
        let body = sent(&proxy);
        assert_nothing_added(&body, &["get_weather"]);
        assert_eq!(body["parallel_tool_calls"], false, "{body}");
        assert!(body["reasoning"].get("effort").is_none(), "{body}");
        assert_eq!(body["input"][0]["role"], "developer");
        assert_eq!(items(&body, "function_call")[0]["call_id"], "call_1");
        assert_eq!(items(&body, "function_call_output")[0]["output"], "18C");
        assert!(answer.contains("Done."), "{answer}");
        if !stream {
            let completion: Value = serde_json::from_str(&answer).unwrap();
            assert_eq!(completion["usage"]["completion_tokens"], 40, "{completion}");
        }
    }
}

/// Gemini `generateContent`: the system instruction as a developer item, the function
/// call and response paired under one ID, the caller's thinking budget as an effort, and
/// the answer back as candidate text with the provider's usage.
#[tokio::test]
async fn gemini_round_trip_keeps_meaning() {
    let proxy = proxy(vec![completed()], "").await;
    let request = json!({
        "systemInstruction": {"parts": [{"text": "Be brief."}]},
        "generationConfig": {"thinkingConfig": {"thinkingBudget": 1024}},
        "tools": [{"functionDeclarations": [{
            "name": "get_weather",
            "description": "Weather for a city.",
            "parameters": {"type": "OBJECT", "properties": {"city": {"type": "STRING"}}}
        }]}],
        "contents": [
            {"role": "user", "parts": [{"text": "Weather in Paris?"}]},
            {"role": "model", "parts": [{"functionCall": {"name": "get_weather", "args": {"city": "Paris"}}}]},
            {"role": "user", "parts": [{"functionResponse": {"name": "get_weather", "response": {"result": "18C"}}}]}
        ]
    });
    let (status, answer) = post(&proxy.url, "/v1beta/models/gpt-5.5:generateContent", &request).await;
    assert_eq!(status, 200, "{answer}");
    let body = sent(&proxy);
    assert_nothing_added(&body, &["get_weather"]);
    assert_eq!(body["input"][0]["role"], "developer");
    let call = items(&body, "function_call")[0].clone();
    let output = items(&body, "function_call_output")[0].clone();
    assert_eq!(call["call_id"], output["call_id"], "{body}");
    assert_eq!(output["output"], "18C");
    assert!(body["reasoning"]["effort"].is_string(), "{body}");
    let response: Value = serde_json::from_str(&answer).unwrap();
    assert!(answer.contains("Done."), "{answer}");
    assert_eq!(response["usageMetadata"]["candidatesTokenCount"], 40, "{response}");
}

/// Gemini Interactions: a plain input gets no default instructions or reasoning.
#[tokio::test]
async fn interactions_conversion_adds_nothing() {
    let proxy = proxy(vec![completed()], "").await;
    let (status, answer) = post(
        &proxy.url,
        "/v1beta/interactions",
        &json!({"model": "gpt-5.5", "input": "Hello"}),
    )
    .await;
    assert_eq!(status, 200, "{answer}");
    let body = sent(&proxy);
    assert_nothing_added(&body, &[]);
    assert!(body["reasoning"].get("effort").is_none(), "{body}");
    assert!(answer.contains("Done."), "{answer}");
}

/// Payload rules and the Codex-client rewrites cannot reach a translated body.
#[tokio::test]
async fn operator_rules_cannot_rewrite_translated_bodies() {
    let plain = proxy(vec![completed()], "").await;
    post(&plain.url, "/v1/messages", &claude_request(false)).await;
    let rules = "payload:\n  override:\n    - models: [{name: gpt-5.5}]\n      params: {instructions: injected, store: true}\n  default:\n    - models: [{name: gpt-5.5}]\n      params: {prompt_cache_key: injected}\n  filter:\n    - models: [{name: gpt-5.5}]\n      params: [tools]\nupstream:\n  codex:\n    orphan-delegation-compatibility: true\nclient:\n  codex:\n    optimize-multi-agent-v2: true\n";
    let ruled = proxy(vec![completed()], rules).await;
    let (status, answer) = post(&ruled.url, "/v1/messages", &claude_request(false)).await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(sent(&ruled), sent(&plain), "the same body with and without the rules");
}

/// An upstream error reaches the Chat client in its own error shape with the provider's
/// message.
#[tokio::test]
async fn an_upstream_error_keeps_its_message() {
    let proxy = proxy(
        vec![Reply::json(
            400,
            r#"{"error":{"type":"invalid_request_error","code":"invalid_value","message":"Unknown parameter: frobnicate."}}"#,
        )],
        "",
    )
    .await;
    let (status, answer) = post(&proxy.url, "/v1/chat/completions", &chat_request(false)).await;
    assert_eq!(status, 400, "{answer}");
    assert!(answer.contains("Unknown parameter: frobnicate."), "{answer}");
}

// Home: the pool is remote and the local store holds no credential.

struct Home {
    picks: std::sync::Mutex<Vec<Credential>>,
    released: Arc<AtomicUsize>,
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
        let credential = self.picks.lock().unwrap().remove(0);
        let released = self.released.clone();
        Box::pin(async move {
            Ok(cpa_server::remote::RemoteGrant {
                credential,
                end: Box::new(move || {
                    released.fetch_add(1, Ordering::SeqCst);
                    None
                }),
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
    ) -> futures_util::future::BoxFuture<'_, Result<Vec<u8>, cpa_server::remote::ModelsError>> {
        unreachable!("no model lists")
    }
}

/// A Claude request that Home hands to a native Codex credential takes the translated
/// route, and Home's lease is released.
#[tokio::test]
async fn a_home_pick_takes_the_translated_route() {
    let upstream = CaptureUpstream::start(Script {
        http: vec![completed()],
        ..Script::default()
    })
    .await;
    let auth_dir = AuthDir::new();
    let cfg = Config::parse(&format!(
        "auth-dir: {}\naccess:\n  api-keys: [client-key]\n",
        auth_dir.0.display()
    ))
    .unwrap();
    let rt = Arc::new(cpa_server::testing::runtime(cfg, Vec::new(), executors()));
    let mut pick = credential(&upstream.base_url);
    pick.attributes
        .insert(cpa_core::config::credentials::HOME_AUTH_INDEX.into(), "idx-home".into());
    let released = Arc::new(AtomicUsize::new(0));
    rt.set_remote_dispatch(Some(Arc::new(Home {
        picks: std::sync::Mutex::new(vec![pick]),
        released: released.clone(),
    })));
    let proxy = Proxy {
        url: serve(cpa_server::router(rt)).await,
        upstream,
        _auth_dir: auth_dir,
    };
    let (status, answer) = post(&proxy.url, "/v1/messages", &claude_request(false)).await;
    assert_eq!(status, 200, "{answer}");
    assert_claude_conversion(&sent(&proxy));
    for _ in 0..100 {
        if released.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(released.load(Ordering::SeqCst), 1, "the lease is released");
}
