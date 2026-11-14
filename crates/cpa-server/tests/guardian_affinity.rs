//! Codex Guardian reviewers and session affinity (#113): client -> cliproxy-rs ->
//! loopback mock Anthropic upstream, two fake Claude API keys serving two models,
//! private `auth-dir`s, under a network-denied runtime.
//!
//! First-party Codex (openai/codex@44ccf394573c, `core/src/client.rs` and
//! `core/src/responses_metadata.rs`) sends a Guardian reviewer's request with the root's
//! `session-id`, its own `thread-id`, `x-openai-subagent: guardian` and
//! `x-codex-parent-thread-id`. The reviewer usually runs on another model than its
//! parent. With session affinity it inherits the account its parent's conversation is
//! bound to, but only for the same client key.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use serde_json::json;

const OK: &str = r#"{"id":"msg_1","type":"message","role":"assistant","model":"m","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;
const LIMIT: &str = r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#;
const ROOT: &str = "019a0000-0000-7000-8000-000000000001";
const REVIEWER: &str = "019a0000-0000-7000-8000-000000000002";

/// `(upstream key, model)` of every call, in order.
type Calls = Arc<Mutex<Vec<(String, String)>>>;

/// `sk-fake-a` answers 429 to its first call and 200 after; `sk-fake-b` always 200.
async fn mock() -> (String, Calls) {
    let calls: Calls = Arc::default();
    let seen = calls.clone();
    let app = axum::Router::new().fallback(move |req: Request| {
        let seen = seen.clone();
        async move {
            let key = req
                .headers()
                .get("x-api-key")
                .or_else(|| req.headers().get("authorization"))
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .trim_start_matches("Bearer ")
                .to_owned();
            let body = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap();
            let model = serde_json::from_slice::<serde_json::Value>(&body).unwrap()["model"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let mut seen = seen.lock().unwrap();
            let first_a = key == "sk-fake-a" && !seen.iter().any(|(k, _)| k == "sk-fake-a");
            seen.push((key, model));
            if first_a {
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    [("content-type", "application/json")],
                    LIMIT,
                )
                    .into_response()
            } else {
                (StatusCode::OK, [("content-type", "application/json")], OK).into_response()
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, calls)
}

fn auth_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("guardian-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn start(dir: &Path, upstream: &str) -> String {
    let config = Config::parse(&format!(
        "api-keys: [client-key, other-key]\nauth-dir: {}\nrouting:\n  strategy: fill-first\n  session-affinity: true\n\
         claude-api-key:\n  - api-key: sk-fake-a\n    base-url: {upstream}\n    priority: 1\n    \
         models: [{{name: parent-model}}, {{name: review-model}}]\n  - api-key: sk-fake-b\n    \
         base-url: {upstream}\n    models: [{{name: parent-model}}, {{name: review-model}}]\n",
        dir.display()
    ))
    .unwrap();
    let credentials = cpa_core::config::credentials::load(&config);
    let executors = Executors {
        claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: Default::default(),
        meta: Default::default(),
        xai: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(config, credentials, executors));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = cpa_server::app(rt, axum::Router::new());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

async fn post(url: &str, client_key: &str, model: &str, headers: &[(&str, &str)]) -> u16 {
    let body = json!({"model": model, "max_tokens": 8, "messages": [{"role": "user", "content": "hi"}]});
    let mut request = wreq::Client::new()
        .post(format!("{url}/v1/messages"))
        .header("x-api-key", client_key)
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request.body(body.to_string()).send().await.unwrap().status().as_u16()
}

/// The parent's conversation moves to `sk-fake-b` after `sk-fake-a`'s 429. Its Guardian
/// reviewer, on another model and with `sk-fake-a` ready again, follows the parent to
/// `sk-fake-b`. The same reviewer headers from another client key, and an ordinary
/// subagent, get the strategy's pick (`sk-fake-a`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guardian_reviewers_follow_their_parent_for_the_same_client_key() {
    let dir = auth_dir("follow");
    let (upstream, calls) = mock().await;
    let url = start(&dir, &upstream).await;
    let parent = [("session-id", ROOT), ("thread-id", ROOT)];
    assert_eq!(post(&url, "client-key", "parent-model", &parent).await, 200);
    let review = |kind: &'static str, thread: &'static str| {
        [
            ("session-id", ROOT),
            ("thread-id", thread),
            ("x-openai-subagent", kind),
            ("x-codex-parent-thread-id", ROOT),
        ]
    };
    let intruder = "019a0000-0000-7000-8000-000000000003";
    let worker = "019a0000-0000-7000-8000-000000000004";
    assert_eq!(
        post(&url, "other-key", "review-model", &review("guardian", intruder)).await,
        200
    );
    assert_eq!(
        post(&url, "client-key", "review-model", &review("collab_spawn", worker)).await,
        200
    );
    assert_eq!(
        post(&url, "client-key", "review-model", &review("guardian", REVIEWER)).await,
        200
    );
    let keys: Vec<String> = calls.lock().unwrap().iter().map(|(k, m)| format!("{k} {m}")).collect();
    assert_eq!(
        keys,
        [
            "sk-fake-a parent-model",
            "sk-fake-b parent-model",
            "sk-fake-a review-model",
            "sk-fake-a review-model",
            "sk-fake-b review-model",
        ],
        "another key and an ordinary subagent take the strategy's pick; the reviewer follows its parent"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
