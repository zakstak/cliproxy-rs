//! The Codex translated route through the real router (#27): Claude Messages and Chat
//! Completions requests served by a Codex credential on a native profile
//! (`docs/codex/CONTRACT.md` "Translated route"). The Gemini and Interactions sources
//! went with those client protocols (issue #49).
//!
//! Each source request is hand-authored from its provider's documented request shape:
//! Anthropic Messages (`system`, `thinking`, `tools`, `tool_use`/`tool_result` blocks),
//! and OpenAI Chat Completions (`messages` with `tool_calls` and `tool` messages, `tools`,
//! `parallel_tool_calls`). The upstream answer is the #21 first-party fixture
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
        meta: Default::default(),
        xai: Default::default(),
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

/// Raw source duplicates survive the test ingress: serializing Value would erase
/// the condition that previously discarded an unrelated, valid cache identity.
#[tokio::test]
async fn translated_duplicate_tier_keeps_valid_cache_identity() {
    let mut control = None;
    for tier in [
        r#""service_tier":"flex""#,
        r#""service_tier":"flex","service_tier":"flex""#,
        r#""service_tier":"flex","service_\u0074ier":"flex""#,
    ] {
        let body = format!(
            r#"{{"model":"gpt-5.5","messages":[{{"role":"user","content":"hi"}}],{tier},"prompt_cache_key":"cache-FAKE"}}"#
        );
        let proxy = proxy(vec![completed()], "").await;
        let response = wreq::Client::new()
            .post(format!("{}/v1/chat/completions", proxy.url))
            .header("authorization", "Bearer client-key")
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        response.bytes().await.unwrap();
        let sent = proxy.upstream.http();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].headers["session-id"], "cache-FAKE", "{tier}");
        match &control {
            Some(body) => assert_eq!(&sent[0].body, body, "translation stays identical"),
            None => control = Some(sent[0].body.clone()),
        }
    }
    let proxy = proxy(vec![completed()], "").await;
    let response = wreq::Client::new()
        .post(format!("{}/v1/responses", proxy.url))
        .header("authorization", "Bearer client-key")
        .header("content-type", "application/json")
        .body(r#"{"model":"gpt-5.5","input":[],"service_tier":"flex","service_tier":"flex","prompt_cache_key":"cache-FAKE"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 400);
    assert!(proxy.upstream.http().is_empty(), "native duplicates stay refused");
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
/// part and an empty audio part.
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
    let cases = [
        ("/v1/messages", redacted, "redacted_thinking"),
        ("/v1/messages", document, "document"),
        ("/v1/chat/completions", refusal, "refusal"),
        ("/v1/chat/completions", audio, "input_audio"),
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

/// #80: a Responses-shaped Chat Completions body keeps its developer messages as
/// developer items, in place, on the translated route. The pinned client sends developer
/// messages itself (CONTRACT.md "Retained conversions"); Go v8.0.35 sends them as user
/// text (docs/DIFFERENCES-FROM-GO.md).
#[tokio::test]
async fn responses_shaped_chat_keeps_developer_messages() {
    for stream in [true, false] {
        let proxy = proxy(vec![completed()], "").await;
        let request = json!({"model": "gpt-5.5", "stream": stream, "input": [
            {"type": "message", "role": "developer", "content": "leading instruction"},
            {"type": "message", "role": "user", "content": "question"},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "prior answer"}]},
            {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "later instruction"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "continue"}]},
        ]});
        let (status, answer) = post(&proxy.url, "/v1/chat/completions", &request).await;
        assert_eq!(status, 200, "stream={stream}: {answer}");
        assert!(answer.contains("Done."), "stream={stream}: {answer}");
        let body = sent(&proxy);
        assert_nothing_added(&body, &[]);
        let messages: Vec<(&str, &str, &str)> = items(&body, "message")
            .into_iter()
            .map(|m| {
                let part = &m["content"][0];
                (
                    m["role"].as_str().unwrap(),
                    part["type"].as_str().unwrap(),
                    part["text"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            messages,
            [
                ("developer", "input_text", "leading instruction"),
                ("user", "input_text", "question"),
                ("assistant", "output_text", "prior answer"),
                ("developer", "input_text", "later instruction"),
                ("user", "input_text", "continue"),
            ],
            "stream={stream}: {body}"
        );
    }
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

// Review 5812377412: generation controls, nested content, schemas, call IDs and images.

/// [`proxy`], for tests that keep a `proxy` binding.
async fn make(replies: Vec<Reply>, extra_config: &str) -> Proxy {
    proxy(replies, extra_config).await
}

/// Explicit generation controls reach the upstream as their documented Responses fields
/// (`max_output_tokens`, `temperature`, `top_p`, ...), and a control with no Responses
/// equivalent is refused before anything is sent.
#[tokio::test]
async fn generation_controls_map_or_are_refused() {
    let mut claude = claude_request(false);
    claude["max_tokens"] = json!(1);
    claude["temperature"] = json!(0.25);
    claude["top_p"] = json!(0.5);
    let mut chat = chat_request(false);
    chat["max_completion_tokens"] = json!(7);
    chat["temperature"] = json!(0.25);
    chat["prompt_cache_key"] = json!("client-key-for-caching");
    let mapped = [
        (
            "/v1/messages",
            claude,
            json!({"max_output_tokens": 1, "temperature": 0.25, "top_p": 0.5}),
        ),
        (
            "/v1/chat/completions",
            chat,
            json!({"max_output_tokens": 7, "temperature": 0.25, "prompt_cache_key": "client-key-for-caching"}),
        ),
    ];
    for (path, request, want) in mapped {
        let proxy = make(vec![completed()], "").await;
        let (status, answer) = post(&proxy.url, path, &request).await;
        assert_eq!(status, 200, "{path}: {answer}");
        let body = sent(&proxy);
        for (field, value) in want.as_object().unwrap() {
            assert_eq!(&body[field], value, "{path}: {field} in {body}");
        }
    }
    let mut top_k = claude_request(false);
    top_k["top_k"] = json!(5);
    let mut stop = claude_request(false);
    stop["stop_sequences"] = json!(["END"]);
    let mut chat_stop = chat_request(false);
    chat_stop["stop"] = json!(["END"]);
    let mut chat_n = chat_request(false);
    chat_n["n"] = json!(2);
    let mut chat_penalty = chat_request(false);
    chat_penalty["presence_penalty"] = json!(0.5);
    let mut chat_seed = chat_request(false);
    chat_seed["seed"] = json!(42);
    let refused = [
        ("/v1/messages", top_k, "top_k"),
        ("/v1/messages", stop, "stop_sequences"),
        ("/v1/chat/completions", chat_stop, "stop"),
        ("/v1/chat/completions", chat_n, "`n`"),
        ("/v1/chat/completions", chat_penalty, "presence_penalty"),
        ("/v1/chat/completions", chat_seed, "seed"),
    ];
    for (path, request, field) in refused {
        let proxy = make(vec![completed()], "").await;
        let (status, answer) = post(&proxy.url, path, &request).await;
        assert_eq!(status, 400, "{path} {field}: {answer}");
        assert!(
            answer.contains("unsupported conversion") && answer.contains(field),
            "{path}: {answer}"
        );
        assert!(proxy.upstream.http().is_empty(), "{path} {field}: nothing was sent");
    }
}

/// Nested and handled content is kept or refused, never partly sent: a tool result with
/// text and an image URL keeps both; a tool result with text and an unsupported block, a
/// thinking block with reasoning text but no Codex signature, a non-text system block, a
/// Chat result without its call and a Chat call of an unknown type are refused before the
/// send.
#[tokio::test]
async fn nested_content_is_kept_or_refused() {
    let mut kept = claude_request(false);
    kept["messages"][2]["content"][0]["content"] = json!([
        {"type": "text", "text": "18C"},
        {"type": "image", "source": {"type": "url", "url": "https://example.com/chart.png"}}
    ]);
    let proxy = make(vec![completed()], "").await;
    let (status, answer) = post(&proxy.url, "/v1/messages", &kept).await;
    assert_eq!(status, 200, "{answer}");
    let body = sent(&proxy);
    let output = &items(&body, "function_call_output")[0]["output"];
    assert_eq!(output[0]["text"], "18C", "{body}");
    assert_eq!(output[1]["image_url"], "https://example.com/chart.png", "{body}");

    let mut mixed = claude_request(false);
    mixed["messages"][2]["content"][0]["content"] = json!([
        {"type": "text", "text": "18C"},
        {"type": "search_result", "source": "https://example.com", "title": "t", "content": []}
    ]);
    let mut foreign = claude_request(false);
    foreign["messages"][1]["content"][0] =
        json!({"type": "thinking", "thinking": "I should call the tool.", "signature": "EqQBCkgIBxABGAIqQKlaude"});
    let mut system = claude_request(false);
    system["system"] = json!([{"type": "text", "text": "Be brief."}, {"type": "image", "source": {"type": "url", "url": "https://example.com/x.png"}}]);
    let orphan_result = json!({"model": "gpt-5.5", "messages": [
        {"role": "user", "content": "hi"},
        {"role": "tool", "tool_call_id": "call_missing", "content": "18C"}
    ]});
    let unknown_call = json!({"model": "gpt-5.5", "messages": [
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": null, "tool_calls": [{"id": "call_1", "type": "computer", "computer": {}}]},
        {"role": "user", "content": "go on"}
    ]});
    let cases = [
        ("/v1/messages", mixed, "search_result"),
        ("/v1/messages", foreign, "thinking"),
        ("/v1/messages", system, "system block"),
        (
            "/v1/chat/completions",
            orphan_result,
            "tool message without a matching tool call",
        ),
        ("/v1/chat/completions", unknown_call, "computer"),
    ];
    for (path, request, part) in cases {
        let proxy = make(vec![completed()], "").await;
        let (status, answer) = post(&proxy.url, path, &request).await;
        assert_eq!(status, 400, "{path} {part}: {answer}");
        assert!(
            answer.contains("unsupported conversion") && answer.contains(part),
            "{path}: {answer}"
        );
        assert!(proxy.upstream.http().is_empty(), "{path} {part}: nothing was sent");
    }
}

/// A caller's tool schema reaches the upstream as sent: `$schema`, a nested `pattern`
/// with a `\p{...}` escape, `patternProperties` and an unknown keyword stay, and no
/// `additionalProperties` is added.
#[tokio::test]
async fn tool_schemas_keep_their_constraints() {
    let schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {
            "name": {"type": "string", "pattern": "^\\p{Lu}\\p{Ll}+$"},
            "tags": {"type": "object", "patternProperties": {"^\\p{L}+$": {"type": "string"}}},
            "limit": {"type": "integer", "x-unit": "items"}
        },
        "required": ["name"]
    });
    let mut claude = claude_request(false);
    claude["tools"][0]["input_schema"] = schema.clone();
    let proxy = make(vec![completed()], "").await;
    let (status, answer) = post(&proxy.url, "/v1/messages", &claude).await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(sent(&proxy)["tools"][0]["parameters"], schema, "the caller's schema");
}

/// A call ID longer than 64 bytes reaches the upstream unchanged on both the call and
/// its result, so they still correlate.
#[tokio::test]
async fn long_call_ids_stay_unchanged() {
    let id = format!("toolu_{}", "x".repeat(100));
    let mut claude = claude_request(false);
    claude["messages"][1]["content"][1]["id"] = json!(id);
    claude["messages"][2]["content"][0]["tool_use_id"] = json!(id);
    let proxy = make(vec![completed()], "").await;
    let (status, answer) = post(&proxy.url, "/v1/messages", &claude).await;
    assert_eq!(status, 200, "{answer}");
    let body = sent(&proxy);
    assert_eq!(items(&body, "function_call")[0]["call_id"], json!(id));
    assert_eq!(items(&body, "function_call_output")[0]["call_id"], json!(id));

    let call = format!("call_{}", "y".repeat(100));
    let mut chat = chat_request(false);
    chat["messages"][2]["tool_calls"][0]["id"] = json!(call);
    chat["messages"][3]["tool_call_id"] = json!(call);
    let proxy = make(vec![completed()], "").await;
    let (status, answer) = post(&proxy.url, "/v1/chat/completions", &chat).await;
    assert_eq!(status, 200, "{answer}");
    let body = sent(&proxy);
    assert_eq!(items(&body, "function_call")[0]["call_id"], json!(call));
    assert_eq!(items(&body, "function_call_output")[0]["call_id"], json!(call));
}

const IMAGE_RULES: &str = "payload:\n  override:\n    - models: [{name: \"*\"}]\n      params: {model: policy/image-model, instructions: injected, reasoning.effort: high, \"audit.-1\": once}\n";

/// A direct Images request (an API-key credential with a configured image model) reaches
/// the provider's Images endpoint as the client sent it, with only its model routed:
/// payload rules cannot rewrite it.
#[tokio::test]
async fn direct_images_are_not_rewritten() {
    let upstream = CaptureUpstream::start(Script {
        http: vec![Reply::json(
            200,
            r#"{"created":1713833628,"data":[{"b64_json":"AA=="}]}"#,
        )],
        ..Script::default()
    })
    .await;
    let auth_dir = AuthDir::new();
    let cfg = Config::parse(&format!(
        "auth-dir: {}\naccess:\n  api-keys: [client-key]\ncodex-api-key:\n  - api-key: key-FAKE\n    base-url: {}\n    models:\n      - name: gpt-image-2\n{IMAGE_RULES}",
        auth_dir.0.display(),
        upstream.base_url
    ))
    .unwrap();
    let credentials = cpa_core::config::credentials::from_config(&cfg);
    let rt = Arc::new(cpa_server::testing::runtime(cfg, credentials, executors()));
    let proxy = Proxy {
        url: serve(cpa_server::router(rt)).await,
        upstream,
        _auth_dir: auth_dir,
    };
    let (status, answer) = post(
        &proxy.url,
        "/v1/images/generations",
        &json!({"model": "gpt-image-2", "prompt": "a cat"}),
    )
    .await;
    assert_eq!(status, 200, "{answer}");
    let http = proxy.upstream.http();
    assert_eq!(http[0].uri.path(), "/images/generations");
    let body: Value = serde_json::from_slice(&http[0].body).unwrap();
    assert_eq!(body["model"], "gpt-image-2", "{body}");
    assert_eq!(body["prompt"], "a cat");
    assert!(
        body.get("audit").is_none() && body.get("instructions").is_none(),
        "{body}"
    );
}

// Review 5812490964: value-level controls, failed tool results.

/// Controls the converter handles are checked by value: Chat JSON mode becomes the
/// Responses JSON format and any Chat tier is kept; a Claude tier, tool choice or
/// thinking type with no Responses form is refused.
#[tokio::test]
async fn converted_controls_are_checked_by_value() {
    let mut json_mode = chat_request(false);
    json_mode["response_format"] = json!({"type": "json_object"});
    json_mode["service_tier"] = json!("flex");
    let proxy = make(vec![completed()], "").await;
    let (status, answer) = post(&proxy.url, "/v1/chat/completions", &json_mode).await;
    assert_eq!(status, 200, "{answer}");
    let body = sent(&proxy);
    assert_eq!(body["text"]["format"], json!({"type": "json_object"}), "{body}");
    assert_eq!(body["service_tier"], "flex", "{body}");

    let mut tier = claude_request(false);
    tier["service_tier"] = json!("standard_only");
    let mut choice = claude_request(false);
    choice["tool_choice"] = json!({"type": "bogus"});
    let mut thinking = claude_request(false);
    thinking["thinking"] = json!({"type": "interleaved"});
    let mut format = chat_request(false);
    format["response_format"] = json!({"type": "grammar"});
    for (path, request, field) in [
        ("/v1/messages", tier, "service_tier"),
        ("/v1/messages", choice, "tool_choice"),
        ("/v1/messages", thinking, "thinking"),
        ("/v1/chat/completions", format, "response_format"),
    ] {
        let proxy = make(vec![completed()], "").await;
        let (status, answer) = post(&proxy.url, path, &request).await;
        assert_eq!(status, 400, "{path} {field}: {answer}");
        assert!(
            answer.contains("unsupported conversion") && answer.contains(field),
            "{answer}"
        );
        assert!(proxy.upstream.http().is_empty(), "{field}: nothing was sent");
    }
}

/// A failed Claude tool result (`is_error: true`) has no Responses form and is refused
/// before the send, with string or block content; an ordinary result is sent.
#[tokio::test]
async fn failed_tool_results_are_refused() {
    for content in [json!("ENOENT"), json!([{"type": "text", "text": "ENOENT"}])] {
        let mut request = claude_request(false);
        request["messages"][2]["content"][0]["content"] = content.clone();
        request["messages"][2]["content"][0]["is_error"] = json!(true);
        let proxy = make(vec![completed()], "").await;
        let (status, answer) = post(&proxy.url, "/v1/messages", &request).await;
        assert_eq!(status, 400, "{content}: {answer}");
        assert!(answer.contains("is_error"), "{answer}");
        assert!(proxy.upstream.http().is_empty(), "{content}: nothing was sent");
        request["messages"][2]["content"][0]["is_error"] = json!(false);
        let proxy = make(vec![completed()], "").await;
        let (status, answer) = post(&proxy.url, "/v1/messages", &request).await;
        assert_eq!(status, 200, "{content}: an ordinary result: {answer}");
    }
}

/// #80: a Claude `output_config.effort` without a `thinking` type has no Responses form,
/// so it is refused before the send, buffered and streamed, instead of being dropped.
/// Beside adaptive thinking it is the client's reasoning effort.
#[tokio::test]
async fn standalone_claude_effort_is_refused_before_the_send() {
    for stream in [false, true] {
        let proxy = proxy(vec![completed()], "").await;
        let mut request = claude_request(stream);
        request.as_object_mut().unwrap().remove("thinking");
        request["output_config"] = json!({"effort": "low"});
        let (status, answer) = post(&proxy.url, "/v1/messages", &request).await;
        assert_eq!(status, 400, "stream={stream}: {answer}");
        assert!(
            answer.contains("`output_config.effort` without a `thinking` type"),
            "stream={stream}: {answer}"
        );
        assert!(proxy.upstream.http().is_empty(), "stream={stream}: nothing was sent");
    }
    let proxy = proxy(vec![completed()], "").await;
    let mut request = claude_request(false);
    request["thinking"] = json!({"type": "adaptive"});
    request["output_config"] = json!({"effort": "low"});
    let (status, answer) = post(&proxy.url, "/v1/messages", &request).await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(sent(&proxy)["reasoning"]["effort"], "low");
}

/// A `response.created`, a text delta when `text` is not empty, then a
/// `response.incomplete` with `details` as its `incomplete_details`.
fn incomplete(details: &str, text: &str) -> Reply {
    let mut sse = String::from(
        "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
    );
    let mut output = String::from("[]");
    if !text.is_empty() {
        sse.push_str(&format!(
            "event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"delta\":\"{text}\"}}\n\n"
        ));
        output = format!(
            r#"[{{"type":"message","id":"msg_1","status":"incomplete","role":"assistant","content":[{{"type":"output_text","text":"{text}"}}]}}]"#
        );
    }
    sse.push_str(&format!(
        "event: response.incomplete\ndata: {{\"type\":\"response.incomplete\",\"response\":{{\"id\":\"resp_1\",\"status\":\"incomplete\",\"incomplete_details\":{details},\"output\":{output},\"usage\":{{\"input_tokens\":10,\"output_tokens\":2,\"total_tokens\":12}}}}}}\n\n"
    ));
    Reply::sse(sse)
}

/// A `response.created`, a function call cut off after `{"city":`, then a
/// `response.incomplete` with `reason`. With `done`, the call item is also reported done
/// before the terminal event, as an upstream may; otherwise only the terminal output
/// carries it.
fn incomplete_call(reason: &str, done: bool) -> Reply {
    let item = r#"{"type":"function_call","id":"fc_1","status":"incomplete","name":"get_weather","call_id":"call_9","arguments":"{\"city\":"}"#;
    let mut sse = String::from(
        "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
    );
    sse.push_str(&format!(
        "event: response.output_item.added\ndata: {{\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{}}}\n\n",
        item.replace(r#""arguments":"{\"city\":""#, r#""arguments":"""#)
    ));
    sse.push_str("event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"output_index\":0,\"delta\":\"{\\\"city\\\":\"}\n\n");
    if done {
        sse.push_str(&format!(
            "event: response.output_item.done\ndata: {{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{item}}}\n\n"
        ));
    }
    sse.push_str(&format!(
        "event: response.incomplete\ndata: {{\"type\":\"response.incomplete\",\"response\":{{\"id\":\"resp_1\",\"status\":\"incomplete\",\"incomplete_details\":{{\"reason\":\"{reason}\"}},\"output\":[{item}],\"usage\":{{\"input_tokens\":10,\"output_tokens\":2,\"total_tokens\":12}}}}}}\n\n"
    ));
    Reply::sse(sse)
}

/// The stop a Claude and a Chat client read from `answer`: Claude `stop_reason`, Chat
/// `finish_reason`, streamed or buffered.
fn client_stop(answer: &str) -> Option<String> {
    let values: Vec<Value> = match serde_json::from_str::<Value>(answer) {
        Ok(value) => vec![value],
        Err(_) => answer
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|d| serde_json::from_str(d).ok())
            .collect(),
    };
    values.iter().find_map(|v| {
        [
            &v["stop_reason"],
            &v["delta"]["stop_reason"],
            &v["choices"][0]["finish_reason"],
        ]
        .into_iter()
        .find_map(|s| s.as_str().map(str::to_owned))
    })
}

/// A `response.incomplete` is never a normal stop on the translated route
/// (`docs/codex/CONTRACT.md` "Translated route", Responses). A reason the client API
/// has a stop for becomes that stop: `max_output_tokens` is Claude `max_tokens` and Chat
/// `length`, `content_filter` is Claude `refusal` and Chat `content_filter`. Any other
/// reason, with or without output, has none in Claude Messages or Chat Completions, so
/// the response fails with an upstream error that names the reason: the first-party
/// `sse-incomplete-partial` (an `invalid_recipient` stop inside a call), an unknown or
/// missing reason, and `interrupted`, which the first-party client reads as a turn that
/// did not end (`codex-api/src/sse/responses.rs:451-506`). An incomplete response that
/// carries a tool call fails whatever its reason.
#[tokio::test]
async fn incomplete_responses_keep_their_meaning() {
    let partial = Reply::sse(fixture("sse-incomplete-partial").bytes());
    let mapped = [
        (r#"{"reason":"max_output_tokens"}"#, "max_tokens", "length"),
        (r#"{"reason":"content_filter"}"#, "refusal", "content_filter"),
    ];
    for stream in [true, false] {
        for (details, claude, chat) in mapped {
            for (path, body, stop) in [
                ("/v1/messages", claude_request(stream), claude),
                ("/v1/chat/completions", chat_request(stream), chat),
            ] {
                let proxy = proxy(vec![incomplete(details, "Partial")], "").await;
                let (status, answer) = post(&proxy.url, path, &body).await;
                assert_eq!(status, 200, "{path} stream={stream} {details}: {answer}");
                assert_eq!(
                    client_stop(&answer).as_deref(),
                    Some(stop),
                    "{path} stream={stream}: {answer}"
                );
                assert!(answer.contains("Partial"), "the partial output stays: {answer}");
            }
        }
        let failing = [
            (partial.clone(), "invalid_recipient"),
            (incomplete(r#"{"reason":"unknown"}"#, ""), "unknown"),
            (incomplete("null", "Partial"), "unknown"),
            (incomplete(r#"{"reason":"interrupted"}"#, "Partial"), "interrupted"),
            // A Claude client would read a tool call in an incomplete response as a
            // `tool_use` stop, and a buffered one would get `{}` for the cut-off
            // arguments, so a call fails whatever the reason.
            (incomplete_call("max_output_tokens", true), "max_output_tokens"),
            (incomplete_call("max_output_tokens", false), "max_output_tokens"),
            (incomplete_call("content_filter", true), "content_filter"),
        ];
        for (reply, reason) in failing {
            for (path, body) in [
                ("/v1/messages", claude_request(stream)),
                ("/v1/chat/completions", chat_request(stream)),
            ] {
                let proxy = proxy(vec![reply.clone()], "").await;
                let (status, answer) = post(&proxy.url, path, &body).await;
                assert_eq!(
                    client_stop(&answer),
                    None,
                    "{path} stream={stream} {reason}: no normal stop: {answer}"
                );
                assert!(
                    answer.contains(&format!("upstream response incomplete: {reason}")),
                    "{path} stream={stream}: the error names the reason: {answer}"
                );
                if !stream {
                    assert_eq!(status, 502, "{path} {reason}: {answer}");
                    assert!(!answer.contains("tool_use\""), "no call from a cut-off item: {answer}");
                }
                assert_eq!(proxy.upstream.http().len(), 1, "never retried: {path} {reason}");
            }
        }
    }
}

/// The outbound credential guard covers the translated route
/// (`docs/codex/CONTRACT.md` "Credential isolation"): an upstream that writes the
/// credential it was sent into its answer never hands it to the client. An error keeps
/// its status and loses its body, a streamed client event that carries it is withheld
/// while the rest of the stream arrives, and a buffered answer that carries it fails.
#[tokio::test]
async fn the_selected_credential_never_reaches_a_translated_client() {
    let echo = |status| {
        Reply::json(
            status,
            r#"{"error":{"type":"invalid_request_error","message":"bad token at-FAKE"}}"#,
        )
    };
    let leaking = || {
        Reply::sse(
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n\
event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"delta\":\"Hello\"}\n\n\
event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"delta\":\"at-FAKE\"}\n\n\
event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"msg_1\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"Helloat-FAKE\"}]}],\"usage\":{\"input_tokens\":10,\"output_tokens\":2,\"total_tokens\":12}}}\n\n",
        )
    };
    for stream in [true, false] {
        for (path, body) in [
            ("/v1/messages", claude_request(stream)),
            ("/v1/chat/completions", chat_request(stream)),
        ] {
            let rejecting = proxy(vec![echo(400)], "").await;
            let (status, answer) = post(&rejecting.url, path, &body).await;
            assert_eq!(status, 400, "{path} stream={stream}: the classified status: {answer}");
            assert!(!answer.contains("at-FAKE"), "{path} stream={stream}: {answer}");

            let answering = proxy(vec![leaking()], "").await;
            let (status, answer) = post(&answering.url, path, &body).await;
            assert!(!answer.contains("at-FAKE"), "{path} stream={stream}: {answer}");
            if stream {
                assert_eq!(status, 200, "{answer}");
                assert!(answer.contains("Hello"), "the rest of the stream arrives: {answer}");
                assert!(client_stop(&answer).is_some(), "the stream completes: {answer}");
            } else {
                assert_eq!(status, 502, "{path}: {answer}");
            }
        }
    }
}
