//! Request faults and safe quota evidence for Claude (#94): client -> cliproxy-rs ->
//! loopback mock Anthropic upstream, fake API keys and OAuth tokens in private
//! `auth-dir`s, under a network-denied runtime.
//!
//! - Anthropic's exhausted-credit 400 is a payment failure of that credential: the
//!   request fails over and the credential cools for 30 minutes (Go v8.0.67 d00267c3);
//!   any other 400 stays a request fault.
//! - A 401 a request-scoped rule matches is the rule's to handle: no refresh, and usage
//!   records the rule's classification in `fail.code` (4138edb1, 18883a57).
//! - A local Claude cooldown 429 tells the client when to retry and, when the cooldown
//!   came from a rejected Anthropic unified quota, replays exactly the quota headers
//!   Anthropic sent, nothing else and nothing invented; per-minute limits and Cloudflare
//!   challenges carry none. The evidence stays with the model it cools and survives a
//!   restart with `save-cooldown-status` (cb70eacf, e458999b).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use serde_json::{Value, json};

const MODEL: &str = "claude-opus-4-6";
const OK: &str = r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-4-6","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;
const CREDIT: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits."},"request_id":"req_credit"}"#;
const BAD_REQUEST: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages: at least one message is required"}}"#;

/// What the mock answers, by the request's API key or OAuth token and its body.
type Script = Arc<dyn Fn(&str, &str) -> Response + Send + Sync>;

#[derive(Clone)]
struct Mock {
    calls: Arc<Mutex<Vec<String>>>,
    script: Script,
}

async fn upstream(State(mock): State<Mock>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    let key = parts
        .headers
        .get("x-api-key")
        .or_else(|| parts.headers.get("authorization"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .trim_start_matches("Bearer ")
        .to_owned();
    mock.calls.lock().unwrap().push(format!("{} {key}", parts.uri.path()));
    (mock.script)(&key, &String::from_utf8_lossy(&body))
}

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

async fn mock(script: impl Fn(&str, &str) -> Response + Send + Sync + 'static) -> (String, Mock) {
    let mock = Mock {
        calls: Arc::default(),
        script: Arc::new(script),
    };
    let url = serve(axum::Router::new().fallback(upstream).with_state(mock.clone())).await;
    (url, mock)
}

fn auth_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("claude-faults-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// An API-key credential file `id` in `dir`, with extra metadata.
fn api_key(dir: &Path, id: &str, key: &str, upstream: &str, extra: Value) -> Credential {
    let mut meta = json!({"type": "claude", "api_key": key, "base_url": upstream});
    meta.as_object_mut()
        .unwrap()
        .extend(extra.as_object().cloned().unwrap_or_default());
    std::fs::write(dir.join(id), meta.to_string()).unwrap();
    let mut c = Credential::from_file(dir, &dir.join(id), meta.as_object().unwrap().clone()).unwrap();
    c.attributes.insert("api_key".into(), key.into());
    c.attributes.insert("base_url".into(), upstream.into());
    c
}

fn runtime(
    dir: &Path,
    extra_yaml: &str,
    credentials: Vec<Credential>,
    executors: Executors,
) -> Arc<cpa_server::Runtime> {
    let yaml = format!(
        "api-keys: [client-key]\nauth-dir: {}\nobservability: {{usage: {{usage-statistics-enabled: true}}}}\n{extra_yaml}",
        dir.display()
    );
    let config = Config::parse(&yaml).unwrap();
    let rt = Arc::new(cpa_server::testing::runtime(config.clone(), credentials, executors));
    rt.usage_queue().configure(true, &config);
    rt
}

fn executors(claude: ClaudeExecutor) -> Executors {
    Executors {
        claude,
        codex: Default::default(),
        meta: Default::default(),
        xai: Default::default(),
    }
}

async fn app(rt: &Arc<cpa_server::Runtime>) -> String {
    serve(cpa_server::app(rt.clone(), axum::Router::new())).await
}

async fn post(url: &str, path: &str, body: Value) -> (u16, HeaderMap, String) {
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
    let mut headers = HeaderMap::new();
    for (name, value) in response.headers() {
        headers.append(
            axum::http::HeaderName::from_bytes(name.as_str().as_bytes()).unwrap(),
            axum::http::HeaderValue::from_bytes(value.as_bytes()).unwrap(),
        );
    }
    (status, headers, response.text().await.unwrap())
}

fn messages(model: &str, text: &str, stream: bool) -> Value {
    json!({"model": model, "max_tokens": 16, "stream": stream, "messages": [{"role": "user", "content": text}]})
}

async fn usage(rt: &cpa_server::Runtime, want: usize) -> Vec<Value> {
    let mut got = Vec::new();
    for _ in 0..300 {
        got.extend(rt.usage_queue().pop_oldest(50));
        if got.len() >= want {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    got.extend(rt.usage_queue().pop_oldest(50));
    got.iter().map(|r| serde_json::from_slice(r).unwrap()).collect()
}

fn ok() -> Response {
    ([("content-type", "application/json")], OK).into_response()
}

fn error(status: u16, body: &'static str, headers: Vec<(&'static str, String)>) -> Response {
    let mut res = (StatusCode::from_u16(status).unwrap(), body).into_response();
    res.headers_mut()
        .insert("content-type", "application/json".parse().unwrap());
    for (name, value) in headers {
        res.headers_mut().insert(name, value.parse().unwrap());
    }
    res
}

/// The calls the mock saw, as `path key`.
fn calls(m: &Mock) -> Vec<String> {
    m.calls.lock().unwrap().clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exhausted_credit_fails_over_and_cools_that_credential() {
    let dir = auth_dir("credit");
    let (upstream_url, m) = mock(|key, _| {
        if key == "sk-fake-a" {
            error(400, CREDIT, vec![("request-id", "req_credit".into())])
        } else {
            ok()
        }
    })
    .await;
    let a = api_key(
        &dir,
        "claude-a.json",
        "sk-fake-a",
        &upstream_url,
        json!({"priority": 1}),
    );
    let b = api_key(&dir, "claude-b.json", "sk-fake-b", &upstream_url, json!({}));
    let rt = runtime(
        &dir,
        "",
        vec![a, b],
        executors(ClaudeExecutor::new("http://127.0.0.1:1").unwrap()),
    );
    let url = app(&rt).await;

    let (status, _, body) = post(&url, "/v1/messages", messages(MODEL, "hi", false)).await;
    assert_eq!(status, 200, "fails over to the funded credential: {body}");
    assert_eq!(calls(&m), ["/v1/messages sk-fake-a", "/v1/messages sk-fake-b"]);
    let cooling = rt.store().cooldowns("claude-a.json");
    assert_eq!(cooling.len(), 1, "{cooling:?}");
    assert!(
        cooling[0].remaining > Duration::from_secs(1700) && cooling[0].remaining <= Duration::from_secs(1800),
        "a 30-minute payment cooldown: {cooling:?}"
    );
    assert!(!cooling[0].quota, "not a quota window: {cooling:?}");
    assert!(rt.store().cooldowns("claude-b.json").is_empty());
    // The cooling credential is skipped from now on.
    let (status, _, _) = post(&url, "/v1/messages", messages(MODEL, "again", false)).await;
    assert_eq!(status, 200);
    assert_eq!(calls(&m).len(), 3);
    assert_eq!(calls(&m)[2], "/v1/messages sk-fake-b");
    // One record per attempt: the refused one keeps the upstream status and body.
    let records = usage(&rt, 3).await;
    assert_eq!(records.len(), 3, "{records:?}");
    let failed: Vec<&Value> = records.iter().filter(|r| r["failed"] == true).collect();
    assert_eq!(failed.len(), 1, "{records:?}");
    assert_eq!(failed[0]["fail"]["status_code"], 400);
    assert!(failed[0]["fail"]["body"].as_str().unwrap().contains("credit balance"));
    assert!(failed[0]["fail"].get("code").is_none(), "{:?}", failed[0]["fail"]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Control: any other 400 is the request's fault. No failover, no cooldown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn other_400s_stay_request_faults() {
    let dir = auth_dir("bad-request");
    let (upstream_url, m) = mock(|key, _| {
        if key == "sk-fake-a" {
            error(400, BAD_REQUEST, vec![])
        } else {
            ok()
        }
    })
    .await;
    let a = api_key(
        &dir,
        "claude-a.json",
        "sk-fake-a",
        &upstream_url,
        json!({"priority": 1}),
    );
    let b = api_key(&dir, "claude-b.json", "sk-fake-b", &upstream_url, json!({}));
    let rt = runtime(
        &dir,
        "",
        vec![a, b],
        executors(ClaudeExecutor::new("http://127.0.0.1:1").unwrap()),
    );
    let url = app(&rt).await;
    let (status, _, body) = post(&url, "/v1/messages", messages(MODEL, "hi", false)).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("at least one message"), "{body}");
    assert_eq!(calls(&m), ["/v1/messages sk-fake-a"]);
    assert!(rt.store().cooldowns("claude-a.json").is_empty());
    let records = usage(&rt, 1).await;
    assert_eq!(records.len(), 1);
    assert!(records[0]["fail"].get("code").is_none(), "{:?}", records[0]["fail"]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// An OAuth login whose upstream refuses its token, with a token endpoint on the mock.
async fn oauth_login(name: &str, rule: Option<&str>) -> (Arc<cpa_server::Runtime>, String, Mock, PathBuf) {
    let dir = auth_dir(name);
    let (upstream_url, m) = mock(|key, _| match key {
        "sk-ant-oat01-old-fake" => error(
            401,
            r#"{"type":"error","error":{"type":"authentication_error","message":"token expired"}}"#,
            vec![],
        ),
        _ => ok(),
    })
    .await;
    // The token endpoint shares the mock: a refresh shows up as a `/token` call.
    let token_mock = m.clone();
    let token_url = serve(axum::Router::new().fallback(move |req: Request| {
        let m = token_mock.clone();
        async move {
            m.calls.lock().unwrap().push(req.uri().path().to_owned());
            axum::Json(
                json!({"access_token": "sk-ant-oat01-new-fake", "refresh_token": "fake-rotated",
                        "expires_in": 3600, "account": {"uuid": "acct-fake", "email_address": "a@example.invalid"}}),
            )
            .into_response()
        }
    }))
    .await;
    let mut meta = json!({"type": "claude", "access_token": "sk-ant-oat01-old-fake", "refresh_token": "fake-refresh",
        "expired": "2099-01-01T00:00:00Z", "account_uuid": "acct-fake", "email": "a@example.invalid",
        "claude_device_ids": ["b".repeat(64)]});
    if let Some(action) = rule {
        meta["request_scoped_errors"] = json!([{"status": 401, "match": ["token expired"], "action": action}]);
    }
    std::fs::write(dir.join("claude.json"), meta.to_string()).unwrap();
    let mut credential =
        Credential::from_file(&dir, &dir.join("claude.json"), meta.as_object().unwrap().clone()).unwrap();
    credential.attributes.insert("base_url".into(), upstream_url.clone());
    let oauth = cpa_exec::oauth::OAuth::with_endpoints(
        wreq::Client::new(),
        &format!("{token_url}/token"),
        &format!("{token_url}/profile"),
        &format!("{token_url}/roles"),
    );
    let rt = runtime(
        &dir,
        "",
        vec![credential],
        executors(ClaudeExecutor::new(&upstream_url).unwrap().with_oauth(oauth)),
    );
    (rt, upstream_url, m, dir)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_401_a_rule_matches_is_never_refreshed() {
    for (action, cools, code) in [
        ("stop", false, "request_scoped"),
        ("stop-and-cooldown", true, "force_cooldown"),
    ] {
        let (rt, _, m, dir) = oauth_login(&format!("rule-{action}"), Some(action)).await;
        let url = app(&rt).await;
        let revision = rt.store().get("claude.json").unwrap().revision;
        let (status, _, body) = post(&url, "/v1/messages", messages(MODEL, "hi", false)).await;
        assert_eq!(status, 401, "{action}: {body}");
        assert_eq!(
            calls(&m),
            ["/v1/messages sk-ant-oat01-old-fake"],
            "{action}: no refresh and no retry"
        );
        assert_eq!(
            rt.store().get("claude.json").unwrap().revision,
            revision,
            "{action}: no credential write"
        );
        assert_eq!(!rt.store().cooldowns("claude.json").is_empty(), cools, "{action}");
        let records = usage(&rt, 1).await;
        assert_eq!(records.len(), 1, "{action}: {records:?}");
        assert_eq!(records[0]["fail"]["status_code"], 401);
        assert_eq!(records[0]["fail"]["code"], code, "{action}: {:?}", records[0]["fail"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Control: without a rule, a refused token is refreshed once and the request retried.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_401_without_a_rule_refreshes_once() {
    let (rt, _, m, dir) = oauth_login("no-rule", None).await;
    let url = app(&rt).await;
    let (status, _, body) = post(&url, "/v1/messages", messages(MODEL, "hi", false)).await;
    assert_eq!(status, 200, "{body}");
    let seen: Vec<String> = calls(&m)
        .iter()
        .map(|c| c.split(' ').next().unwrap().to_owned())
        .collect();
    assert_eq!(seen, ["/v1/messages", "/token", "/profile", "/v1/messages"]);
    let records = usage(&rt, 2).await;
    assert_eq!(records.len(), 2, "{records:?}");
    assert!(records.iter().all(|r| r["fail"].get("code").is_none()));
    let _ = std::fs::remove_dir_all(&dir);
}

fn epoch_in(d: Duration) -> String {
    (SystemTime::now() + d)
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string()
}

/// The Anthropic headers of a local 429, by name.
fn unified(headers: &HeaderMap) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = headers
        .iter()
        .filter(|(n, _)| n.as_str().starts_with("anthropic-ratelimit-unified-"))
        .map(|(n, v)| (n.as_str().to_owned(), v.to_str().unwrap().to_owned()))
        .collect();
    out.sort();
    out
}

fn retry_after(headers: &HeaderMap) -> u64 {
    headers["retry-after"].to_str().unwrap().parse().unwrap()
}

/// A local cooldown 429 on the Claude route: its retry time in the message and the
/// `Retry-After` header.
fn assert_local_cooldown(status: u16, headers: &HeaderMap, body: &str, label: &str) {
    assert_eq!(status, 429, "{label}: {body}");
    let v: Value = serde_json::from_str(body).unwrap();
    assert_eq!(v["type"], "error", "{label}: {body}");
    let message = v["error"]["message"].as_str().unwrap();
    assert!(message.contains("cooling down"), "{label}: {message}");
    assert!(message.contains(" (reset after "), "{label}: the retry time: {message}");
    assert!(retry_after(headers) > 0, "{label}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_local_cooldown_429_replays_only_the_rejected_quota_evidence() {
    let dir = auth_dir("unified");
    let reset = epoch_in(Duration::from_secs(1800));
    let sent = vec![
        ("anthropic-ratelimit-unified-status", "rejected".to_owned()),
        ("anthropic-ratelimit-unified-5h-status", "rejected".to_owned()),
        ("anthropic-ratelimit-unified-5h-reset", reset.clone()),
        ("anthropic-ratelimit-unified-5h-utilization", "1.0".to_owned()),
        ("anthropic-ratelimit-unified-7d-status", "allowed".to_owned()),
        ("anthropic-ratelimit-unified-7d-utilization", "0.42".to_owned()),
        (
            "anthropic-ratelimit-unified-representative-claim",
            "five_hour".to_owned(),
        ),
        ("anthropic-ratelimit-unified-reset", reset.clone()),
        // Not quota evidence: never replayed.
        ("request-id", "req_secret_trace".to_owned()),
        ("anthropic-organization-id", "org-secret".to_owned()),
        ("x-secret", "nope".to_owned()),
    ];
    let reply = sent.clone();
    let (upstream_url, m) = mock(move |_, _| {
        error(
            429,
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"limit reached"}}"#,
            reply.clone(),
        )
    })
    .await;
    let a = api_key(&dir, "claude-a.json", "sk-fake-a", &upstream_url, json!({}));
    let rt = runtime(
        &dir,
        "",
        vec![a],
        executors(ClaudeExecutor::new("http://127.0.0.1:1").unwrap()),
    );
    let url = app(&rt).await;
    let (status, _, _) = post(&url, "/v1/messages", messages(MODEL, "hi", false)).await;
    assert_eq!(status, 429, "the upstream's own answer");
    let mut want: Vec<(String, String)> = sent
        .iter()
        .filter(|(n, _)| n.starts_with("anthropic-ratelimit-unified-"))
        .map(|(n, v)| (n.to_string(), v.clone()))
        .collect();
    want.sort();
    for stream in [false, true] {
        let (status, headers, body) = post(&url, "/v1/messages", messages(MODEL, "again", stream)).await;
        assert_local_cooldown(status, &headers, &body, &format!("stream {stream}"));
        assert_eq!(
            unified(&headers),
            want,
            "stream {stream}: the provider's quota headers as sent"
        );
        for secret in [
            "request-id",
            "anthropic-organization-id",
            "x-secret",
            "x-api-key",
            "authorization",
        ] {
            assert!(!headers.contains_key(secret), "stream {stream}: {secret} leaked");
        }
        assert!(!body.contains("req_secret_trace") && !body.contains("org-secret"));
    }
    assert_eq!(calls(&m).len(), 1, "the cooling credential is not sent upstream");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Per-minute limits and Cloudflare challenges cool the credential too, but are no
/// subscription quota: their local 429 carries no Anthropic quota headers. Nor does a
/// 429 whose unified status Anthropic reported as allowed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_minute_and_cloudflare_cooldowns_carry_no_quota_headers() {
    let per_minute = r#"{"type":"error","error":{"type":"rate_limit_error","message":"This request would exceed the requests per minute rate limit. Please try again later."}}"#;
    let cloudflare =
        "<html><head><title>Just a moment...</title></head><body>cloudflare challenge-platform</body></html>";
    for (label, status, body, headers) in [
        (
            "per-minute",
            429,
            per_minute,
            vec![
                ("retry-after", "30".to_owned()),
                ("anthropic-ratelimit-unified-status", "allowed".to_owned()),
                ("anthropic-ratelimit-unified-5h-status", "allowed".to_owned()),
            ],
        ),
        ("cloudflare", 403, cloudflare, vec![]),
        (
            "plain 429",
            429,
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
            vec![],
        ),
    ] {
        let dir = auth_dir(&format!("no-quota-{}", label.replace(' ', "-")));
        let (upstream_url, _) = mock(move |_, _| error(status, body, headers.clone())).await;
        let a = api_key(&dir, "claude-a.json", "sk-fake-a", &upstream_url, json!({}));
        let rt = runtime(
            &dir,
            "",
            vec![a],
            executors(ClaudeExecutor::new("http://127.0.0.1:1").unwrap()),
        );
        let url = app(&rt).await;
        let _ = post(&url, "/v1/messages", messages(MODEL, "hi", false)).await;
        let (status, headers, body) = post(&url, "/v1/messages", messages(MODEL, "again", false)).await;
        assert_local_cooldown(status, &headers, &body, label);
        assert_eq!(unified(&headers), [], "{label}: no quota headers");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// A Fable-only weekly rejection is model-scoped: its evidence answers that model's local
/// 429 only, and a sibling model on the same credential still serves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_scoped_quota_evidence_stays_with_its_model() {
    let dir = auth_dir("fable");
    let reset = epoch_in(Duration::from_secs(1200));
    let sent = vec![
        ("anthropic-ratelimit-unified-status", "rejected".to_owned()),
        ("anthropic-ratelimit-unified-7d-status", "allowed".to_owned()),
        ("anthropic-ratelimit-unified-7d-utilization", "0.40".to_owned()),
        ("anthropic-ratelimit-unified-7d_oi-status", "rejected".to_owned()),
        ("anthropic-ratelimit-unified-7d_oi-reset", reset.clone()),
        ("retry-after", "1200".to_owned()),
    ];
    let reply = sent.clone();
    let (upstream_url, _) = mock(move |_, body| {
        if body.contains("claude-fable-5-1") {
            error(
                429,
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"weekly limit"}}"#,
                reply.clone(),
            )
        } else {
            ok()
        }
    })
    .await;
    let a = api_key(&dir, "claude-a.json", "sk-fake-a", &upstream_url, json!({}));
    let rt = runtime(
        &dir,
        "",
        vec![a],
        executors(ClaudeExecutor::new("http://127.0.0.1:1").unwrap()),
    );
    let url = app(&rt).await;
    let _ = post(&url, "/v1/messages", messages("claude-fable-5-1", "hi", false)).await;
    let (status, headers, body) = post(&url, "/v1/messages", messages("claude-fable-5-1", "again", false)).await;
    assert_local_cooldown(status, &headers, &body, "fable");
    let names: Vec<String> = unified(&headers).into_iter().map(|(n, _)| n).collect();
    assert_eq!(
        names,
        [
            "anthropic-ratelimit-unified-7d-status",
            "anthropic-ratelimit-unified-7d-utilization",
            "anthropic-ratelimit-unified-7d_oi-reset",
            "anthropic-ratelimit-unified-7d_oi-status",
            "anthropic-ratelimit-unified-status",
        ]
    );
    let (status, _, body) = post(&url, "/v1/messages", messages(MODEL, "sibling", false)).await;
    assert_eq!(status, 200, "a sibling model still serves: {body}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// With `save-cooldown-status`, a restart restores the cooldown with its evidence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_quota_evidence_survives_a_restart() {
    let dir = auth_dir("restart");
    let reset = epoch_in(Duration::from_secs(1500));
    let sent = vec![
        ("anthropic-ratelimit-unified-status", "rejected".to_owned()),
        ("anthropic-ratelimit-unified-7d-status", "rejected".to_owned()),
        ("anthropic-ratelimit-unified-7d-reset", reset.clone()),
        (
            "anthropic-ratelimit-unified-representative-claim",
            "seven_day".to_owned(),
        ),
    ];
    let reply = sent.clone();
    let (upstream_url, m) = mock(move |_, _| {
        error(
            429,
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"weekly limit"}}"#,
            reply.clone(),
        )
    })
    .await;
    let yaml = "routing:\n  cooldown:\n    save-cooldown-status: true\n";
    let start = |dir: &Path| {
        let a = api_key(dir, "claude-a.json", "sk-fake-a", &upstream_url, json!({}));
        runtime(
            dir,
            yaml,
            vec![a],
            executors(ClaudeExecutor::new("http://127.0.0.1:1").unwrap()),
        )
    };
    let rt = start(&dir);
    let url = app(&rt).await;
    let _ = post(&url, "/v1/messages", messages(MODEL, "hi", false)).await;
    let (_, before, _) = post(&url, "/v1/messages", messages(MODEL, "again", false)).await;
    assert!(!unified(&before).is_empty());
    let saved = std::fs::read_to_string(dir.join("claude-a.cds")).expect("the cooldown is saved");
    assert!(saved.contains("cooldown_signals"), "{saved}");
    assert!(!saved.contains("sk-fake-a"), "no credential in the saved state");
    drop(rt);
    let restarted = start(&dir);
    let url = app(&restarted).await;
    let (status, after, body) = post(&url, "/v1/messages", messages(MODEL, "after restart", false)).await;
    assert_local_cooldown(status, &after, &body, "restart");
    assert_eq!(unified(&after), unified(&before), "the same evidence after a restart");
    assert_eq!(calls(&m).len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}
