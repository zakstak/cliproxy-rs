//! #80: a client's output limit and explicit sequential-tool choice reach Grok and Muse.
//! Each request goes through the real router and executor to a loopback upstream that
//! records the final outbound body: every client format (Claude Messages, Chat
//! Completions, Responses), buffered and streaming, with the controls stated and absent.
//! A control the Responses body cannot carry is refused before anything is sent.
//! Go v8.0.35 drops the limit and forces `parallel_tool_calls: true`
//! (docs/DIFFERENCES-FROM-GO.md). Fake API keys, a private `auth-dir` and a loopback
//! upstream only.

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
        .header("x-api-key", "client-key")
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .body(serde_json::to_vec(body).unwrap())
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.text().await.unwrap())
}

#[derive(Clone, Copy, Debug)]
enum Client {
    Messages,
    Chat,
    Responses,
}

impl Client {
    fn path(self) -> &'static str {
        match self {
            Client::Messages => "/v1/messages",
            Client::Chat => "/v1/chat/completions",
            Client::Responses => "/v1/responses",
        }
    }

    /// A request with one function tool, so the parallel-tool control has a meaning, and
    /// with or without the client's output limit (17) and sequential-tool choice.
    fn body(self, model: &str, stream: bool, explicit: bool) -> Value {
        let mut body = match self {
            Client::Messages => json!({"model": model, "messages": [{"role": "user", "content": "Local question"}],
                "tools": [{"name": "lookup", "input_schema": {"type": "object"}}]}),
            Client::Chat => json!({"model": model, "messages": [{"role": "user", "content": "Local question"}],
                "tools": [{"type": "function", "function": {"name": "lookup", "parameters": {"type": "object"}}}]}),
            Client::Responses => json!({"model": model, "input": "Local question",
                "tools": [{"type": "function", "name": "lookup", "parameters": {"type": "object"}}]}),
        };
        let object = body.as_object_mut().unwrap();
        object.insert("stream".into(), json!(stream));
        if explicit {
            match self {
                Client::Messages => {
                    object.insert("max_tokens".into(), json!(17));
                    object.insert(
                        "tool_choice".into(),
                        json!({"type": "auto", "disable_parallel_tool_use": true}),
                    );
                }
                Client::Chat => {
                    object.insert("max_completion_tokens".into(), json!(17));
                    object.insert("parallel_tool_calls".into(), json!(false));
                }
                Client::Responses => {
                    object.insert("max_output_tokens".into(), json!(17));
                    object.insert("parallel_tool_calls".into(), json!(false));
                }
            }
        }
        body
    }
}

const PROVIDERS: [(&str, &str, &str); 2] = [
    ("xai", "grok-4.7", "Bearer sk-xai-FAKE-LOCAL-ONLY"),
    ("meta", "muse-spark-1.3", "Bearer sk-meta-FAKE-LOCAL-ONLY"),
];

#[tokio::test]
async fn grok_and_muse_receive_the_clients_output_limit_and_parallel_choice() {
    let auth_dir = AuthDir::new();
    let (proxy, xai_calls, meta_calls) = proxy(&auth_dir).await;
    for (provider, model, authorization) in PROVIDERS {
        let calls = if provider == "xai" { &xai_calls } else { &meta_calls };
        for client in [Client::Messages, Client::Chat, Client::Responses] {
            for stream in [false, true] {
                for explicit in [true, false] {
                    let case = format!("{provider} {client:?} stream={stream} explicit={explicit}");
                    let before = calls.lock().unwrap().len();
                    let (status, text) = post(
                        format!("{proxy}{}", client.path()),
                        &client.body(model, stream, explicit),
                    )
                    .await;
                    assert_eq!(status, 200, "{case}: {text}");
                    assert!(text.contains("local answer"), "{case}: {text}");
                    let seen = calls.lock().unwrap();
                    assert_eq!(seen.len(), before + 1, "{case}: one upstream request");
                    let (path, auth, body) = &seen[before];
                    assert_eq!(path, "/v1/responses", "{case}");
                    assert_eq!(auth, authorization, "{case}: own credential only");
                    let sent: Value = serde_json::from_str(body).unwrap();
                    let (limit, parallel) = if explicit {
                        (json!(17), json!(false))
                    } else {
                        (Value::Null, json!(true))
                    };
                    assert_eq!(
                        sent.get("max_output_tokens").cloned().unwrap_or(Value::Null),
                        limit,
                        "{case}: {body}"
                    );
                    assert_eq!(sent["parallel_tool_calls"], parallel, "{case}: {body}");
                    for legacy in ["max_tokens", "max_completion_tokens"] {
                        assert!(sent.get(legacy).is_none(), "{case}: {legacy} in {body}");
                    }
                    assert!(!body.contains("client-key"), "{case}: client key forwarded");
                }
            }
        }
    }
}

#[tokio::test]
async fn unreadable_controls_are_refused_before_anything_is_sent() {
    let auth_dir = AuthDir::new();
    let (proxy, xai_calls, meta_calls) = proxy(&auth_dir).await;
    for (_, model, _) in PROVIDERS {
        for (client, field, value, message) in [
            (
                Client::Chat,
                "parallel_tool_calls",
                json!("false"),
                "`parallel_tool_calls` must be a boolean",
            ),
            (Client::Chat, "max_tokens", json!("17"), "`max_tokens` must be a number"),
            (
                Client::Responses,
                "parallel_tool_calls",
                json!(0),
                "`parallel_tool_calls` must be a boolean",
            ),
            (
                Client::Responses,
                "max_output_tokens",
                json!("17"),
                "`max_output_tokens` must be a number",
            ),
            (
                Client::Messages,
                "max_tokens",
                json!("17"),
                "`max_tokens` must be a number",
            ),
        ] {
            for stream in [false, true] {
                let mut body = client.body(model, stream, false);
                body.as_object_mut().unwrap().insert(field.into(), value.clone());
                let (status, text) = post(format!("{proxy}{}", client.path()), &body).await;
                assert_eq!(status, 400, "{model} {client:?} {field}: {text}");
                assert!(text.contains(message), "{model} {client:?} {field}: {text}");
            }
        }
    }
    assert!(xai_calls.lock().unwrap().is_empty(), "nothing reached Grok");
    assert!(meta_calls.lock().unwrap().is_empty(), "nothing reached Muse");
}

/// #80: a Claude Messages `output_config.effort` without a `thinking` type has no
/// Responses form, so it is refused before anything is sent instead of being dropped.
/// Beside adaptive thinking it still becomes `reasoning.effort`.
#[tokio::test]
async fn standalone_claude_effort_is_refused_before_anything_is_sent() {
    let auth_dir = AuthDir::new();
    let (proxy, xai_calls, meta_calls) = proxy(&auth_dir).await;
    for (provider, model, _) in PROVIDERS {
        for stream in [false, true] {
            let case = format!("{provider} stream={stream}");
            let mut body = Client::Messages.body(model, stream, false);
            body["output_config"] = json!({"effort": "low"});
            let (status, text) = post(format!("{proxy}/v1/messages"), &body).await;
            assert_eq!(status, 400, "{case}: {text}");
            assert!(
                text.contains("`output_config.effort` without a `thinking` type"),
                "{case}: {text}"
            );
        }
    }
    assert!(xai_calls.lock().unwrap().is_empty(), "nothing reached Grok");
    assert!(meta_calls.lock().unwrap().is_empty(), "nothing reached Muse");
    for (provider, model, _) in PROVIDERS {
        let calls = if provider == "xai" { &xai_calls } else { &meta_calls };
        let mut body = Client::Messages.body(model, false, false);
        body["thinking"] = json!({"type": "adaptive"});
        body["output_config"] = json!({"effort": "low"});
        let (status, text) = post(format!("{proxy}/v1/messages"), &body).await;
        assert_eq!(status, 200, "{provider}: {text}");
        let seen = calls.lock().unwrap();
        assert_eq!(seen.len(), 1, "{provider}");
        let sent: Value = serde_json::from_str(&seen[0].2).unwrap();
        assert_eq!(sent["reasoning"]["effort"], "low", "{provider}: {sent}");
    }
}
