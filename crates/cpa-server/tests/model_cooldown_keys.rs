//! Canonical model cooldown keys and forced-cooldown persistence (#95): client ->
//! cliproxy-rs -> loopback mock Anthropic upstream, fake API keys, private `auth-dir`s,
//! under a network-denied runtime.
//!
//! - A configured model whose name differs from its alias only in case is sent upstream
//!   under the configured name, so every client spelling of it shares one cooldown,
//!   live and after a restart that migrates records saved under an older spelling (Go
//!   v8.0.67 098af7d5, issue 7360 shape 1). Spellings sent upstream differently stay
//!   apart.
//! - A cooldown forced by a request-scoped `*-and-cooldown` rule survives
//!   `disable-cooling` across a config reload and a restart with
//!   `save-cooldown-status`; an ordinary failure under `disable-cooling` cools nothing
//!   (Go v8.0.67 9d7d012b).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use serde_json::{Value, json};

/// The model each upstream call carried.
type Calls = Arc<Mutex<Vec<String>>>;

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

/// A mock answering every call with `status` and `body`, with a one-hour `Retry-After`.
async fn mock(status: u16, body: &'static str) -> (String, Calls) {
    let calls: Calls = Arc::default();
    let seen = calls.clone();
    let app = axum::Router::new().fallback(move |req: Request| {
        let seen = seen.clone();
        async move {
            let body_in = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap();
            let model = serde_json::from_slice::<Value>(&body_in).unwrap()["model"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            seen.lock().unwrap().push(model);
            (
                StatusCode::from_u16(status).unwrap(),
                [("content-type", "application/json"), ("retry-after", "3600")],
                body,
            )
                .into_response()
        }
    });
    (serve(app).await, calls)
}

fn auth_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("model-keys-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn executors() -> Executors {
    Executors {
        claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: Default::default(),
        meta: Default::default(),
        xai: Default::default(),
    }
}

async fn app(rt: &Arc<cpa_server::Runtime>) -> String {
    serve(cpa_server::app(rt.clone(), axum::Router::new())).await
}

async fn post(url: &str, model: &str) -> (u16, String) {
    let body = json!({"model": model, "max_tokens": 8, "messages": [{"role": "user", "content": "hi"}]});
    let response = wreq::Client::new()
        .post(format!("{url}/v1/messages"))
        .header("x-api-key", "client-key")
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.text().await.unwrap())
}

const LIMIT: &str = r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#;

/// The config of one Claude API key whose configured name differs from its alias only
/// in case, with `save-cooldown-status` on.
fn configured(dir: &Path, upstream: &str) -> Config {
    Config::parse(&format!(
        "api-keys: [client-key]\nauth-dir: {}\nrouting:\n  cooldown:\n    save-cooldown-status: true\n\
         claude-api-key:\n  - api-key: sk-fake-claude\n    base-url: {upstream}\n    \
         models: [{{name: Claude-Opus-4-6, alias: claude-opus-4-6}}]\n",
        dir.display()
    ))
    .unwrap()
}

fn configured_runtime(config: Config) -> (Arc<cpa_server::Runtime>, String) {
    let credentials = cpa_core::config::credentials::load(&config);
    let id = credentials
        .iter()
        .find(|c| c.provider == "claude")
        .expect("the configured key")
        .id
        .clone();
    (
        Arc::new(cpa_server::testing::runtime(config, credentials, executors())),
        id,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spellings_sent_as_one_configured_name_share_one_cooldown() {
    let dir = auth_dir("shared");
    let (upstream, calls) = mock(429, LIMIT).await;
    let (rt, id) = configured_runtime(configured(&dir, &upstream));
    let url = app(&rt).await;
    let (status, _) = post(&url, "claude-opus-4-6").await;
    assert_eq!(status, 429);
    assert_eq!(
        *calls.lock().unwrap(),
        ["Claude-Opus-4-6"],
        "sent under the configured name"
    );
    for spelling in ["CLAUDE-OPUS-4-6", "Claude-Opus-4-6", "claude-opus-4-6(high)"] {
        let (status, body) = post(&url, spelling).await;
        assert_eq!(status, 429, "{spelling}: {body}");
        assert!(body.contains("cooling down"), "{spelling}: a local cooldown: {body}");
    }
    assert_eq!(calls.lock().unwrap().len(), 1, "no spelling reached the upstream again");
    let cooling = rt.store().cooldowns(&id);
    assert_eq!(
        cooling.iter().map(|c| c.model.as_str()).collect::<Vec<_>>(),
        ["Claude-Opus-4-6"],
        "one key"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Records an older version saved under client spellings come back under the key the
/// spelling resolves to, merged, with the later deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_migrates_and_merges_records_saved_under_older_spellings() {
    use cpa_server::cooldown_store::{Quota, Record};
    let dir = auth_dir("migrate");
    let (upstream, calls) = mock(429, LIMIT).await;
    let config = configured(&dir, &upstream);
    let id = cpa_core::config::credentials::load(&config)
        .into_iter()
        .find(|c| c.provider == "claude")
        .unwrap()
        .id;
    let now = SystemTime::now();
    let record = |model: &str, secs: u64| Record {
        provider: "claude".into(),
        auth_id: id.clone(),
        model: model.into(),
        status: "cooling".into(),
        next_retry_after: Some(now + Duration::from_secs(secs)),
        reason: "quota".into(),
        quota: Quota {
            exceeded: true,
            reason: "quota".into(),
            next_recover_at: Some(now + Duration::from_secs(secs)),
            trust_windows: Some(1),
            ..Default::default()
        },
        ..Default::default()
    };
    cpa_server::cooldown_store::save(
        &dir,
        vec![record("CLAUDE-OPUS-4-6", 600), record("claude-opus-4-6", 1500)],
        now,
    )
    .unwrap();
    let (rt, _) = configured_runtime(config);
    let cooling = rt.store().cooldowns(&id);
    assert_eq!(
        cooling.iter().map(|c| c.model.as_str()).collect::<Vec<_>>(),
        ["Claude-Opus-4-6"],
        "migrated and merged: {cooling:?}"
    );
    assert!(
        cooling[0].remaining > Duration::from_secs(1400),
        "the later deadline: {cooling:?}"
    );
    let url = app(&rt).await;
    for spelling in ["claude-opus-4-6", "CLAUDE-OPUS-4-6"] {
        let (status, body) = post(&url, spelling).await;
        assert_eq!(status, 429, "{spelling}: {body}");
    }
    assert!(
        calls.lock().unwrap().is_empty(),
        "the migrated cooldown answers every spelling"
    );
    let rewritten = cpa_server::cooldown_store::load(&dir).unwrap();
    assert_eq!(
        rewritten.iter().map(|r| r.model.as_str()).collect::<Vec<_>>(),
        ["Claude-Opus-4-6"],
        "saved under the current key"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Control: an OAuth login sends each spelling as typed, so a cooldown under one does
/// not answer another.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spellings_sent_as_typed_keep_their_own_cooldowns() {
    let dir = auth_dir("typed");
    let (upstream, calls) = mock(429, LIMIT).await;
    let meta = json!({"type": "claude", "access_token": "sk-ant-oat01-FAKE-LOCAL-ONLY",
        "email": "fixture@example.invalid", "account_uuid": "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
        "claude_device_ids": ["0123456789abcdef".repeat(4)]});
    std::fs::write(dir.join("claude.json"), meta.to_string()).unwrap();
    let credential = cpa_server::testing::local(
        Credential::from_file(&dir, &dir.join("claude.json"), meta.as_object().unwrap().clone()).unwrap(),
    );
    let config = Config::parse(&format!("api-keys: [client-key]\nauth-dir: {}\n", dir.display())).unwrap();
    let rt = Arc::new(cpa_server::testing::runtime(
        config,
        vec![credential],
        Executors {
            claude: ClaudeExecutor::new(&upstream).unwrap(),
            ..executors()
        },
    ));
    let url = app(&rt).await;
    assert_eq!(post(&url, "claude-opus-4-6").await.0, 429);
    assert_eq!(post(&url, "CLAUDE-OPUS-4-6").await.0, 429);
    assert_eq!(calls.lock().unwrap().len(), 2, "each spelling reached the upstream");
    let _ = std::fs::remove_dir_all(&dir);
}

const FORCED: &str = r#"{"type":"error","error":{"type":"api_error","message":"forced overload"}}"#;
const PLAIN: &str = r#"{"type":"error","error":{"type":"api_error","message":"plain overload"}}"#;

/// One API-key credential file with cooling disabled and a forcing rule for 503s.
fn forcing(dir: &Path, upstream: &str) -> Credential {
    let meta = json!({"type": "claude", "api_key": "sk-fake-forced", "base_url": upstream,
        "disable_cooling": true,
        "request_scoped_errors": [{"status": 503, "match": ["forced"], "action": "stop-and-cooldown"}]});
    std::fs::write(dir.join("claude-f.json"), meta.to_string()).unwrap();
    let mut c = Credential::from_file(dir, &dir.join("claude-f.json"), meta.as_object().unwrap().clone()).unwrap();
    c.attributes.insert("api_key".into(), "sk-fake-forced".into());
    c.attributes.insert("base_url".into(), upstream.into());
    c
}

fn forcing_config(dir: &Path) -> Config {
    Config::parse(&format!(
        "api-keys: [client-key]\nauth-dir: {}\nrouting:\n  cooldown:\n    save-cooldown-status: true\n",
        dir.display()
    ))
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forced_cooldown_survives_disable_cooling_reload_and_restart() {
    let dir = auth_dir("forced");
    let (upstream, calls) = mock(503, FORCED).await;
    let start = || {
        Arc::new(cpa_server::testing::runtime(
            forcing_config(&dir),
            vec![forcing(&dir, &upstream)],
            executors(),
        ))
    };
    let rt = start();
    let url = app(&rt).await;
    assert_eq!(post(&url, "claude-opus-4-6").await.0, 503);
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert!(
        !rt.store().cooldowns("claude-f.json").is_empty(),
        "forced despite disable-cooling"
    );
    // A config reload clears ordinary cooldowns of a cooling-disabled credential, not
    // forced ones.
    assert!(rt.publish_config(forcing_config(&dir)));
    assert!(
        !rt.store().cooldowns("claude-f.json").is_empty(),
        "kept across the reload"
    );
    let (status, body) = post(&url, "claude-opus-4-6").await;
    assert_ne!(status, 200, "{body}");
    assert_eq!(calls.lock().unwrap().len(), 1, "still cooling after the reload");
    let saved = std::fs::read_to_string(dir.join("claude-f.cds")).expect("the forced cooldown is saved");
    assert!(
        saved.contains("force_cooldown_until") && saved.contains("force_cooldown_evaluated"),
        "{saved}"
    );
    assert!(!saved.contains("sk-fake-forced"));
    drop(rt);
    let restarted = start();
    assert!(
        !restarted.store().cooldowns("claude-f.json").is_empty(),
        "restored despite disable-cooling"
    );
    let url = app(&restarted).await;
    let (status, body) = post(&url, "claude-opus-4-6").await;
    assert_ne!(status, 200, "{body}");
    assert_eq!(calls.lock().unwrap().len(), 1, "still cooling after the restart");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Control: an ordinary failure under `disable-cooling` cools nothing and saves nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ordinary_failure_under_disable_cooling_cools_nothing() {
    let dir = auth_dir("ordinary");
    let (upstream, calls) = mock(503, PLAIN).await;
    let rt = Arc::new(cpa_server::testing::runtime(
        forcing_config(&dir),
        vec![forcing(&dir, &upstream)],
        executors(),
    ));
    let url = app(&rt).await;
    assert_eq!(post(&url, "claude-opus-4-6").await.0, 503);
    assert!(rt.store().cooldowns("claude-f.json").is_empty());
    assert_eq!(post(&url, "claude-opus-4-6").await.0, 503);
    assert!(calls.lock().unwrap().len() >= 2, "the upstream is tried again");
    assert!(rt.store().cooldowns("claude-f.json").is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}
