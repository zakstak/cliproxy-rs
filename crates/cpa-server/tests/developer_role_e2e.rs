//! #80: a Responses-shaped body sent to `/v1/chat/completions` keeps its developer
//! messages as developer messages, in place, on Grok and Muse. Each request goes through
//! the real router and executor to a loopback upstream that records the final outbound
//! body, buffered and streaming. Go v8.0.35 sends developer text as user text
//! (docs/DIFFERENCES-FROM-GO.md). The setup follows `responses_controls_e2e`: fake API
//! keys, a private `auth-dir` and loopback upstreams only.

use std::sync::{Arc, Mutex};

use axum::http::{HeaderMap, Uri};
use cpa_core::config::Config;
use cpa_exec::Executors;
use serde_json::{Value, json};

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
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

    fn config(&self, extra: &str) -> Config {
        Config::parse(&format!(
            "oauth:\n  auth-dir: {}\naccess:\n  api-keys: [client-key]\n{extra}",
            self.0.display()
        ))
        .unwrap()
    }
}

impl Drop for AuthDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn executors() -> Executors {
    Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        meta: Default::default(),
        xai: Default::default(),
    }
}

/// A Responses event stream with one text output and a completed terminal event.
fn responses_stream(model: &str) -> String {
    let message = json!({"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
        "content": [{"type": "output_text", "text": "local answer", "annotations": []}]});
    let response = |status: &str, output: Value| {
        json!({"id": "resp_1", "object": "response", "created_at": 1790000000, "status": status,
            "model": model, "output": output,
            "usage": {"input_tokens": 5, "output_tokens": 2, "total_tokens": 7}})
    };
    let part = json!({"item_id": "msg_1", "output_index": 0, "content_index": 0});
    let with_part = |extra: Value| {
        let mut v = part.clone();
        v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        v
    };
    let events = [
        (
            "response.created",
            json!({"response": response("in_progress", json!([]))}),
        ),
        (
            "response.output_item.added",
            json!({"output_index": 0, "item": {"type": "message", "id": "msg_1", "role": "assistant",
                "status": "in_progress", "content": []}}),
        ),
        (
            "response.content_part.added",
            with_part(json!({"part": {"type": "output_text", "text": "", "annotations": []}})),
        ),
        (
            "response.output_text.delta",
            with_part(json!({"delta": "local answer"})),
        ),
        ("response.output_text.done", with_part(json!({"text": "local answer"}))),
        (
            "response.content_part.done",
            with_part(json!({"part": {"type": "output_text", "text": "local answer", "annotations": []}})),
        ),
        ("response.output_item.done", json!({"output_index": 0, "item": message})),
        (
            "response.completed",
            json!({"response": response("completed", json!([message]))}),
        ),
    ];
    events
        .iter()
        .enumerate()
        .map(|(n, (kind, data))| {
            let mut data = data.clone();
            let object = data.as_object_mut().unwrap();
            object.insert("type".into(), json!(kind));
            object.insert("sequence_number".into(), json!(n));
            format!("event: {kind}\ndata: {data}\n\n")
        })
        .collect()
}

/// One request the upstream saw: path, `Authorization` and body.
type Calls = Arc<Mutex<Vec<(String, String, String)>>>;

async fn recording_upstream(model: &'static str) -> (String, Calls) {
    let calls: Calls = Arc::default();
    let record = calls.clone();
    let url = serve(
        axum::Router::new().fallback(move |uri: Uri, headers: HeaderMap, body: axum::body::Bytes| {
            let record = record.clone();
            async move {
                let authorization = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                record.lock().unwrap().push((
                    uri.path().to_owned(),
                    authorization,
                    String::from_utf8_lossy(&body).into_owned(),
                ));
                ([("content-type", "text/event-stream")], responses_stream(model))
            }
        }),
    )
    .await;
    (url, calls)
}

/// A proxy with one fake Grok key and one fake Muse key, each at its own recording
/// upstream.
async fn proxy(auth_dir: &AuthDir) -> (String, Calls, Calls) {
    let (xai_url, xai_calls) = recording_upstream("grok-4.7").await;
    let (meta_url, meta_calls) = recording_upstream("muse-spark-1.3").await;
    let cfg = auth_dir.config(&format!(
        "xai-api-key:\n  - api-key: sk-xai-FAKE-LOCAL-ONLY\n    base-url: {xai_url}/v1\n\
         meta-api-key:\n  - api-key: sk-meta-FAKE-LOCAL-ONLY\n    base-url: {meta_url}/v1\n"
    ));
    let credentials = cpa_core::config::credentials::from_config(&cfg);
    assert_eq!(credentials.len(), 2, "the Grok and Muse keys");
    let rt = Arc::new(cpa_server::testing::runtime(cfg, credentials, executors()));
    (serve(cpa_server::router(rt)).await, xai_calls, meta_calls)
}

async fn post(url: String, body: &Value) -> (u16, String) {
    let response = wreq::Client::new()
        .post(url)
        .header("authorization", "Bearer client-key")
        .header("content-type", "application/json")
        .body(serde_json::to_vec(body).unwrap())
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.text().await.unwrap())
}

const PROVIDERS: [(&str, &str, &str); 2] = [
    ("xai", "grok-4.7", "Bearer sk-xai-FAKE-LOCAL-ONLY"),
    ("meta", "muse-spark-1.3", "Bearer sk-meta-FAKE-LOCAL-ONLY"),
];

/// A Responses-shaped Chat Completions body: a leading developer message with string
/// content and a later one with a text-part array, around user and assistant turns.
fn body(model: &str, stream: bool) -> Value {
    json!({"model": model, "stream": stream, "input": [
        {"type": "message", "role": "developer", "content": "leading instruction"},
        {"type": "message", "role": "user", "content": "question"},
        {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "prior answer"}]},
        {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "later instruction"}]},
        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "continue"}]},
    ]})
}

/// The outbound Responses `input` messages, each as its role, its content part types and
/// its text.
fn sent_messages(sent: &Value) -> Vec<(String, Vec<String>, String)> {
    sent["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            assert_eq!(item["type"], "message", "only messages: {sent}");
            let parts = item["content"].as_array().unwrap();
            (
                item["role"].as_str().unwrap().to_owned(),
                parts.iter().map(|p| p["type"].as_str().unwrap().to_owned()).collect(),
                parts.iter().map(|p| p["text"].as_str().unwrap()).collect(),
            )
        })
        .collect()
}

#[tokio::test]
async fn grok_and_muse_receive_developer_messages_in_place() {
    let auth_dir = AuthDir::new();
    let (proxy, xai_calls, meta_calls) = proxy(&auth_dir).await;
    let want: Vec<(String, Vec<String>, String)> = [
        ("developer", "input_text", "leading instruction"),
        ("user", "input_text", "question"),
        ("assistant", "output_text", "prior answer"),
        ("developer", "input_text", "later instruction"),
        ("user", "input_text", "continue"),
    ]
    .into_iter()
    .map(|(role, kind, text)| (role.to_owned(), vec![kind.to_owned()], text.to_owned()))
    .collect();
    for (provider, model, authorization) in PROVIDERS {
        let calls = if provider == "xai" { &xai_calls } else { &meta_calls };
        for stream in [false, true] {
            let case = format!("{provider} stream={stream}");
            let before = calls.lock().unwrap().len();
            let (status, text) = post(format!("{proxy}/v1/chat/completions"), &body(model, stream)).await;
            assert_eq!(status, 200, "{case}: {text}");
            assert!(text.contains("local answer"), "{case}: {text}");
            if stream {
                assert!(text.contains("chat.completion.chunk"), "{case}: {text}");
            } else {
                let completion: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(completion["object"], "chat.completion", "{case}: {text}");
                assert_eq!(
                    completion["choices"][0]["message"]["content"], "local answer",
                    "{case}: {text}"
                );
            }
            let seen = calls.lock().unwrap();
            assert_eq!(seen.len(), before + 1, "{case}: one upstream request");
            let (path, auth, sent) = &seen[before];
            assert_eq!(path, "/v1/responses", "{case}");
            assert_eq!(auth, authorization, "{case}: own credential only");
            assert!(!sent.contains("client-key"), "{case}: client key forwarded");
            let sent: Value = serde_json::from_str(sent).unwrap();
            assert_eq!(sent_messages(&sent), want, "{case}: {sent}");
        }
    }
}
