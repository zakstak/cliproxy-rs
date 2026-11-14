//! Claude thinking-token usage end to end (#80): client -> cliproxy-rs -> loopback mock
//! Anthropic upstream, one API-key credential with a fake key in a private `auth-dir`.
//!
//! Anthropic reports the reasoning share of `usage.output_tokens` as
//! `usage.output_tokens_details.thinking_tokens` (anthropic-sdk-python 5139cf2e,
//! `types/usage.py`, `types/output_tokens_details.py`). A Chat Completions client gets
//! it as `usage.completion_tokens_details.reasoning_tokens`, buffered and streamed,
//! with an explicit 0 kept and an unreported count left out. Native Messages answers
//! carry the upstream bytes unchanged. The mock writes its streams in small pieces
//! that split events and lines, so the count also survives the proxy's SSE framing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use serde_json::{Value, json};

const FILE: &str = "claude-usage.json";

/// The `output_tokens_details` a marker in the prompt selects, `None` for none.
fn details(prompt: &str) -> Option<Value> {
    if prompt.contains("MODE_SEVEN") {
        Some(json!({"thinking_tokens": 7}))
    } else if prompt.contains("MODE_ZERO") {
        Some(json!({"thinking_tokens": 0}))
    } else {
        None
    }
}

fn final_usage(details: Option<Value>) -> Value {
    let mut usage = json!({"output_tokens": 10});
    if let Some(details) = details {
        usage["output_tokens_details"] = details;
    }
    usage
}

fn json_message(details: Option<Value>) -> String {
    let mut usage = json!({"input_tokens":5,"cache_creation_input_tokens":2,"cache_read_input_tokens":3});
    usage["output_tokens"] = json!(10);
    if let Some(details) = details {
        usage["output_tokens_details"] = details;
    }
    json!({"id":"msg_u","type":"message","role":"assistant","model":"claude-opus-4-6",
        "content":[{"type":"thinking","thinking":"let me think","signature":"sig"},{"type":"text","text":"answer"}],
        "stop_reason":"end_turn","stop_sequence":null,"usage":usage})
    .to_string()
}

fn sse(details: Option<Value>) -> String {
    [
        json!({"type":"message_start","message":{"id":"msg_u","type":"message","role":"assistant","model":"claude-opus-4-6","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":5,"cache_creation_input_tokens":2,"cache_read_input_tokens":3,"output_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"let me think"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"answer"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":final_usage(details)}),
        json!({"type":"message_stop"}),
    ]
    .iter()
    .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
    .collect()
}

async fn upstream(req: axum::extract::Request) -> Response {
    let body = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap();
    let request: Value = serde_json::from_slice(&body).unwrap();
    let details = details(&String::from_utf8_lossy(&body));
    if request["stream"] != true {
        return ([("content-type", "application/json")], json_message(details)).into_response();
    }
    // 7-byte pieces split `data:` lines, JSON and event boundaries.
    let pieces: Vec<Result<Bytes, std::io::Error>> = sse(details)
        .into_bytes()
        .chunks(7)
        .map(|c| Ok(Bytes::copy_from_slice(c)))
        .collect();
    (
        [("content-type", "text/event-stream")],
        Body::from_stream(futures_util::stream::iter(pieces)),
    )
        .into_response()
}

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

fn auth_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("claude-usage-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn proxy(auth_dir: &Path, upstream_url: &str) -> String {
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
    serve(cpa_server::app(rt, axum::Router::new())).await
}

async fn post(url: &str, path: &str, body: Value) -> Vec<u8> {
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
    let body = response.bytes().await.unwrap().to_vec();
    assert_eq!(status, 200, "{path}: {}", String::from_utf8_lossy(&body));
    body
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

/// The Chat usage the mock's counts produce, with `reasoning` as the reasoning count.
fn chat_usage(reasoning: Option<i64>) -> Value {
    let mut usage = json!({
        "prompt_tokens": 10,
        "completion_tokens": 10,
        "total_tokens": 20,
        "prompt_tokens_details": {"cached_tokens": 3, "cached_creation_tokens": 2, "cache_write_tokens": 2},
    });
    if let Some(tokens) = reasoning {
        usage["completion_tokens_details"] = json!({"reasoning_tokens": tokens});
    }
    usage
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_reports_claude_thinking_tokens_as_reasoning_tokens() {
    let dir = auth_dir("chat");
    let upstream_url = serve(axum::Router::new().fallback(upstream)).await;
    let url = proxy(&dir, &upstream_url).await;

    for (marker, reasoning) in [("MODE_SEVEN", Some(7)), ("MODE_ZERO", Some(0)), ("MODE_ABSENT", None)] {
        let want = chat_usage(reasoning);
        let chat = |stream: bool| {
            let mut body = json!({"model": "claude-opus-4-6", "stream": stream,
                "messages": [{"role": "user", "content": marker}]});
            if stream {
                body["stream_options"] = json!({"include_usage": true});
            }
            body
        };

        let body: Value = serde_json::from_slice(&post(&url, "/v1/chat/completions", chat(false)).await).unwrap();
        assert_eq!(body["usage"], want, "{marker} buffered: {body}");
        assert_eq!(body["choices"][0]["message"]["content"], "answer", "{marker}");
        assert_eq!(body["choices"][0]["message"]["reasoning_content"], "let me think");

        let chunks = data(&post(&url, "/v1/chat/completions", chat(true)).await);
        let usages: Vec<&Value> = chunks
            .iter()
            .filter(|c| c["usage"].is_object())
            .map(|c| &c["usage"])
            .collect();
        assert_eq!(usages.len(), 2, "{marker}: finish and trailing usage: {chunks:?}");
        for usage in usages {
            assert_eq!(*usage, want, "{marker} stream: {chunks:?}");
        }
        assert_eq!(
            chunks.last().unwrap()["choices"],
            json!([]),
            "{marker}: trailing usage chunk"
        );
        let text: String = chunks
            .iter()
            .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
            .collect();
        assert_eq!(text, "answer", "{marker}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Native Messages forwards the upstream's usage bytes, details included, unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_messages_usage_bytes_unchanged() {
    let dir = auth_dir("native");
    let upstream_url = serve(axum::Router::new().fallback(upstream)).await;
    let url = proxy(&dir, &upstream_url).await;

    for marker in ["MODE_SEVEN", "MODE_ZERO", "MODE_ABSENT"] {
        let messages = |stream: bool| {
            json!({"model": "claude-opus-4-6", "max_tokens": 64, "stream": stream,
                "messages": [{"role": "user", "content": marker}]})
        };
        let body = post(&url, "/v1/messages", messages(false)).await;
        assert_eq!(
            String::from_utf8(body).unwrap(),
            json_message(details(marker)),
            "{marker}"
        );
        let body = post(&url, "/v1/messages", messages(true)).await;
        assert_eq!(String::from_utf8(body).unwrap(), sse(details(marker)), "{marker}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
