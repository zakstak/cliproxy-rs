//! Tool property keys Claude rejects, end to end (#77): client -> cliproxy-rs ->
//! loopback mock Anthropic upstream, one API-key credential with a fake key in a
//! private `auth-dir`.
//!
//! The mock refuses a tool schema whose property keys fall outside
//! `^[a-zA-Z0-9_.-]{1,64}$` with Anthropic's 400, as router-for-me/CLIProxyAPI#7389
//! reports for Codex's GitLab MCP tools. Otherwise it calls the tool with the key the
//! schema declares for "Exclude labels", as a model following the schema would.
//! Responses and Chat Completions clients get their own key back; a native Messages
//! request reaches the upstream unchanged and gets Anthropic's refusal.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use serde_json::{Value, json};

const FILE: &str = "claude-keys.json";
const REJECTION: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"tools.0.custom.input_schema.properties: Property keys should match pattern '^[a-zA-Z0-9_.-]{1,64}$'"}}"#;

fn accepted(key: &str) -> bool {
    (1..=64).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

fn keys_accepted(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().all(|(k, v)| {
            let props_ok = k != "properties" || v.as_object().is_none_or(|p| p.keys().all(|k| accepted(k)));
            props_ok && keys_accepted(v)
        }),
        Value::Array(items) => items.iter().all(keys_accepted),
        _ => true,
    }
}

#[derive(Default)]
struct Up {
    bodies: Mutex<Vec<Value>>,
}

async fn upstream(State(up): State<Arc<Up>>, req: axum::extract::Request) -> Response {
    let body = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    up.bodies.lock().unwrap().push(body.clone());
    if !keys_accepted(&body["tools"]) {
        return (
            StatusCode::BAD_REQUEST,
            [("content-type", "application/json")],
            REJECTION,
        )
            .into_response();
    }
    let key = body["tools"][0]["input_schema"]["properties"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, p)| p["description"] == "Exclude labels")
        .map(|(k, _)| k.clone())
        .unwrap();
    let input = json!({key: "bug", "state": "opened"}).to_string();
    if body["stream"] == true {
        let (a, b) = input.split_at(input.len() / 2);
        let events = [
            json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-4-6","content":[],"stop_reason":null,"usage":{"input_tokens":5,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"list_merge_requests","input":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":a}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":b}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":3}}),
            json!({"type":"message_stop"}),
        ];
        let sse: String = events
            .iter()
            .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
            .collect();
        return ([("content-type", "text/event-stream")], sse).into_response();
    }
    let message = json!({"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-4-6",
        "content":[{"type":"tool_use","id":"toolu_1","name":"list_merge_requests","input":serde_json::from_str::<Value>(&input).unwrap()}],
        "stop_reason":"tool_use","usage":{"input_tokens":5,"output_tokens":3}});
    ([("content-type", "application/json")], message.to_string()).into_response()
}

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

fn auth_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("claude-keys-{name}-{}", std::process::id()));
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

async fn post(url: &str, path: &str, body: &Value) -> (u16, Vec<u8>) {
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

/// The `data:` payloads of an SSE body, as JSON.
fn data(body: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(body)
        .lines()
        .filter_map(|l| l.strip_prefix("data: ").or_else(|| l.strip_prefix("data:")))
        .filter(|d| d.trim() != "[DONE]")
        .map(|d| serde_json::from_str(d.trim()).unwrap())
        .collect()
}

fn schema() -> Value {
    json!({"type": "object", "properties": {
        "not[labels]": {"type": "string", "description": "Exclude labels"},
        "state": {"type": "string"}
    }, "required": ["not[labels]"]})
}

fn args(raw: &Value) -> Value {
    serde_json::from_str(raw.as_str().unwrap()).unwrap()
}

fn want() -> Value {
    json!({"not[labels]": "bug", "state": "opened"})
}

/// The last request the upstream saw: its first tool's schema and its first tool_use input.
fn last_sent(up: &Up) -> (Value, Value) {
    let body = up.bodies.lock().unwrap().last().cloned().unwrap();
    let tool_use = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["content"].as_array().into_iter().flatten())
        .find(|b| b["type"] == "tool_use")
        .map(|b| b["input"].clone())
        .unwrap_or(Value::Null);
    (body["tools"][0]["input_schema"].clone(), tool_use)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn translated_routes_rename_and_restore_rejected_property_keys() {
    let dir = auth_dir("translated");
    let up = Arc::new(Up::default());
    let upstream_url = serve(axum::Router::new().fallback(upstream).with_state(up.clone())).await;
    let url = proxy(&dir, &upstream_url).await;

    for stream in [false, true] {
        let responses = json!({"model": "claude-opus-4-6", "stream": stream,
        "tools": [{"type": "function", "name": "list_merge_requests", "parameters": schema()}],
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
            {"type": "function_call", "call_id": "call_0", "name": "list_merge_requests", "arguments": "{\"not[labels]\":\"wip\"}"},
            {"type": "function_call_output", "call_id": "call_0", "output": "[]"}
        ]});
        let (status, body) = post(&url, "/v1/responses", &responses).await;
        assert_eq!(status, 200, "stream={stream}: {}", String::from_utf8_lossy(&body));
        let (sent, history) = last_sent(&up);
        assert_eq!(
            sent["properties"]["not_labels_"]["description"], "Exclude labels",
            "{sent}"
        );
        assert_eq!(sent["required"], json!(["not_labels_"]));
        assert_eq!(history, json!({"not_labels_": "wip"}));
        let output = if stream {
            let events = data(&body);
            events.last().unwrap()["response"]["output"].clone()
        } else {
            serde_json::from_slice::<Value>(&body).unwrap()["output"].clone()
        };
        let call = output
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["type"] == "function_call")
            .unwrap();
        assert_eq!(call["name"], "list_merge_requests");
        assert_eq!(args(&call["arguments"]), want(), "stream={stream}");

        let chat = json!({"model": "claude-opus-4-6", "stream": stream,
            "tools": [{"type": "function", "function": {"name": "list_merge_requests", "parameters": schema()}}],
            "messages": [{"role": "user", "content": "hi"}]});
        let (status, body) = post(&url, "/v1/chat/completions", &chat).await;
        assert_eq!(status, 200, "stream={stream}: {}", String::from_utf8_lossy(&body));
        let arguments = if stream {
            data(&body)
                .iter()
                .flat_map(|c| {
                    c["choices"][0]["delta"]["tool_calls"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                })
                .filter_map(|c| c["function"]["arguments"].as_str().map(str::to_owned))
                .collect::<String>()
        } else {
            let body: Value = serde_json::from_slice(&body).unwrap();
            body["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        assert_eq!(
            serde_json::from_str::<Value>(&arguments).unwrap(),
            want(),
            "stream={stream}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_messages_keep_the_clients_schema() {
    let dir = auth_dir("native");
    let up = Arc::new(Up::default());
    let upstream_url = serve(axum::Router::new().fallback(upstream).with_state(up.clone())).await;
    let url = proxy(&dir, &upstream_url).await;

    let messages = json!({"model": "claude-opus-4-6", "max_tokens": 64,
        "tools": [{"name": "list_merge_requests", "input_schema": schema()}],
        "messages": [{"role": "user", "content": "hi"}]});
    let (status, body) = post(&url, "/v1/messages", &messages).await;
    assert_eq!(status, 400, "{}", String::from_utf8_lossy(&body));
    assert!(String::from_utf8_lossy(&body).contains("Property keys should match pattern"));
    let (sent, _) = last_sent(&up);
    assert_eq!(sent, schema(), "native schema reaches Claude unchanged");
}
