//! Claude stop reasons and request faults end to end (#72): client -> cliproxy-rs ->
//! loopback mock Anthropic upstream, one API-key credential with a fake key in a
//! private `auth-dir`.
//!
//! - A refusal (`stop_reason: refusal`, sparse `stop_details`) is a successful provider
//!   result: native Messages answers carry the upstream bytes unchanged, Chat and
//!   Responses report it as a content filter, and nothing cools.
//! - `model_context_window_exceeded` keeps the partial output as a successful
//!   truncation.
//! - A prompt over the context window (HTTP 400 `invalid_request_error`) is a request
//!   fault: the client gets it and the credential keeps serving.
//! - A Fable-only weekly rejection without 5h headers cools only that model, so a
//!   sibling model on the same credential still serves.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use serde_json::{Value, json};

const FILE: &str = "claude-stop.json";

fn refusal_json() -> String {
    json!({"id":"msg_r","type":"message","role":"assistant","model":"claude-opus-4-6",
        "content":[{"type":"text","text":"partial"}],"stop_reason":"refusal","stop_sequence":null,
        "stop_details":{"type":"refusal","category":null,"explanation":null},
        "usage":{"input_tokens":5,"output_tokens":3}})
    .to_string()
}

fn sse(reason: &str, details: Option<Value>) -> String {
    let mut delta = json!({"stop_reason": reason, "stop_sequence": null});
    if let Some(d) = details {
        delta["stop_details"] = d;
    }
    [
        json!({"type":"message_start","message":{"id":"msg_s","type":"message","role":"assistant","model":"claude-opus-4-6","content":[],"stop_reason":null,"usage":{"input_tokens":5,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":delta,"usage":{"output_tokens":3}}),
        json!({"type":"message_stop"}),
    ]
    .iter()
    .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
    .collect()
}

#[derive(Default)]
struct Up {
    calls: AtomicUsize,
}

async fn upstream(State(up): State<Arc<Up>>, req: axum::extract::Request) -> Response {
    up.calls.fetch_add(1, Ordering::SeqCst);
    let body = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8_lossy(&body);
    let stream = serde_json::from_slice::<Value>(&body).unwrap()["stream"] == true;
    let sse_reply = |s: String| ([("content-type", "text/event-stream")], s).into_response();
    if text.contains("MODE_OVERFLOW") {
        let err = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 210000 tokens > 200000 maximum"},"request_id":"req_o"}"#;
        return (StatusCode::BAD_REQUEST, [("content-type", "application/json")], err).into_response();
    }
    if text.contains("MODE_FABLE_WEEKLY") {
        let err = r#"{"type":"error","error":{"type":"rate_limit_error","message":"weekly limit"}}"#;
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [
                ("content-type", "application/json"),
                ("anthropic-ratelimit-unified-status", "rejected"),
                ("anthropic-ratelimit-unified-7d-status", "allowed"),
                ("anthropic-ratelimit-unified-7d-utilization", "0.40"),
                ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
            ],
            err,
        )
            .into_response();
    }
    if text.contains("MODE_REFUSAL") {
        return if stream {
            sse_reply(sse("refusal", Some(json!({"type":"refusal"}))))
        } else {
            ([("content-type", "application/json")], refusal_json()).into_response()
        };
    }
    if text.contains("MODE_CONTEXT") {
        return sse_reply(sse("model_context_window_exceeded", None));
    }
    if stream {
        sse_reply(sse("end_turn", None))
    } else {
        let ok = r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-4-6","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;
        ([("content-type", "application/json")], ok).into_response()
    }
}

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

fn auth_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("claude-stop-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn proxy(auth_dir: &Path, upstream_url: &str) -> (String, Arc<cpa_server::Runtime>) {
    let yaml = format!("api-keys: [client-key]\nauth-dir: {}\n", auth_dir.display());
    let meta = json!({"type": "claude", "api_key": "sk-fake-claude", "base_url": upstream_url});
    std::fs::write(auth_dir.join(FILE), meta.to_string()).unwrap();
    let mut credential =
        Credential::from_file(auth_dir, &auth_dir.join(FILE), meta.as_object().unwrap().clone()).unwrap();
    credential.attributes.insert("api_key".into(), "sk-fake-claude".into());
    credential.attributes.insert("base_url".into(), upstream_url.into());
    let rt = Arc::new(cpa_server::testing::runtime(
        Config::parse(&yaml).unwrap(),
        vec![credential],
        Executors {
            claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            meta: Default::default(),
            xai: Default::default(),
        },
    ));
    let app = cpa_server::app(rt.clone(), axum::Router::new());
    (serve(app).await, rt)
}

async fn post(url: &str, path: &str, body: Value) -> (u16, Vec<u8>) {
    let response = wreq::Client::new()
        .post(format!("{url}{path}"))
        .header("x-api-key", "client-key")
        .header("authorization", "Bearer client-key")
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.bytes().await.unwrap().to_vec())
}

fn messages(model: &str, marker: &str, stream: bool) -> Value {
    json!({"model": model, "max_tokens": 64, "stream": stream,
        "messages": [{"role": "user", "content": marker}]})
}

fn chat(marker: &str, stream: bool) -> Value {
    json!({"model": "claude-opus-4-6", "stream": stream,
        "messages": [{"role": "user", "content": marker}]})
}

fn responses(marker: &str, stream: bool) -> Value {
    json!({"model": "claude-opus-4-6", "stream": stream, "input": marker})
}

/// The `data:` payloads of an SSE body, as JSON.
fn data(body: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(body)
        .lines()
        .filter_map(|l| l.strip_prefix("data: ").or_else(|| l.strip_prefix("data:")))
        .filter(|d| d.trim() != "[DONE]")
        .map(|d| serde_json::from_str(d.trim()).unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refusal_and_truncation_are_successful_results() {
    let dir = auth_dir("refusal");
    let up = Arc::new(Up::default());
    let upstream_url = serve(axum::Router::new().fallback(upstream).with_state(up.clone())).await;
    let (url, rt) = proxy(&dir, &upstream_url).await;

    // Native Messages: the upstream's bytes, buffered and streamed.
    let (status, body) = post(&url, "/v1/messages", messages("claude-opus-4-6", "MODE_REFUSAL", false)).await;
    assert_eq!(status, 200);
    assert_eq!(
        String::from_utf8(body).unwrap(),
        refusal_json(),
        "native refusal bytes unchanged"
    );
    let (status, body) = post(&url, "/v1/messages", messages("claude-opus-4-6", "MODE_REFUSAL", true)).await;
    assert_eq!(status, 200);
    assert_eq!(
        String::from_utf8(body).unwrap(),
        sse("refusal", Some(json!({"type":"refusal"}))),
        "native refusal stream unchanged"
    );

    // Chat Completions.
    let (status, body) = post(&url, "/v1/chat/completions", chat("MODE_REFUSAL", false)).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["choices"][0]["finish_reason"], "content_filter", "{body}");
    assert_eq!(body["choices"][0]["message"]["content"], "partial");
    let (status, body) = post(&url, "/v1/chat/completions", chat("MODE_REFUSAL", true)).await;
    assert_eq!(status, 200);
    let finishes: Vec<Value> = data(&body)
        .into_iter()
        .filter_map(|c| c["choices"][0]["finish_reason"].as_str().map(Value::from))
        .collect();
    assert_eq!(finishes, [json!("content_filter")]);

    // Responses.
    let (status, body) = post(&url, "/v1/responses", responses("MODE_REFUSAL", false)).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["status"], "incomplete", "{body}");
    assert_eq!(body["incomplete_details"]["reason"], "content_filter");
    assert_eq!(body["output"][0]["content"][0]["text"], "partial");
    let (status, body) = post(&url, "/v1/responses", responses("MODE_REFUSAL", true)).await;
    assert_eq!(status, 200);
    let events = data(&body);
    let last = events.last().unwrap();
    assert_eq!(last["type"], "response.incomplete", "{events:?}");
    assert_eq!(last["response"]["incomplete_details"]["reason"], "content_filter");

    // model_context_window_exceeded: a truncation that keeps its output.
    let (status, body) = post(&url, "/v1/chat/completions", chat("MODE_CONTEXT", false)).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["choices"][0]["finish_reason"], "length");
    assert_eq!(body["choices"][0]["message"]["content"], "partial");
    assert_eq!(body["usage"]["completion_tokens"], 3);
    let (status, body) = post(&url, "/v1/responses", responses("MODE_CONTEXT", true)).await;
    assert_eq!(status, 200);
    let events = data(&body);
    let last = events.last().unwrap();
    assert_eq!(last["type"], "response.incomplete");
    assert_eq!(last["response"]["incomplete_details"]["reason"], "max_output_tokens");
    assert_eq!(last["response"]["output"][0]["content"][0]["text"], "partial");

    assert_eq!(
        up.calls.load(Ordering::SeqCst),
        8,
        "every request reached the upstream once"
    );
    assert!(
        rt.store().cooldowns(FILE).is_empty(),
        "{:?}",
        rt.store().cooldowns(FILE)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_overflow_is_a_request_fault() {
    let dir = auth_dir("overflow");
    let up = Arc::new(Up::default());
    let upstream_url = serve(axum::Router::new().fallback(upstream).with_state(up.clone())).await;
    let (url, rt) = proxy(&dir, &upstream_url).await;

    for (path, body) in [
        ("/v1/messages", messages("claude-opus-4-6", "MODE_OVERFLOW", false)),
        ("/v1/chat/completions", chat("MODE_OVERFLOW", false)),
        ("/v1/responses", responses("MODE_OVERFLOW", false)),
    ] {
        let (status, reply) = post(&url, path, body).await;
        assert_eq!(status, 400, "{path}: {}", String::from_utf8_lossy(&reply));
        assert!(
            String::from_utf8_lossy(&reply).contains("prompt is too long"),
            "{path}: {}",
            String::from_utf8_lossy(&reply)
        );
        assert!(
            rt.store().cooldowns(FILE).is_empty(),
            "{path}: {:?}",
            rt.store().cooldowns(FILE)
        );
    }
    // The credential still serves.
    let (status, _) = post(&url, "/v1/messages", messages("claude-opus-4-6", "hi", false)).await;
    assert_eq!(status, 200);
    assert_eq!(up.calls.load(Ordering::SeqCst), 4, "one attempt each, no retry round");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fable_weekly_rejection_without_5h_headers_cools_only_that_model() {
    let dir = auth_dir("fable");
    let up = Arc::new(Up::default());
    let upstream_url = serve(axum::Router::new().fallback(upstream).with_state(up.clone())).await;
    let (url, rt) = proxy(&dir, &upstream_url).await;

    let (status, body) = post(
        &url,
        "/v1/messages",
        messages("claude-fable-5-1", "MODE_FABLE_WEEKLY", false),
    )
    .await;
    assert_eq!(status, 429, "{}", String::from_utf8_lossy(&body));
    let cooling = rt.store().cooldowns(FILE);
    assert!(!cooling.is_empty(), "the rejected model cools");
    assert!(
        cooling.iter().all(|c| !c.model.is_empty()),
        "no credential-wide cooldown without account-wide evidence: {cooling:?}"
    );
    let (status, body) = post(&url, "/v1/messages", messages("claude-opus-4-6", "hi", false)).await;
    assert_eq!(
        status,
        200,
        "a sibling model still serves: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(up.calls.load(Ordering::SeqCst), 2);
    let _ = std::fs::remove_dir_all(&dir);
}
