//! Functional contracts (first written for issue #67, H1) for the retained providers
//! Grok (`xai`) and Muse (`meta`) through the real router, each client format over HTTP
//! and, where the route streams, SSE; and the refusal of removed providers (#49) and
//! native plugins (#47).
//!
//! The upstream is `cpa_server::testing::codex_capture::CaptureUpstream`, the capture
//! server the Codex contract tests use: loopback only, recording each request as
//! received. Credentials are synthetic (`FAKE`), the `auth-dir` is private, and the
//! runtime comes from `cpa_server::testing::runtime`, which cannot leave the machine.
//!
//! Assertions are semantic: what reached the upstream (path, credential, model, stream
//! mode, the caller's text) and what reached the client (status, framing, text, terminal
//! event). Each check returns a `Result` so `the_semantic_checks_reject_a_mutated_capture`
//! can prove they fail on a real deviation instead of passing vacuously.

use std::sync::Arc;

use axum::http::HeaderMap;
use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_server::testing::codex_capture::{CaptureUpstream, Exchange, Reply, Script};
use serde_json::Value;

const CLIENT_KEY: &str = "client-key-FAKE";
const XAI_KEY: &str = "sk-xai-FAKE-LOCAL-ONLY";
const META_KEY: &str = "sk-meta-FAKE-LOCAL-ONLY";
const ANSWER: &str = "local answer";
const QUESTION: &str = "Local question";

/// A private, empty `auth-dir`, removed when dropped: credential loading never falls back
/// to `~/.cli-proxy-api`.
struct AuthDir(std::path::PathBuf);

impl AuthDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("cpa-it-retained-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for AuthDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Provider {
    Xai,
    Meta,
}

impl Provider {
    fn family(self) -> &'static str {
        match self {
            Provider::Xai => "xai",
            Provider::Meta => "meta",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Provider::Xai => XAI_KEY,
            Provider::Meta => META_KEY,
        }
    }

    /// A model the embedded catalog lists for the provider.
    fn model(self) -> &'static str {
        match self {
            Provider::Xai => "grok-4.7",
            Provider::Meta => "muse-spark-1.3",
        }
    }
}

/// A Responses event stream with one text output and a completed terminal event.
fn completed_stream(model: &str) -> String {
    let event = |kind: &str, data: Value| format!("event: {kind}\ndata: {data}\n\n");
    let message = serde_json::json!({
        "type": "message", "id": "msg_fixture", "role": "assistant", "status": "completed",
        "content": [{"type": "output_text", "text": ANSWER, "annotations": []}]
    });
    let response = |status: &str, output: Value| {
        serde_json::json!({
            "id": "resp_fixture", "object": "response", "created_at": 1_790_000_000,
            "status": status, "model": model, "output": output,
            "usage": {"input_tokens": 5, "output_tokens": 2, "total_tokens": 7}
        })
    };
    [
        event(
            "response.created",
            serde_json::json!({"type": "response.created", "sequence_number": 0,
                "response": response("in_progress", serde_json::json!([]))}),
        ),
        event(
            "response.output_item.added",
            serde_json::json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0,
                "item": {"type": "message", "id": "msg_fixture", "role": "assistant", "status": "in_progress", "content": []}}),
        ),
        event(
            "response.content_part.added",
            serde_json::json!({"type": "response.content_part.added", "sequence_number": 2, "item_id": "msg_fixture",
                "output_index": 0, "content_index": 0, "part": {"type": "output_text", "text": "", "annotations": []}}),
        ),
        event(
            "response.output_text.delta",
            serde_json::json!({"type": "response.output_text.delta", "sequence_number": 3, "item_id": "msg_fixture",
                "output_index": 0, "content_index": 0, "delta": ANSWER}),
        ),
        event(
            "response.output_text.done",
            serde_json::json!({"type": "response.output_text.done", "sequence_number": 4, "item_id": "msg_fixture",
                "output_index": 0, "content_index": 0, "text": ANSWER}),
        ),
        event(
            "response.content_part.done",
            serde_json::json!({"type": "response.content_part.done", "sequence_number": 5, "item_id": "msg_fixture",
                "output_index": 0, "content_index": 0, "part": {"type": "output_text", "text": ANSWER, "annotations": []}}),
        ),
        event(
            "response.output_item.done",
            serde_json::json!({"type": "response.output_item.done", "sequence_number": 6, "output_index": 0, "item": message}),
        ),
        event(
            "response.completed",
            serde_json::json!({"type": "response.completed", "sequence_number": 7,
                "response": response("completed", serde_json::json!([message]))}),
        ),
    ]
    .concat()
}

struct Proxy {
    url: String,
    upstream: CaptureUpstream,
    rt: Arc<cpa_server::Runtime>,
    _auth_dir: AuthDir,
}

/// A proxy with one configured API key for `provider`, pointed at a capture upstream.
async fn proxy(provider: Provider, replies: Vec<Reply>) -> Proxy {
    proxy_with(provider, replies, "").await
}

/// [`proxy`] with `extra` appended to its configuration.
async fn proxy_with(provider: Provider, replies: Vec<Reply>, extra: &str) -> Proxy {
    let upstream = CaptureUpstream::start(Script {
        http: replies,
        ..Script::default()
    })
    .await;
    let auth_dir = AuthDir::new();
    let cfg = Config::parse(&format!(
        "auth-dir: {}\naccess:\n  api-keys: [{CLIENT_KEY}]\nrouting:\n  retry:\n    request-retry: 0\napi-keys:\n  {}:\n    - name: {}-fixture\n      base-url: {}/v1\n      keys:\n        - api-key: {}\n{extra}",
        auth_dir.0.display(),
        provider.family(),
        provider.family(),
        upstream.base_url,
        provider.key(),
    ))
    .unwrap();
    let credentials = cpa_core::config::credentials::load(&cfg);
    assert_eq!(credentials.len(), 1, "exactly the one fixture credential");
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: Default::default(),
        meta: Default::default(),
        xai: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(cfg, credentials, executors));
    cpa_server::install_registry(&rt);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = cpa_server::router(rt.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Proxy {
        url,
        upstream,
        rt,
        _auth_dir: auth_dir,
    }
}

/// The client formats a retained provider is reached through.
#[derive(Clone, Copy, Debug)]
enum Client {
    /// OpenAI Responses (`/v1/responses`).
    Responses,
    /// OpenAI Chat Completions (`/v1/chat/completions`).
    Chat,
    /// Anthropic Messages (`/v1/messages`).
    Messages,
}

impl Client {
    fn path(self) -> &'static str {
        match self {
            Client::Responses => "/v1/responses",
            Client::Chat => "/v1/chat/completions",
            Client::Messages => "/v1/messages",
        }
    }

    fn body(self, model: &str, stream: bool) -> String {
        let value = match self {
            Client::Responses => serde_json::json!({"model": model, "input": QUESTION, "stream": stream}),
            Client::Chat => serde_json::json!({"model": model, "stream": stream,
                "messages": [{"role": "user", "content": QUESTION}]}),
            Client::Messages => serde_json::json!({"model": model, "max_tokens": 32, "stream": stream,
                "messages": [{"role": "user", "content": QUESTION}]}),
        };
        value.to_string()
    }
}

struct Answer {
    status: u16,
    headers: HeaderMap,
    body: String,
}

async fn send(proxy: &Proxy, client: Client, body: String) -> Answer {
    let mut request = wreq::Client::new()
        .post(format!("{}{}", proxy.url, client.path()))
        .header("content-type", "application/json");
    request = match client {
        Client::Messages => request
            .header("x-api-key", CLIENT_KEY)
            .header("anthropic-version", "2023-06-01"),
        _ => request.header("authorization", format!("Bearer {CLIENT_KEY}")),
    };
    let response = request.body(body).send().await.unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    Answer {
        status,
        headers,
        body: response.text().await.unwrap(),
    }
}

/// What the upstream must have received for one client request.
fn check_upstream(provider: Provider, seen: &[Exchange]) -> Result<(), String> {
    let [exchange] = seen else {
        return Err(format!("expected one upstream request, saw {}", seen.len()));
    };
    if exchange.method != axum::http::Method::POST || exchange.uri.path() != "/v1/responses" {
        return Err(format!("upstream request {} {}", exchange.method, exchange.uri));
    }
    let authorization = exchange
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if authorization != format!("Bearer {}", provider.key()) {
        return Err(format!("upstream authorization {authorization:?}"));
    }
    let mut head = String::new();
    for (name, value) in &exchange.headers {
        head.push_str(&format!("{name}: {}\n", String::from_utf8_lossy(value.as_bytes())));
    }
    let body_text = String::from_utf8_lossy(&exchange.body);
    if head.contains(CLIENT_KEY) || body_text.contains(CLIENT_KEY) {
        return Err("the client key reached the upstream".into());
    }
    let body: Value = serde_json::from_slice(&exchange.body).map_err(|e| format!("upstream body: {e}"))?;
    if body["model"] != provider.model() {
        return Err(format!("upstream model {}", body["model"]));
    }
    // Both executors always stream from the Responses API, also for buffered clients.
    if body["stream"] != true {
        return Err(format!("upstream stream {}", body["stream"]));
    }
    if !body_text.contains(QUESTION) {
        return Err("the caller's text did not reach the upstream".into());
    }
    Ok(())
}

/// The client-visible meaning of a successful answer.
fn check_answer(provider: Provider, client: Client, stream: bool, answer: &Answer) -> Result<(), String> {
    if answer.status != 200 {
        return Err(format!("status {}: {}", answer.status, answer.body));
    }
    if answer.body.contains(provider.key()) || answer.body.contains(XAI_KEY) || answer.body.contains(META_KEY) {
        return Err("an upstream key reached the client".into());
    }
    let content_type = answer
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if stream {
        if !content_type.starts_with("text/event-stream") {
            return Err(format!("stream content-type {content_type:?}"));
        }
        let events = sse_data(&answer.body);
        let mut text = String::new();
        let mut terminal = false;
        for data in &events {
            if data == "[DONE]" {
                terminal |= matches!(client, Client::Chat);
                continue;
            }
            let event: Value = serde_json::from_str(data).map_err(|e| format!("event {data}: {e}"))?;
            match client {
                Client::Responses => match event["type"].as_str() {
                    Some("response.output_text.delta") => text.push_str(event["delta"].as_str().unwrap_or_default()),
                    Some("response.completed") => terminal = true,
                    _ => {}
                },
                Client::Chat => {
                    if let Some(delta) = event["choices"][0]["delta"]["content"].as_str() {
                        text.push_str(delta);
                    }
                }
                Client::Messages => match event["type"].as_str() {
                    Some("content_block_delta") => text.push_str(event["delta"]["text"].as_str().unwrap_or_default()),
                    Some("message_stop") => terminal = true,
                    _ => {}
                },
            }
        }
        if text != ANSWER {
            return Err(format!("streamed text {text:?}"));
        }
        if !terminal {
            return Err("the stream has no terminal event".into());
        }
        return Ok(());
    }
    if !content_type.starts_with("application/json") {
        return Err(format!("buffered content-type {content_type:?}"));
    }
    let body: Value = serde_json::from_str(&answer.body).map_err(|e| format!("buffered body: {e}"))?;
    let (text, finished) = match client {
        Client::Responses => (
            body["output"][0]["content"][0]["text"].as_str(),
            body["status"] == "completed",
        ),
        Client::Chat => (
            body["choices"][0]["message"]["content"].as_str(),
            body["choices"][0]["finish_reason"] == "stop",
        ),
        Client::Messages => (body["content"][0]["text"].as_str(), body["stop_reason"] == "end_turn"),
    };
    if text != Some(ANSWER) {
        return Err(format!("buffered text {text:?} in {body}"));
    }
    if !finished {
        return Err(format!("buffered answer is not finished: {body}"));
    }
    Ok(())
}

/// Each event's `data` payload, in order.
fn sse_data(body: &str) -> Vec<String> {
    body.split('\n')
        .filter_map(|line| line.trim_end_matches('\r').strip_prefix("data:"))
        .map(|data| data.trim_start().to_owned())
        .collect()
}

async fn round_trip(provider: Provider, client: Client, stream: bool) {
    let proxy = proxy(provider, vec![Reply::sse(completed_stream(provider.model()))]).await;
    let answer = send(&proxy, client, client.body(provider.model(), stream)).await;
    let label = format!("{provider:?} {client:?} stream={stream}");
    check_answer(provider, client, stream, &answer).unwrap_or_else(|e| panic!("{label}: {e}"));
    check_upstream(provider, &proxy.upstream.http()).unwrap_or_else(|e| panic!("{label}: {e}"));
}

#[tokio::test]
async fn xai_responses_http_and_stream() {
    round_trip(Provider::Xai, Client::Responses, false).await;
    round_trip(Provider::Xai, Client::Responses, true).await;
}

#[tokio::test]
async fn xai_chat_completions_http_and_stream() {
    round_trip(Provider::Xai, Client::Chat, false).await;
    round_trip(Provider::Xai, Client::Chat, true).await;
}

#[tokio::test]
async fn xai_messages_http_and_stream() {
    round_trip(Provider::Xai, Client::Messages, false).await;
    round_trip(Provider::Xai, Client::Messages, true).await;
}

#[tokio::test]
async fn meta_responses_http_and_stream() {
    round_trip(Provider::Meta, Client::Responses, false).await;
    round_trip(Provider::Meta, Client::Responses, true).await;
}

#[tokio::test]
async fn meta_chat_completions_http_and_stream() {
    round_trip(Provider::Meta, Client::Chat, false).await;
    round_trip(Provider::Meta, Client::Chat, true).await;
}

#[tokio::test]
async fn meta_messages_http_and_stream() {
    round_trip(Provider::Meta, Client::Messages, false).await;
    round_trip(Provider::Meta, Client::Messages, true).await;
}

/// Turns on usage statistics for `proxy`.
fn record_usage(proxy: &Proxy) {
    proxy.rt.usage_queue().configure(
        true,
        &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
    );
}

/// The first `count` usage records `proxy` queued, then any that follow within 100 ms.
async fn usage_records(proxy: &Proxy, count: usize) -> Vec<Value> {
    let queue = proxy.rt.usage_queue();
    let mut records: Vec<Value> = Vec::new();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while records.len() < count {
            records.extend(queue.pop_oldest(10).iter().map(|r| serde_json::from_slice(r).unwrap()));
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    records.extend(queue.pop_oldest(10).iter().map(|r| serde_json::from_slice(r).unwrap()));
    records
}

/// #178: a turn that ends in `response.failed` keeps, in its record, the usage the
/// provider reported in that event (OpenAI's Responses streaming reference: the event
/// carries the `response`, `usage` included), buffered and streamed, for Grok and Muse.
/// The record is a failure, except for a Grok stream, which passes the event to the
/// client as any other ("xAI non-streaming responses" in DIFFERENCES-FROM-GO.md).
#[tokio::test]
async fn a_failed_response_keeps_the_usage_it_reported() {
    let created = r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_f","object":"response","status":"in_progress","output":[]}}"#;
    let failed = r#"{"type":"response.failed","sequence_number":1,"response":{"id":"resp_f","object":"response","status":"failed","output":[],"error":{"code":"server_error","message":"scripted failure"},"usage":{"input_tokens":11,"input_tokens_details":{"cached_tokens":4},"output_tokens":5,"output_tokens_details":{"reasoning_tokens":2},"total_tokens":16}}}"#;
    for provider in [Provider::Xai, Provider::Meta] {
        for stream in [false, true] {
            let label = format!("{provider:?} stream={stream}");
            let reply = Reply::sse(format!("data: {created}\n\ndata: {failed}\n\n"));
            let proxy = proxy(provider, vec![reply]).await;
            record_usage(&proxy);
            let _ = send(
                &proxy,
                Client::Responses,
                Client::Responses.body(provider.model(), stream),
            )
            .await;
            let records = usage_records(&proxy, 1).await;
            let [record] = records.as_slice() else {
                panic!("{label}: one record, got {records:?}");
            };
            let failed = !(provider == Provider::Xai && stream);
            assert_eq!(record["failed"], failed, "{label}: {record}");
            let tokens = &record["tokens"];
            assert_eq!(
                [
                    &tokens["input_tokens"],
                    &tokens["cached_tokens"],
                    &tokens["output_tokens"],
                    &tokens["reasoning_tokens"],
                    &tokens["total_tokens"],
                ],
                [11, 4, 5, 2, 16],
                "{label}: {record}"
            );
            assert!(record.get("usage_observed").is_none(), "{label}: {record}");
        }
    }
}

/// #178: a Grok record's `service_tier` is the tier the request carried upstream, after
/// translation and payload rules, not the client's: here the translation drops the
/// client's `flex`, an override sets `priority`, and a filter removes the tier again.
/// A request without a tier records the provider's default, `auto`.
#[tokio::test]
async fn xai_records_the_service_tier_sent_after_payload_rules() {
    let rule = |kind: &str, params: &str| {
        format!("payload:\n  {kind}:\n    - models: [{{name: \"grok-*\"}}]\n      params: {params}\n")
    };
    let cases = [
        (String::new(), "auto"),
        (rule("override", "{service_tier: priority}"), "priority"),
        (
            format!(
                "{}  filter:\n    - models: [{{name: \"grok-*\"}}]\n      params: [service_tier]\n",
                rule("override", "{service_tier: priority}")
            ),
            "auto",
        ),
    ];
    for (rules, want) in cases {
        for stream in [false, true] {
            let label = format!("{rules:?} stream={stream}");
            let reply = Reply::sse(completed_stream(Provider::Xai.model()));
            let proxy = proxy_with(Provider::Xai, vec![reply], &rules).await;
            record_usage(&proxy);
            let body = serde_json::json!({"model": Provider::Xai.model(), "input": QUESTION, "stream": stream, "service_tier": "flex"});
            let answer = send(&proxy, Client::Responses, body.to_string()).await;
            assert_eq!(answer.status, 200, "{label}: {}", answer.body);
            let [exchange] = proxy
                .upstream
                .http()
                .try_into()
                .unwrap_or_else(|_| panic!("{label}: one request"));
            let sent: Value = serde_json::from_slice(&exchange.body).unwrap();
            assert_eq!(
                sent["service_tier"].as_str().unwrap_or("auto"),
                want,
                "{label}: sent {sent}"
            );
            let records = usage_records(&proxy, 1).await;
            let [record] = records.as_slice() else {
                panic!("{label}: one record, got {records:?}");
            };
            assert_eq!(record["service_tier"], want, "{label}: {record}");
        }
    }
}

/// An upstream rate limit keeps its status for each provider and client format, and the
/// upstream key stays on the upstream side.
#[tokio::test]
async fn upstream_rate_limits_keep_their_status() {
    let error = r#"{"error":{"message":"scripted rate limit","type":"rate_limit_error","code":"rate_limited"}}"#;
    for provider in [Provider::Xai, Provider::Meta] {
        for client in [Client::Responses, Client::Chat, Client::Messages] {
            for stream in [false, true] {
                let proxy = proxy(provider, vec![Reply::json(429, error)]).await;
                let answer = send(&proxy, client, client.body(provider.model(), stream)).await;
                let label = format!("{provider:?} {client:?} stream={stream}");
                assert_eq!(answer.status, 429, "{label}: {}", answer.body);
                assert!(!answer.body.contains(provider.key()), "{label}");
                check_upstream(provider, &proxy.upstream.http()).unwrap_or_else(|e| panic!("{label}: {e}"));
            }
        }
    }
}

/// Negative control: the checks above fail on a deviation in each property they claim
/// to cover. A check that passed these mutations would be vacuous.
#[tokio::test]
async fn the_semantic_checks_reject_a_mutated_capture() {
    let proxy = proxy(Provider::Xai, vec![Reply::sse(completed_stream("grok-4.7"))]).await;
    let answer = send(&proxy, Client::Chat, Client::Chat.body("grok-4.7", false)).await;
    let seen = proxy.upstream.http();
    check_upstream(Provider::Xai, &seen).unwrap();
    check_answer(Provider::Xai, Client::Chat, false, &answer).unwrap();

    let mutate = |edit: &dyn Fn(&mut Exchange)| {
        let mut copy = seen.clone();
        edit(&mut copy[0]);
        check_upstream(Provider::Xai, &copy)
    };
    let replace = |from: &str, to: &str| {
        let from = from.to_owned();
        let to = to.to_owned();
        move |e: &mut Exchange| {
            let text = String::from_utf8(e.body.to_vec()).unwrap().replacen(&from, &to, 1);
            e.body = text.into();
        }
    };
    assert!(mutate(&replace("\"grok-4.7\"", "\"grok-4.6\"")).is_err(), "model");
    assert!(
        mutate(&replace("\"stream\":true", "\"stream\":false")).is_err(),
        "stream mode"
    );
    assert!(mutate(&replace(QUESTION, "Other question")).is_err(), "caller text");
    assert!(
        mutate(&|e: &mut Exchange| {
            e.headers
                .insert("authorization", format!("Bearer {CLIENT_KEY}").parse().unwrap());
        })
        .is_err(),
        "client key forwarded"
    );
    assert!(check_upstream(Provider::Xai, &[]).is_err(), "no upstream request");

    let changed = |body: String, status: u16| Answer {
        status,
        headers: answer.headers.clone(),
        body,
    };
    assert!(
        check_answer(Provider::Xai, Client::Chat, false, &changed(answer.body.clone(), 502)).is_err(),
        "status"
    );
    assert!(
        check_answer(
            Provider::Xai,
            Client::Chat,
            false,
            &changed(answer.body.replace(ANSWER, "other answer"), 200)
        )
        .is_err(),
        "text"
    );
    assert!(
        check_answer(
            Provider::Xai,
            Client::Chat,
            false,
            &changed(answer.body.replace(ANSWER, XAI_KEY), 200)
        )
        .is_err(),
        "upstream key leaked"
    );
}

/// Removed providers (#49) stay refused: their client routes answer 410 with migration
/// guidance through the real router, their settings fail to load, and their model names
/// never reach a retained upstream.
#[tokio::test]
async fn removed_provider_routes_settings_and_models_stay_refused() {
    let proxy = proxy(Provider::Xai, vec![]).await;
    let client = wreq::Client::new();
    for (method, path) in [
        ("POST", "/v1beta/models/gemini-2.5-pro:generateContent"),
        ("GET", "/v1beta/models"),
        ("POST", "/v1beta/interactions"),
        ("GET", "/v1/ws"),
        ("GET", "/antigravity/callback"),
        ("GET", "/devin/callback"),
    ] {
        let response = client
            .request(method.parse().unwrap(), format!("{}{path}", proxy.url))
            .header("authorization", format!("Bearer {CLIENT_KEY}"))
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let body: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
        assert_eq!(status, 410, "{method} {path}");
        assert_eq!(body["error"]["code"], "removed_provider", "{method} {path}");
    }
    for model in ["gemini-2.5-pro", "kimi-k2", "antigravity-claude"] {
        let answer = send(&proxy, Client::Chat, Client::Chat.body(model, false)).await;
        assert!(!(200..300).contains(&answer.status), "{model}: {}", answer.body);
    }
    assert!(proxy.upstream.http().is_empty(), "a removed model reached the upstream");
    for yaml in [
        "api-keys:\n  gemini:\n    - keys: [{api-key: k}]\n",
        "api-keys:\n  vertex:\n    - keys: [{api-key: k}]\n",
        "oauth:\n  excluded-models:\n    antigravity: [a]\n",
        "openai-compatibility:\n  - name: x\n    base-url: http://127.0.0.1:1\n",
    ] {
        let error = Config::parse(yaml).expect_err(yaml).to_string();
        assert!(error.contains(cpa_core::removed::GUIDANCE), "{yaml}: {error}");
    }
}

/// Native plugins (#47) stay refused: any `plugins` section fails the config load with
/// the migration error. `management.rs` covers the retired plugin management routes.
#[tokio::test]
async fn plugin_settings_stay_refused() {
    for yaml in [
        "plugins: {enabled: true, dir: plugins}\n",
        "plugins: {enabled: false}\n",
        "plugins: {}\n",
    ] {
        let error = Config::parse(yaml).expect_err(yaml).to_string();
        assert_eq!(error, cpa_core::config::PLUGINS_REMOVED, "{yaml}");
    }
}
