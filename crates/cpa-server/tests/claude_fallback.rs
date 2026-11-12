//! Claude server-side fallback end to end (#71): client -> cliproxy-rs -> loopback mock
//! Anthropic upstream, one cloaked Claude login with fake tokens in a private
//! `auth-dir`.
//!
//! - `upstream.claude.server-side-fallback` decides whether a request that names no
//!   `fallbacks` gets Claude Code's chain; a caller's own `fallbacks` (empty or not)
//!   reach the upstream unchanged either way.
//! - When the upstream reports a fallback (the SDK's recorded `fallback` block and a
//!   `fallback_message` iteration), native Messages answers stay byte for byte, Chat and
//!   Responses name the serving model and the hop, and every usage record names the
//!   serving model, streamed or buffered.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use serde_json::{Value, json};

const FILE: &str = "claude-fallback.json";
const REQUESTED: &str = "claude-opus-5-5";
const SERVED: &str = "claude-opus-4-8";

fn iterations() -> Value {
    json!([
        {"type":"message","model":REQUESTED,"input_tokens":25,"output_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0},
        {"type":"fallback_message","model":SERVED,"input_tokens":25,"output_tokens":7,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}
    ])
}

fn fallback_block() -> Value {
    json!({"type":"fallback","from":{"model":REQUESTED},"to":{"model":SERVED},"trigger":{"type":"refusal","category":null}})
}

fn native_json() -> String {
    json!({"id":"msg_fb","type":"message","role":"assistant","model":SERVED,
        "content":[fallback_block(),{"type":"text","text":"Hello there!"}],
        "stop_reason":"end_turn","stop_sequence":null,
        "usage":{"input_tokens":25,"output_tokens":8,"iterations":iterations()}})
    .to_string()
}

fn native_sse() -> String {
    [
        json!({"type":"message_start","message":{"id":"msg_fb","type":"message","role":"assistant","content":[],"model":REQUESTED,"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":25,"output_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":fallback_block()}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hello there!"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":8,"iterations":iterations()}}),
        json!({"type":"message_stop"}),
    ]
    .iter()
    .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
    .collect()
}

type Seen = Arc<Mutex<Vec<String>>>;

async fn upstream(axum::extract::State(seen): axum::extract::State<Seen>, body: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    let stream = serde_json::from_str::<Value>(&body).unwrap()["stream"] == true;
    seen.lock().unwrap().push(body);
    if stream {
        ([("content-type", "text/event-stream")], native_sse()).into_response()
    } else {
        ([("content-type", "application/json")], native_json()).into_response()
    }
}

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

fn auth_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("claude-fallback-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn proxy(auth_dir: &Path, upstream_url: &str, setting: &str) -> (String, Arc<cpa_server::Runtime>) {
    let yaml = format!(
        "api-keys: [client-key]\nauth-dir: {}\nobservability: {{usage: {{usage-statistics-enabled: true}}}}\n{setting}",
        auth_dir.display()
    );
    // The executor's origin is the mock; `local` marks the login as pointed there, so
    // nothing can reach Anthropic.
    // A complete synthetic identity and no refresh token: nothing here may start a
    // sign-in refresh or an identity lookup.
    let meta = json!({"type": "claude", "access_token": "sk-ant-oat01-FAKE-LOCAL-ONLY",
        "email": "fixture@example.invalid", "account_uuid": "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
        "organization_uuid": "12345678-1234-4234-8234-123456789abc",
        "claude_device_ids": ["0123456789abcdef".repeat(4)]});
    std::fs::write(auth_dir.join(FILE), meta.to_string()).unwrap();
    let credential = cpa_server::testing::local(
        Credential::from_file(auth_dir, &auth_dir.join(FILE), meta.as_object().unwrap().clone()).unwrap(),
    );
    let config = Config::parse(&yaml).unwrap();
    let rt = Arc::new(cpa_server::testing::runtime(
        config.clone(),
        vec![credential],
        Executors {
            claude: ClaudeExecutor::new(upstream_url).unwrap(),
            codex: Default::default(),
            meta: Default::default(),
            xai: Default::default(),
        },
    ));
    rt.usage_queue().configure(true, &config);
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

fn data(body: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(body)
        .lines()
        .filter_map(|l| l.strip_prefix("data: ").or_else(|| l.strip_prefix("data:")))
        .filter(|d| d.trim() != "[DONE]")
        .map(|d| serde_json::from_str(d.trim()).unwrap())
        .collect()
}

async fn usage_models(rt: &cpa_server::Runtime, want: usize) -> Vec<String> {
    let mut got = Vec::new();
    for _ in 0..300 {
        got.extend(rt.usage_queue().pop_oldest(50));
        if got.len() >= want {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    got.iter()
        .map(|r| {
            let v: Value = serde_json::from_slice(r).unwrap();
            v["response_model"].as_str().unwrap_or_default().to_owned()
        })
        .collect()
}

/// What each route reports as the serving model, and the hop it names when it has a
/// field for one.
async fn routes(url: &str) -> Vec<(String, String, Option<Value>)> {
    let mut out = vec![];
    for stream in [false, true] {
        let label = |route: &str| format!("{route} stream={stream}");
        // Native Messages: the upstream's bytes.
        let (status, body) = post(
            url,
            "/v1/messages",
            json!({"model": REQUESTED, "max_tokens": 64, "stream": stream, "messages": [{"role":"user","content":"hi"}]}),
        )
        .await;
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
        let want = if stream { native_sse() } else { native_json() };
        assert_eq!(String::from_utf8(body).unwrap(), want, "native bytes unchanged");
        out.push((label("messages"), SERVED.to_owned(), None));

        let (status, body) = post(
            url,
            "/v1/chat/completions",
            json!({"model": REQUESTED, "stream": stream, "messages": [{"role":"user","content":"hi"}]}),
        )
        .await;
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
        let (model, hop) = if stream {
            let chunks = data(&body);
            let hop = chunks
                .iter()
                .find_map(|c| c["choices"][0]["delta"].get("fallback").cloned());
            (chunks.last().unwrap()["model"].as_str().unwrap().to_owned(), hop)
        } else {
            let v: Value = serde_json::from_slice(&body).unwrap();
            (
                v["model"].as_str().unwrap().to_owned(),
                v["choices"][0]["message"].get("fallback").cloned(),
            )
        };
        out.push((label("chat"), model, hop));

        let (status, body) = post(
            url,
            "/v1/responses",
            json!({"model": REQUESTED, "stream": stream, "input": "hi"}),
        )
        .await;
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
        let response = if stream {
            data(&body).last().unwrap()["response"].clone()
        } else {
            serde_json::from_slice(&body).unwrap()
        };
        out.push((
            label("responses"),
            response["model"].as_str().unwrap().to_owned(),
            response.get("fallback").cloned(),
        ));
    }
    out
}

fn hop_models(hop: &Value) -> (String, String) {
    (
        hop["from"]["model"].as_str().unwrap().to_owned(),
        hop["to"]["model"].as_str().unwrap().to_owned(),
    )
}

async fn run(setting: &str, name: &str, synthesized: bool) {
    let dir = auth_dir(name);
    let seen: Seen = Arc::default();
    let upstream_url = serve(axum::Router::new().fallback(upstream).with_state(seen.clone())).await;
    let (url, rt) = proxy(&dir, &upstream_url, setting).await;

    for (label, model, hop) in routes(&url).await {
        assert_eq!(model, SERVED, "{label}: serving model");
        if !label.starts_with("messages") {
            let hop = hop.unwrap_or_else(|| panic!("{label}: hop reported"));
            assert_eq!(hop_models(&hop), (REQUESTED.to_owned(), SERVED.to_owned()), "{label}");
            assert_eq!(hop["trigger"], json!({"type":"refusal","category":null}), "{label}");
        }
    }
    let models = usage_models(&rt, 6).await;
    assert_eq!(models.len(), 6, "{models:?}");
    assert!(
        models.iter().all(|m| m == SERVED),
        "usage names the serving model: {models:?}"
    );

    // The six route requests named no fallbacks.
    let sent: Vec<String> = std::mem::take(&mut *seen.lock().unwrap());
    assert_eq!(sent.len(), 6);
    for body in &sent {
        let fallbacks = serde_json::from_str::<Value>(body).unwrap().get("fallbacks").cloned();
        if synthesized {
            assert_eq!(fallbacks, Some(json!([{"model": SERVED}])), "{body}");
        } else {
            assert_eq!(fallbacks, None, "{body}");
        }
    }
    // A caller's own fallbacks reach the upstream unchanged.
    for caller in [r#"[]"#, r#"[{"model":"claude-sonnet-4-6"}]"#, r#""default""#] {
        let body = format!(
            r#"{{"model":"{REQUESTED}","max_tokens":64,"fallbacks":{caller},"messages":[{{"role":"user","content":"hi"}}]}}"#
        );
        let (status, _) = post(&url, "/v1/messages", serde_json::from_str(&body).unwrap()).await;
        assert_eq!(status, 200);
        let sent = seen.lock().unwrap().pop().unwrap();
        let got = serde_json::from_str::<Value>(&sent).unwrap()["fallbacks"].clone();
        assert_eq!(got, serde_json::from_str::<Value>(caller).unwrap(), "{caller}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fallback_absent_setting_synthesizes_none() {
    run("", "absent", false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fallback_disabled_synthesizes_none() {
    run(
        "upstream:\n  claude:\n    server-side-fallback: false\n",
        "disabled",
        false,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fallback_enabled_synthesizes_claude_codes_chain() {
    run(
        "upstream:\n  claude:\n    server-side-fallback: true\n",
        "enabled",
        true,
    )
    .await;
}

/// Synthetic shrinking snapshots exercise consistency, not a claim that Anthropic
/// emits them: the final serving-model signal governs hops and usage together.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shrinking_snapshot_keeps_model_hops_and_usage_together() {
    const LATEST: &str = "claude-opus-5";
    let iteration = |kind: &str, model: &str| json!({"type":kind,"model":model});
    let events = [
        json!({"type":"message_start","message":{"id":"msg_snapshots","type":"message","role":"assistant","content":[],"model":REQUESTED,"usage":{"input_tokens":1,"output_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":null},"usage":{"output_tokens":2,"iterations":[iteration("message",REQUESTED),iteration("message",LATEST),iteration("fallback_message","claude-sonnet-4-6")]}}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2,"iterations":[iteration("message",REQUESTED),iteration("fallback_message",LATEST)]}}),
        json!({"type":"message_stop"}),
    ];
    let wire: String = events
        .iter()
        .map(|event| format!("event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap()))
        .collect();
    let mock = serve(axum::Router::new().fallback(move || {
        let wire = wire.clone();
        async move { ([("content-type", "text/event-stream")], wire) }
    }))
    .await;
    let dir = auth_dir("shrinking-snapshot");
    let (url, rt) = proxy(&dir, &mock, "").await;
    for streamed in [false, true] {
        for (path, request, chat) in [
            (
                "/v1/chat/completions",
                json!({"model":REQUESTED,"stream":streamed,"messages":[{"role":"user","content":"hi"}]}),
                true,
            ),
            (
                "/v1/responses",
                json!({"model":REQUESTED,"stream":streamed,"input":"hi"}),
                false,
            ),
        ] {
            let (status, body) = post(&url, path, request).await;
            assert_eq!(status, 200, "{path}: {}", String::from_utf8_lossy(&body));
            let (model, hop) = if streamed {
                let chunks = data(&body);
                if chat {
                    let hop = chunks
                        .iter()
                        .rev()
                        .find_map(|chunk| chunk["choices"][0]["delta"].get("fallback"))
                        .unwrap()
                        .clone();
                    (chunks.last().unwrap()["model"].clone(), hop)
                } else {
                    let response = &chunks.last().unwrap()["response"];
                    (response["model"].clone(), response["fallback"].clone())
                }
            } else {
                let response: Value = serde_json::from_slice(&body).unwrap();
                let hop = if chat {
                    response["choices"][0]["message"]["fallback"].clone()
                } else {
                    response["fallback"].clone()
                };
                (response["model"].clone(), hop)
            };
            assert_eq!(model, LATEST, "{path} stream={streamed}");
            assert_eq!(
                hop_models(&hop),
                (REQUESTED.to_owned(), LATEST.to_owned()),
                "{path} stream={streamed}"
            );
        }
    }
    assert_eq!(usage_models(&rt, 4).await, vec![LATEST; 4]);
    std::fs::remove_dir_all(dir).unwrap();
}
