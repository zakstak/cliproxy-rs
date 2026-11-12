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

use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::IntoResponse;
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

/// A mock that records `key model` per call and answers each API key with its own reply.
async fn keyed_mock(
    reply: impl Fn(&str, &str) -> (u16, Vec<(&'static str, String)>, &'static str) + Send + Sync + 'static,
) -> (String, Calls) {
    let calls: Calls = Arc::default();
    let seen = calls.clone();
    let reply = Arc::new(reply);
    let app = axum::Router::new().fallback(move |req: Request| {
        let (seen, reply) = (seen.clone(), reply.clone());
        async move {
            let key = req
                .headers()
                .get("x-api-key")
                .or_else(|| req.headers().get("authorization"))
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .trim_start_matches("Bearer ")
                .to_owned();
            let body_in = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap();
            let model = serde_json::from_slice::<Value>(&body_in).unwrap()["model"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            seen.lock().unwrap().push(format!("{key} {model}"));
            let (status, headers, body) = reply(&key, &model);
            let mut res = (StatusCode::from_u16(status).unwrap(), body).into_response();
            res.headers_mut()
                .insert("content-type", "application/json".parse().unwrap());
            for (name, value) in headers {
                res.headers_mut().insert(name, value.parse().unwrap());
            }
            res
        }
    });
    (serve(app).await, calls)
}

const OK: &str = r#"{"id":"msg_1","type":"message","role":"assistant","model":"Claude-Opus-4-6","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;

async fn post_with(url: &str, model: &str, session: Option<&str>) -> (u16, String) {
    let body = json!({"model": model, "max_tokens": 8, "messages": [{"role": "user", "content": "hi"}]});
    let mut request = wreq::Client::new()
        .post(format!("{url}/v1/messages"))
        .header("x-api-key", "client-key")
        .header("content-type", "application/json");
    if let Some(session) = session {
        request = request.header("x-claude-code-session-id", session);
    }
    let response = request.body(body.to_string()).send().await.unwrap();
    let status = response.status().as_u16();
    (status, response.text().await.unwrap())
}

/// A saved quota record of `id` under `model`, `secs` from now.
fn quota_record(id: &str, model: &str, secs: u64) -> cpa_server::cooldown_store::Record {
    use cpa_server::cooldown_store::{Quota, Record};
    let at = SystemTime::now() + Duration::from_secs(secs);
    Record {
        provider: "claude".into(),
        auth_id: id.into(),
        model: model.into(),
        status: "cooling".into(),
        next_retry_after: Some(at),
        reason: "quota".into(),
        quota: Quota {
            exceeded: true,
            reason: "quota".into(),
            next_recover_at: Some(at),
            trust_windows: Some(1),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Two configured keys for one case-only aliased model; `sk-fake-a` takes precedence
/// under fill-first.
fn two_keys(dir: &Path, upstream: &str, routing: &str) -> Config {
    Config::parse(&format!(
        "api-keys: [client-key]\nauth-dir: {}\nrouting:\n  cooldown:\n    save-cooldown-status: true\n{routing}\
         claude-api-key:\n  - api-key: sk-fake-a\n    base-url: {upstream}\n    priority: 1\n    \
         models: [{{name: Claude-Opus-4-6, alias: claude-opus-4-6}}]\n  - api-key: sk-fake-b\n    \
         base-url: {upstream}\n    models: [{{name: Claude-Opus-4-6, alias: claude-opus-4-6}}]\n",
        dir.display()
    ))
    .unwrap()
}

/// A cooldown migrated from a record saved under an older spelling keeps its credential
/// out of every spelling's selection, with fill-first and soonest-reset, with and
/// without a sticky session: the other key serves, and the session stays with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_migrated_cooldown_holds_for_sticky_sessions_and_soonest_reset() {
    for (strategy, routing, session) in [
        ("fill-first", "  strategy: fill-first\n", None),
        (
            "fill-first sticky",
            "  strategy: fill-first\n  session-affinity: true\n",
            Some("6f0e8a51-3c2d-4b7a-9e10-2a4b6c8d0e1f"),
        ),
        ("soonest-reset", "  strategy: soonest-reset\n", None),
        (
            "soonest-reset sticky",
            "  strategy: soonest-reset\n  session-affinity: true\n",
            Some("6f0e8a51-3c2d-4b7a-9e10-2a4b6c8d0e1f"),
        ),
    ] {
        let dir = auth_dir(&format!("migrated-{}", strategy.replace(' ', "-")));
        let (upstream, calls) = keyed_mock(|key, _| {
            if key == "sk-fake-a" {
                (429, vec![("retry-after", "3600".into())], LIMIT)
            } else {
                (200, vec![], OK)
            }
        })
        .await;
        let config = two_keys(&dir, &upstream, routing);
        let a = cpa_core::config::credentials::load(&config)
            .into_iter()
            .find(|c| c.attributes.get("api_key").map(String::as_str) == Some("sk-fake-a"))
            .expect("key a")
            .id;
        cpa_server::cooldown_store::save(&dir, vec![quota_record(&a, "CLAUDE-OPUS-4-6", 1800)], SystemTime::now())
            .unwrap();
        let (rt, _) = configured_runtime(config);
        let url = app(&rt).await;
        for spelling in [
            "claude-opus-4-6",
            "Claude-Opus-4-6",
            "CLAUDE-OPUS-4-6",
            "claude-opus-4-6(high)",
        ] {
            let (status, body) = post_with(&url, spelling, session).await;
            assert_eq!(status, 200, "{strategy} {spelling}: {body}");
        }
        let seen = calls.lock().unwrap().clone();
        assert_eq!(seen.len(), 4, "{strategy}: {seen:?}");
        assert!(
            seen.iter().all(|c| c.starts_with("sk-fake-b ")),
            "{strategy}: the cooling key was never contacted: {seen:?}"
        );
        let cooling = rt.store().cooldowns(&a);
        assert_eq!(
            cooling.iter().map(|c| c.model.as_str()).collect::<Vec<_>>(),
            ["Claude-Opus-4-6"],
            "{strategy}: migrated"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

const FABLE_WEEKLY: &str = r#"{"type":"error","error":{"type":"rate_limit_error","message":"weekly limit"}}"#;

/// A Fable-only weekly rejection is model-scoped: it cools the configured Fable model
/// for every spelling of it, the other configured model on the same key still serves,
/// and a restart restores it under the same key, still model-scoped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_scoped_exhaustion_shares_its_key_and_spares_siblings() {
    let dir = auth_dir("fable-scoped");
    let (upstream, calls) = keyed_mock(|_, model| {
        if model.eq_ignore_ascii_case("claude-fable-5-1") {
            (
                429,
                vec![
                    ("anthropic-ratelimit-unified-status", "rejected".into()),
                    ("anthropic-ratelimit-unified-7d-status", "allowed".into()),
                    ("anthropic-ratelimit-unified-7d-utilization", "0.40".into()),
                    ("anthropic-ratelimit-unified-7d_oi-status", "rejected".into()),
                    ("retry-after", "1800".into()),
                ],
                FABLE_WEEKLY,
            )
        } else {
            (200, vec![], OK)
        }
    })
    .await;
    let config = || {
        Config::parse(&format!(
            "api-keys: [client-key]\nauth-dir: {}\nrouting:\n  cooldown:\n    save-cooldown-status: true\n\
             claude-api-key:\n  - api-key: sk-fake-a\n    base-url: {upstream}\n    models:\n      \
             - {{name: Claude-Fable-5-1, alias: claude-fable-5-1}}\n      - {{name: claude-opus-4-6, alias: opus}}\n",
            dir.display()
        ))
        .unwrap()
    };
    let (rt, id) = configured_runtime(config());
    let url = app(&rt).await;
    assert_eq!(post_with(&url, "claude-fable-5-1", None).await.0, 429);
    for spelling in ["CLAUDE-FABLE-5-1", "Claude-Fable-5-1"] {
        let (status, body) = post_with(&url, spelling, None).await;
        assert_eq!(status, 429, "{spelling}: {body}");
        assert!(body.contains("cooling down"), "{spelling}: {body}");
    }
    assert_eq!(
        post_with(&url, "opus", None).await.0,
        200,
        "a sibling model still serves"
    );
    assert_eq!(
        calls.lock().unwrap().clone(),
        ["sk-fake-a Claude-Fable-5-1", "sk-fake-a claude-opus-4-6"]
    );
    let cooling = rt.store().cooldowns(&id);
    assert_eq!(
        cooling.iter().map(|c| c.model.as_str()).collect::<Vec<_>>(),
        ["Claude-Fable-5-1"],
        "model-scoped, one key: {cooling:?}"
    );
    drop(rt);
    let (restarted, _) = configured_runtime(config());
    let cooling = restarted.store().cooldowns(&id);
    assert_eq!(
        cooling.iter().map(|c| c.model.as_str()).collect::<Vec<_>>(),
        ["Claude-Fable-5-1"],
        "restored model-scoped: {cooling:?}"
    );
    let url = app(&restarted).await;
    assert_eq!(post_with(&url, "claude-fable-5-1", None).await.0, 429);
    assert_eq!(post_with(&url, "opus", None).await.0, 200);
    assert_eq!(
        calls.lock().unwrap().len(),
        3,
        "only the sibling reached the upstream again"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Sol review of #136, through the credential store a request uses: a forced 60 s
/// bounded window survives a shorter ordinary answer and then `disable-cooling`; a
/// credential-wide 429 keeps a model's own forced deadline; a key migration keeps a
/// live probe reservation.
mod store_witnesses {
    use super::*;
    use cpa_core::exec::{ExecError, FailureScope};
    use cpa_server::runtime::{CredentialStore, Outcome, Selection};
    use cpa_server::scheduler::Policy;

    fn credential(dir: &Path, rule: Value) -> Credential {
        let meta = json!({"type": "claude", "api_key": "FAKE", "request_scoped_errors": rule});
        Credential::from_file(dir, &dir.join("a.json"), meta.as_object().unwrap().clone()).unwrap()
    }

    fn failure(status: u16, scope: FailureScope, text: &str, retry: Option<u64>) -> Outcome {
        let mut e = ExecError::local(status, scope, text.to_owned());
        e.retry_after = retry.map(Duration::from_secs);
        Outcome::Failure(e)
    }

    fn cooling_off(c: &Credential) -> Credential {
        let mut off = c.clone();
        off.metadata.insert("disable_cooling".into(), true.into());
        off
    }

    #[test]
    fn a_shorter_ordinary_answer_never_cuts_a_forced_deadline() {
        let dir = auth_dir("store-forced");
        let c = credential(
            &dir,
            json!([{"status": 429, "match": ["forced"], "action": "stop-and-cooldown"}]),
        );
        let store = CredentialStore::new(vec![c.clone()]);
        let p = Arc::new(Policy {
            max_trusted_cooldown: Duration::from_secs(60),
            ..Policy::default()
        });
        let admit = |_: &Credential| Some("m".to_owned());
        let forced = store
            .select_with(Selection::new("claude", "m"), p.clone(), &admit)
            .unwrap();
        let ordinary = store.select_with(Selection::new("claude", "m"), p, &admit).unwrap();
        forced.complete(failure(429, FailureScope::Model, "forced", Some(3600)));
        ordinary.complete(failure(429, FailureScope::Model, "ordinary", Some(10)));
        let states = store.cooldowns(&c.id);
        assert!(states[0].remaining > Duration::from_secs(50), "{states:?}");
        store.upsert(cooling_off(&c));
        let states = store.cooldowns(&c.id);
        assert!(
            states.len() == 1 && states[0].remaining > Duration::from_secs(50) && !states[0].quota,
            "the forced portion, no ordinary quota window: {states:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_credential_quota_keeps_a_models_forced_deadline() {
        for (scope, retry) in [(FailureScope::Credential, 3600), (FailureScope::Model, 1800)] {
            let dir = auth_dir(&format!("store-account-{scope:?}"));
            let c = credential(
                &dir,
                json!([{"status": 503, "match": ["forced"], "action": "stop-and-cooldown"}]),
            );
            let store = CredentialStore::new(vec![c.clone()]);
            let p = Arc::new(Policy::default());
            let admit = |_: &Credential| Some("m".to_owned());
            let forced = store
                .select_with(Selection::new("claude", "m"), p.clone(), &admit)
                .unwrap();
            let ordinary = store.select_with(Selection::new("claude", "m"), p, &admit).unwrap();
            forced.complete(failure(503, FailureScope::Model, "forced", None));
            ordinary.complete(failure(429, scope, "ordinary", Some(retry)));
            store.upsert(cooling_off(&c));
            let states = store.cooldowns(&c.id);
            assert_eq!(
                states.len(),
                1,
                "{scope:?}: only the model's forced portion: {states:?}"
            );
            assert_eq!(states[0].model, "m", "{scope:?}");
            assert!(
                states[0].remaining > Duration::from_secs(50) && states[0].remaining <= Duration::from_secs(60),
                "{scope:?}: {states:?}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn a_key_migration_keeps_a_live_probe_reservation() {
        use cpa_server::cooldown_store::{Quota, Record};
        let dir = auth_dir("store-probe");
        let config = Config::parse(&format!(
            "auth-dir: {}\nclaude-api-key:\n  - api-key: FAKE\n    base-url: http://127.0.0.1:9\n    \
             models: [{{name: M, alias: m}}]\n",
            dir.display()
        ))
        .unwrap();
        let configured = cpa_core::config::credentials::load(&config).remove(0);
        let mut c = configured.clone();
        c.metadata.remove("models");
        c.metadata.remove("model_aliases");
        let now = SystemTime::now();
        let record = |model: &str, secs: u64, bounded: bool| Record {
            provider: "claude".into(),
            auth_id: c.id.clone(),
            model: model.into(),
            status: "cooling".into(),
            next_retry_after: Some(now + Duration::from_secs(secs)),
            quota: if bounded {
                Quota {
                    exceeded: true,
                    reason: "quota".into(),
                    next_recover_at: Some(now + Duration::from_secs(3600)),
                    trust_windows: Some(1),
                    ..Default::default()
                }
            } else {
                Quota::default()
            },
            ..Default::default()
        };
        cpa_server::cooldown_store::save(&dir, vec![record("m", 1, true), record("M", 2, false)], now).unwrap();
        let store = CredentialStore::new(vec![c]);
        store.configure_cooldown_store(Some(dir.clone()));
        std::thread::sleep(Duration::from_millis(1100));
        let p = Arc::new(Policy::default());
        let first = store
            .select_with(Selection::new("claude", "m"), p.clone(), &|_: &Credential| {
                Some("m".into())
            })
            .expect("the bounded window's probe");
        assert!(
            store
                .select_with(Selection::new("claude", "m"), p.clone(), &|_: &Credential| Some(
                    "m".into()
                ))
                .is_err(),
            "control: one probe at a time"
        );
        store.upsert(configured);
        assert_eq!(
            store.cooldowns(&first.credential.id).len(),
            1,
            "control: the keys merged"
        );
        std::thread::sleep(Duration::from_millis(1100));
        let second = store.select_with(Selection::new("claude", "M"), p, &|_: &Credential| Some("M".into()));
        assert!(second.is_err(), "the migration kept the live probe reservation");
        drop(first);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Sol review of #136 at `3efe4c52`: a lease picked before a config-only alias publish
/// moved its key's cooldown still answers that cooldown, and its accepted stream
/// releases a reservation the move merged into an unbounded deadline owner. The OAuth
/// alias is parsed from a real config (`{name: M(high), alias: m}`), which sends `m`
/// upstream as `M(high)`, so `m`'s cooldown key moves to `M`. The publish leaves the
/// credential revision unchanged; a replaced or removed credential's lease answers
/// nothing.
mod migrated_leases {
    use super::*;
    use cpa_core::exec::{ExecError, FailureScope};
    use cpa_core::registry::dynamic::{OAuthAlias, cooldown_key, global_aliases};
    use cpa_server::cooldown_store::{Quota, Record};
    use cpa_server::runtime::{CredentialStore, Lease, Outcome, Selection};
    use cpa_server::scheduler::Policy;
    use std::collections::HashMap;

    struct Setup {
        dir: PathBuf,
        store: Arc<CredentialStore>,
        c: Credential,
        policy: Arc<Policy>,
    }

    /// One Claude OAuth login with saved cooldowns: `bounded` an ended bounded quota
    /// window, `unbounded` an ordinary cooldown that ends 100 ms later. Both have ended
    /// when this returns.
    fn setup(name: &str, bounded: &str, unbounded: Option<&str>) -> Setup {
        let dir = auth_dir(name);
        let meta = json!({"type": "claude", "access_token": "sk-ant-oat01-FAKE", "account_uuid": "synthetic"});
        let c = Credential::from_file(&dir, &dir.join("a.json"), meta.as_object().unwrap().clone()).unwrap();
        let now = SystemTime::now();
        let record = |model: &str, millis: u64, quota: Quota| Record {
            provider: "claude".into(),
            auth_id: c.id.clone(),
            model: model.into(),
            status: "cooling".into(),
            next_retry_after: Some(now + Duration::from_millis(millis)),
            quota,
            ..Default::default()
        };
        let mut records = vec![record(
            bounded,
            100,
            Quota {
                exceeded: true,
                reason: "quota".into(),
                next_recover_at: Some(now + Duration::from_secs(3600)),
                trust_windows: Some(1),
                ..Default::default()
            },
        )];
        records.extend(unbounded.map(|m| record(m, 200, Quota::default())));
        cpa_server::cooldown_store::save(&dir, records, now).unwrap();
        let store = CredentialStore::new(vec![c.clone()]);
        store.configure_cooldown_store(Some(dir.clone()));
        std::thread::sleep(Duration::from_millis(300));
        Setup {
            dir,
            store,
            c,
            policy: Arc::new(Policy::default()),
        }
    }

    fn aliases() -> HashMap<String, Vec<OAuthAlias>> {
        let config =
            Config::parse("oauth:\n  model-alias:\n    claude:\n      - name: M(high)\n        alias: m\n").unwrap();
        global_aliases(&config)
    }

    impl Setup {
        fn pick(&self, key: &str) -> Option<Lease> {
            let admit = |_: &Credential| Some(key.to_owned());
            self.store
                .select_with(Selection::new("claude", key), self.policy.clone(), &admit)
                .ok()
        }

        /// Publishes the alias config, as a config reload does, and checks the move.
        fn publish(&self) {
            let revision = self.store.get(&self.c.id).unwrap().revision;
            let aliases = aliases();
            assert_eq!(
                cooldown_key(&aliases, &self.c, "m"),
                "M(high)",
                "the parsed alias applies"
            );
            self.store.set_model_aliases(aliases);
            assert_eq!(self.store.get(&self.c.id).unwrap().revision, revision, "config only");
            let keys: Vec<String> = self.store.cooldowns(&self.c.id).into_iter().map(|s| s.model).collect();
            assert!(!keys.contains(&"m".to_owned()), "moved: {keys:?}");
        }
    }

    impl Drop for Setup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn a_lease_picked_before_an_alias_publish_answers_its_migrated_key() {
        // Completion and stream acceptance, after one publish and after a repeated one.
        for (accept, publishes) in [(false, 1), (true, 1), (false, 2), (true, 2)] {
            let case = format!("accepted {accept}, {publishes} publishes");
            let s = setup(&format!("old-key-{accept}-{publishes}"), "m", None);
            let lease = s.pick("m").expect("the ended window's probe");
            for _ in 0..publishes {
                s.publish();
            }
            assert!(s.pick("M").is_none(), "{case}: the move keeps the hold");
            if accept {
                lease.accepted();
                assert!(
                    s.pick("M").is_some(),
                    "{case}: the lease's stream answers the moved key"
                );
                lease.complete(Outcome::Success);
            } else {
                lease.complete(Outcome::Success);
                assert!(
                    s.pick("M").is_some(),
                    "{case}: the lease's result answers the moved key"
                );
            }
            assert!(
                s.store.cooldowns(&s.c.id).is_empty(),
                "{case}: nothing left under either key"
            );
        }
        // A failure of such a lease cools the moved key, not the old spelling.
        let s = setup("old-key-failure", "m", None);
        let lease = s.pick("m").unwrap();
        s.publish();
        let mut e = ExecError::local(429, FailureScope::Model, "slow down".to_owned());
        e.retry_after = Some(Duration::from_secs(600));
        lease.complete(Outcome::Failure(e));
        let states = s.store.cooldowns(&s.c.id);
        assert!(
            states.len() == 1 && states[0].model == "M" && states[0].remaining > Duration::from_secs(500),
            "{states:?}"
        );
        // Control: without a publish the lease answers its own key.
        let s = setup("old-key-control", "m", None);
        let lease = s.pick("m").unwrap();
        assert!(s.pick("m").is_none());
        lease.complete(Outcome::Success);
        assert!(s.pick("m").is_some());
    }

    #[test]
    fn an_accepted_stream_releases_a_reservation_merged_into_an_unbounded_owner() {
        // Both directions: the probe reserved under the key that stays (`M`) or under the
        // one that moves (`m`); the later, unbounded cooldown owns the merged deadline.
        for (reserved, other) in [("M", "m"), ("m", "M")] {
            let s = setup(&format!("unbounded-owner-{reserved}"), reserved, Some(other));
            let lease = s.pick(reserved).expect("the ended window's probe");
            assert!(s.pick(reserved).is_none(), "one probe at a time");
            s.publish();
            assert!(s.pick("M").is_none(), "{reserved}: the merge keeps the hold");
            lease.accepted();
            assert!(s.pick("M").is_some(), "{reserved}: the accepted stream releases it");
            lease.complete(Outcome::Success);
        }
        // Control: without a publish the accepted stream releases its own key.
        let s = setup("unbounded-owner-control", "M", Some("m"));
        let lease = s.pick("M").unwrap();
        assert!(s.pick("M").is_none());
        lease.accepted();
        assert!(s.pick("M").is_some());
        lease.complete(Outcome::Success);
    }

    #[test]
    fn a_replaced_or_removed_credential_s_lease_answers_nothing() {
        // Replaced: a metadata edit gives a new revision; the old lease's stream and
        // result are ignored, so the moved hold stays until it lapses.
        let s = setup("stale-replaced", "m", None);
        let lease = s.pick("m").unwrap();
        s.publish();
        let mut edited = s.c.clone();
        edited.metadata.insert("note".into(), "edited".into());
        s.store.upsert(edited);
        lease.accepted();
        assert!(s.pick("M").is_none(), "a stale lease's stream releases nothing");
        lease.complete(Outcome::Success);
        assert!(s.pick("M").is_none(), "a stale lease's result releases nothing");
        assert_eq!(s.store.cooldowns(&s.c.id).len(), 1, "the moved window stays");
        // Removed: its cooldowns go with it, and the old lease's result recreates none.
        let s = setup("stale-removed", "m", None);
        let lease = s.pick("m").unwrap();
        s.publish();
        let revision = s.store.get(&s.c.id).unwrap().revision;
        let bytes = std::fs::read(s.dir.join("a.json")).unwrap_or_default();
        s.store.replace_file_backed(&s.c.id, revision, &bytes, None).unwrap();
        assert!(s.store.get(&s.c.id).is_none());
        lease.complete(Outcome::Success);
        assert!(s.store.cooldowns(&s.c.id).is_empty());
    }
}
