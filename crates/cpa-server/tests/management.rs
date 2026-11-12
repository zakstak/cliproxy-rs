//! Disposable local server only: no provider endpoints or credential exchanges.
use cpa_core::config::Config;
use cpa_core::gostr::GoStr;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use cpa_server::management::Management;
use cpa_server::{Runtime, management, watching};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;

mod forward_port;

struct Fixture {
    dir: PathBuf,
    state: Arc<Management>,
    rt: Arc<Runtime>,
}
#[tokio::test]
async fn codex_passive_quota_snapshots_appear_in_credential_entries() {
    let f = Fixture::from_yaml("codex-quota", |auth, hash| {
        std::fs::write(
            auth.join("codex-a.json"),
            r#"{"type":"codex","email":"a@example.invalid"}"#,
        )
        .unwrap();
        std::fs::write(
            auth.join("claude-b.json"),
            r#"{"type":"claude","email":"b@example.invalid"}"#,
        )
        .unwrap();
        format!(
            "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\n",
            auth.display()
        )
    });
    let snapshot = f.rt.store().snapshot();
    let id = |provider: &str| snapshot.iter().find(|c| c.provider == provider).unwrap().id.clone();
    let (codex, claude) = (id("codex"), id("claude"));
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("x-codex-plan-type", "pro".parse().unwrap());
    headers.insert("x-codex-primary-used-percent", "12".parse().unwrap());
    headers.insert("x-unrelated", "1".parse().unwrap());
    // Signals recorded for another provider's credential never surface (Go keys
    // observation by provider).
    f.rt.executors.codex.quota().observe(&codex, "", &headers);
    f.rt.executors.codex.quota().observe(&claude, "", &headers);
    let (base, server) = f.server().await;
    let list = || async {
        wreq::Client::new()
            .get(format!("{base}/v8/management/credentials"))
            .bearer_auth("fake-management-only")
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()
    };
    let listed = list().await;
    let entry_in = |listed: &Value, name: &str| {
        listed["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == name)
            .unwrap()
            .clone()
    };
    let entry = |name: &str| entry_in(&listed, name);
    let quota = entry("codex-a.json")["quota"].clone();
    assert_eq!(
        quota["signals"],
        json!({"X-Codex-Plan-Type": "pro", "X-Codex-Primary-Used-Percent": "12"})
    );
    // Go time.Time JSON (RFC 3339 with an offset), close to now.
    let observed = chrono::DateTime::parse_from_rfc3339(quota["observed_at"].as_str().unwrap()).unwrap();
    assert!((chrono::Utc::now() - observed.to_utc()).num_seconds().abs() < 60);
    assert_eq!(entry("claude-b.json")["quota"], json!({"signals": {}}));
    assert!(entry("codex-a.json").get("model_quotas").is_none());

    // Per-model observations (Go model_quotas): keyed by the model without its
    // thinking suffix; the credential quota is the latest snapshot.
    let mut mini = axum::http::HeaderMap::new();
    mini.insert("x-codex-primary-used-percent", "70".parse().unwrap());
    f.rt.executors.codex.quota().observe(&codex, "gpt-5.4(high)", &headers);
    f.rt.executors.codex.quota().observe(&codex, "gpt-5.4-mini", &mini);
    f.rt.executors
        .codex
        .quota()
        .observe(&claude, "claude-sonnet-4-6", &headers);
    let listed = list().await;
    server.abort();
    let codex_entry = entry_in(&listed, "codex-a.json");
    let models = codex_entry["model_quotas"].as_object().unwrap();
    assert_eq!(models.keys().collect::<Vec<_>>(), ["gpt-5.4", "gpt-5.4-mini"]);
    assert_eq!(
        models["gpt-5.4"]["signals"],
        json!({"X-Codex-Plan-Type": "pro", "X-Codex-Primary-Used-Percent": "12"})
    );
    assert_eq!(
        models["gpt-5.4-mini"]["signals"],
        json!({"X-Codex-Primary-Used-Percent": "70"})
    );
    assert!(models["gpt-5.4-mini"]["observed_at"].is_string());
    assert_eq!(
        codex_entry["quota"]["signals"],
        json!({"X-Codex-Primary-Used-Percent": "70"})
    );
    assert!(entry_in(&listed, "claude-b.json").get("model_quotas").is_none());
}

impl Fixture {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("manage-{name}-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("auth")).unwrap();
        let path = dir.join("config.yaml");
        let hash = bcrypt::hash("fake-management-only", 4).unwrap();
        std::fs::write(&path, format!("# operator note\nconfig-version: 8\nserver:\n  host: '127.0.0.1' # listener\n  port: 0\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\nrouting:\n  retry:\n    request-retry: 3 # attempts\naccess:\n  api-keys: [fake-client]\n", dir.join("auth").display())).unwrap();
        let cfg = Config::load(&path).unwrap();
        let rt = Arc::new(cpa_server::testing::runtime(
            cfg,
            vec![],
            Executors {
                claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
                codex: Default::default(),
                meta: Default::default(),
                xai: Default::default(),
            },
        ));
        let state = Management::new(rt.clone(), path);
        Self { dir, state, rt }
    }
    /// A fixture whose runtime holds the credentials `yaml` synthesizes.
    fn from_yaml(name: &str, yaml: impl Fn(&std::path::Path, &str) -> String) -> Self {
        Self::from_yaml_with(name, yaml, Default::default())
    }
    /// [`Self::from_yaml`] with its Codex executor.
    fn from_yaml_with(
        name: &str,
        yaml: impl Fn(&std::path::Path, &str) -> String,
        codex: cpa_exec::codex::CodexExecutor,
    ) -> Self {
        let dir = std::env::temp_dir().join(format!("manage-{name}-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("auth")).unwrap();
        let path = dir.join("config.yaml");
        let hash = bcrypt::hash("fake-management-only", 4).unwrap();
        std::fs::write(&path, yaml(&dir.join("auth"), &hash)).unwrap();
        let cfg = Config::load(&path).unwrap();
        let scan = cpa_core::config::credentials::scan_auth_dir(&cfg);
        let mut loaded = scan.credentials;
        loaded.extend(cpa_core::config::credentials::from_config(&cfg));
        let rt = Arc::new(cpa_server::testing::runtime(
            cfg.clone(),
            loaded,
            Executors {
                claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
                codex,
                meta: Default::default(),
                xai: Default::default(),
            },
        ));
        // As the binary records what it loaded.
        rt.store().note_loaded(&scan.read);
        let state = Management::new(rt.clone(), path);
        Self { dir, state, rt }
    }
    async fn server(&self) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = management::router(self.state.clone());
        let handle = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap();
        });
        (base, handle)
    }
    fn file(&self) -> String {
        std::fs::read_to_string(self.dir.join("config.yaml")).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test]
async fn real_http_config_rejections_comments_publication_and_panel_assets() {
    let f = Fixture::new("http");
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    let request = |method, path: &str| {
        client
            .request(method, format!("{base}/v8/management{path}"))
            .bearer_auth("fake-management-only")
    };
    let before = f.file();
    assert_eq!(
        client
            .get(format!("{base}/v8/management/config"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let config: Value = request(wreq::Method::GET, "/config")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(config["routing"]["retry"]["request-retry"], 3);
    assert_eq!(f.file(), before, "GET must not migrate/persist");
    for (path, value, status) in [
        ("/config/server/port", json!("bad"), 422),
        ("/config/access", Value::Null, 422),
        ("/config/oauth/providers/codex/unknown", json!(true), 400),
        (
            "/config/credentials/concurrency/lifecycle-config-revision",
            json!(5),
            400,
        ),
        ("/config/access/api-keys/0", json!("invalid-index"), 400),
        ("/config/routing/retry/request-retry", json!({"value":2}), 422),
    ] {
        let response = request(wreq::Method::PUT, path).json(&value).send().await.unwrap();
        assert_eq!(response.status().as_u16(), status, "{path}");
        assert_eq!(f.file(), before, "rejected {path} changed disk");
        assert_eq!(f.rt.config().routing.retry.request_retry, 3);
    }
    let r = request(wreq::Method::PUT, "/config/routing/retry/request-retry")
        .json(&0)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(f.rt.config().routing.retry.request_retry, 0);
    let text = f.file();
    assert!(
        text.contains("# operator note") && text.contains("# listener") && text.contains("# attempts"),
        "comments disappeared"
    );
    assert!(text.find("server:").unwrap() < text.find("management:").unwrap());
    let r = request(wreq::Method::PATCH, "/config")
        .json(&json!({"routing":{"cooldown":{"disable-cooling":false}}, "access":{"api-keys":[]}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(f.rt.config().api_keys.is_empty());
    assert_eq!(
        request(wreq::Method::DELETE, "/config/routing/retry/request-retry")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        request(wreq::Method::GET, "/config/routing/retry/request-retry")
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let html = client.get(format!("{base}/management.html")).send().await.unwrap();
    assert_eq!(html.status(), 200);
    let html = html.text().await.unwrap();
    for piece in html
        .split('"')
        .filter(|p| p.starts_with("./assets/") || p.starts_with("./fonts/"))
    {
        assert_eq!(
            client
                .get(format!("{base}/{}", piece.trim_start_matches("./")))
                .send()
                .await
                .unwrap()
                .status(),
            200,
            "asset {piece}"
        );
    }
    server.abort();
}

#[tokio::test]
async fn watcher_keeps_last_good_config_and_reconciles_disabled_deleted_and_self_writes() {
    let f = Fixture::new("reload");
    let auth = f.dir.join("auth/fake.json");
    std::fs::write(
        &auth,
        r#"{"type":"claude","access_token":"fake-token","email":"fake@example.invalid","unknown":{"keep":7}}"#,
    )
    .unwrap();
    watching::reload(&f.state).unwrap();
    let revision = f.rt.store().get("fake.json").unwrap().revision;
    watching::reload(&f.state).unwrap();
    assert_eq!(f.rt.store().get("fake.json").unwrap().revision, revision);
    std::fs::write(&auth, "{incomplete").unwrap();
    watching::reload(&f.state).unwrap();
    assert_eq!(f.rt.store().get("fake.json").unwrap().revision, revision);
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    let value: Value = client
        .patch(format!("{base}/v8/management/credentials/status"))
        .bearer_auth("fake-management-only")
        .json(&json!({"name":"fake.json","disabled":true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(value["disabled"], true);
    let saved: Value = serde_json::from_slice(&std::fs::read(&auth).unwrap()).unwrap();
    assert_eq!(saved["unknown"]["keep"], 7);
    let disabled_revision = f.rt.store().get("fake.json").unwrap().revision;
    watching::reload(&f.state).unwrap();
    assert!(f.rt.store().get("fake.json").unwrap().disabled);
    assert_eq!(f.rt.store().get("fake.json").unwrap().revision, disabled_revision);
    std::fs::write(f.dir.join("config.yaml"), "").unwrap();
    std::fs::remove_file(&auth).unwrap();
    watching::reload(&f.state).unwrap();
    assert!(
        f.rt.store().snapshot().is_empty(),
        "empty config must not retain deleted auth"
    );
    assert_eq!(f.rt.config().api_keys, ["fake-client"]);
    std::fs::write(&auth, serde_json::to_vec(&saved).unwrap()).unwrap();
    watching::reload(&f.state).unwrap();
    assert!(f.rt.store().get("fake.json").unwrap().disabled);
    std::fs::write(f.dir.join("config.yaml"), "access: null\n").unwrap();
    assert!(watching::reload(&f.state).is_err());
    assert_eq!(f.rt.config().api_keys, ["fake-client"]);
    std::fs::remove_file(&auth).unwrap();
    assert!(watching::reload(&f.state).is_err());
    assert!(
        f.rt.store().snapshot().is_empty(),
        "bad config must not retain deleted auth"
    );
    std::fs::write(
        f.dir.join("config.yaml"),
        format!(
            "oauth: {{auth-dir: {}}}\nrequest-retry: 7\n",
            f.dir.join("auth").display()
        ),
    )
    .unwrap();
    watching::reload(&f.state).unwrap();
    assert_eq!(f.rt.config().routing.retry.request_retry, 7);
    assert!(f.rt.store().snapshot().is_empty());
    server.abort();
}

#[tokio::test]
async fn remote_policy_does_not_trust_forwarded_headers_and_key_hashing_preserves_source() {
    let f = Fixture::new("auth");
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    let r = client
        .get(format!("{base}/v8/management/config"))
        .header("X-Forwarded-For", "203.0.113.5")
        .header("X-Management-Key", "fake-management-only")
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        200,
        "untrusted forwarded header must not change local identity"
    );
    for _ in 0..5 {
        assert_eq!(
            client
                .get(format!("{base}/v8/management/config"))
                .bearer_auth("wrong")
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
    }
    assert_eq!(
        client
            .get(format!("{base}/v8/management/config"))
            .bearer_auth("fake-management-only")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    server.abort();
    let path = f.dir.join("plaintext.yaml");
    std::fs::write(
        &path,
        "# keep\nremote-management:\n  secret-key: fake-plaintext # key note\nport: 0\n",
    )
    .unwrap();
    let cfg = Config::load(&path).unwrap();
    assert!(bcrypt::verify("fake-plaintext", &cfg.management.secret_key).unwrap());
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("# keep") && text.contains("# key note"));
    assert!(!text.contains("fake-plaintext"));
    assert!(
        text.contains("remote-management:"),
        "startup must not migrate legacy source"
    );
}

/// Unix only: the read-only case needs Unix directory permissions, enforced (not root).
#[test]
fn plaintext_secret_loads_from_a_read_only_config_and_inherited_keys() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("manage-readonly-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.yaml");
    let text = "port: 8317\nremote-management:\n  secret-key: fake-plain-secret\n";
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    // Root (also uid 0 in a user namespace) writes through these modes, so the
    // read-only case cannot happen there and is not checked.
    let enforced =
        std::fs::OpenOptions::new().append(true).open(&path).is_err() && std::fs::write(dir.join("probe"), "").is_err();
    let loaded = enforced.then(|| Config::load(&path));
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    if let Some(loaded) = loaded {
        // Go: hashed in memory, persistence failure ignored.
        let cfg = loaded.expect("read-only config must load");
        assert!(bcrypt::verify("fake-plain-secret", &cfg.management.secret_key).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text, "file untouched");
    } else {
        let _ = std::fs::remove_file(dir.join("probe"));
        // CI sets CPA_TEST_NO_SKIP: a run as root must not pass without the check.
        assert!(
            std::env::var_os("CPA_TEST_NO_SKIP").is_none(),
            "file permissions are not enforced (running as root?), and CPA_TEST_NO_SKIP is set"
        );
        eprintln!("file permissions are not enforced (running as root?): read-only config case not checked");
    }
    // Keys inherited through a merge (legacy and v8 layout, at the root or inside the
    // parent mapping) are persisted hashed, with no plaintext left in the merge.
    for text in [
        "<<: {remote-management: {secret-key: fake-merged-secret}}\n",
        "<<: {management: {secret-key: fake-merged-secret}}\n",
        "management:\n  <<: {secret-key: fake-merged-secret}\n",
        "remote-management:\n  <<: {secret-key: fake-merged-secret}\n  allow-remote: false\n",
    ] {
        let layout = text;
        std::fs::write(&path, text).unwrap();
        let cfg = Config::load(&path).unwrap();
        assert!(bcrypt::verify("fake-merged-secret", &cfg.management.secret_key).unwrap());
        let file = std::fs::read_to_string(&path).unwrap();
        assert!(!file.contains("fake-merged-secret"), "{layout}: {file}");
        assert_eq!(
            Config::load(&path).unwrap().management.secret_key,
            cfg.management.secret_key
        );
    }
    // A writable plain layout is persisted as a hash, comments kept.
    std::fs::write(&path, "# keep\nmanagement:\n  secret-key: fake-v8-secret # note\n").unwrap();
    let cfg = Config::load(&path).unwrap();
    let file = std::fs::read_to_string(&path).unwrap();
    assert!(file.contains("# keep") && file.contains("# note") && !file.contains("fake-v8-secret"));
    assert!(file.contains(&cfg.management.secret_key));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn management_bodies_are_unbounded_but_the_open_callback_is_not() {
    let f = Fixture::new("body-limit");
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    let note = "x".repeat(3 * 1024 * 1024);
    let r = client
        .post(format!("{base}/v8/management/credentials?name=large.json"))
        .bearer_auth("fake-management-only")
        .header("Content-Type", "application/json")
        .body(format!(r#"{{"type":"codex","note":"{note}"}}"#))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    assert!(f.dir.join("auth/large.json").metadata().unwrap().len() > 3 * 1024 * 1024);
    let r = client
        .post(format!("{base}/v8/management/oauth/callback"))
        .header("Content-Type", "application/json")
        .body(format!(r#"{{"state":"s","code":"{note}"}}"#))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 413);
    server.abort();
}

/// Waits up to four seconds for `done`.
async fn eventually(done: impl Fn() -> bool) {
    tokio::time::timeout(std::time::Duration::from_secs(4), async {
        while !done() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("condition not reached");
}

#[tokio::test]
async fn watcher_reconciles_auth_while_the_config_file_is_missing() {
    let f = Fixture::new("missing-config");
    let watcher = watching::start(&f.state);
    let auth = |name: &str| f.dir.join("auth").join(name);
    std::fs::write(auth("a.json"), r#"{"type":"claude","access_token":"fake-a"}"#).unwrap();
    eventually(|| f.rt.store().get("a.json").is_some()).await;
    // Go keeps serving and still applies auth events after the config disappears.
    let config = f.file();
    std::fs::remove_file(f.dir.join("config.yaml")).unwrap();
    std::fs::write(auth("b.json"), r#"{"type":"claude","access_token":"fake-b"}"#).unwrap();
    std::fs::remove_file(auth("a.json")).unwrap();
    eventually(|| f.rt.store().get("b.json").is_some() && f.rt.store().get("a.json").is_none()).await;
    assert_eq!(f.rt.config().api_keys, ["fake-client"], "last good config kept");
    // An unchanged rewrite (same bytes) is not a change; an edit to the restored
    // config is applied.
    std::fs::write(
        f.dir.join("config.yaml"),
        config.replace("[fake-client]", "[fake-next]"),
    )
    .unwrap();
    eventually(|| f.rt.config().api_keys == ["fake-next"]).await;
    let revision = f.rt.store().get("b.json").unwrap().revision;
    std::fs::write(auth("b.json"), r#"{"type":"claude","access_token":"fake-b"}"#).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert_eq!(f.rt.store().get("b.json").unwrap().revision, revision);
    // A changed auth file of the same size is noticed (racy timestamps re-hash).
    std::fs::write(auth("b.json"), r#"{"type":"claude","access_token":"fake-c"}"#).unwrap();
    eventually(|| f.rt.store().get("b.json").unwrap().revision != revision).await;
    watcher.abort();
}

#[tokio::test]
async fn polling_publishes_owner_legacy_layout_and_ignores_non_auth_directories() {
    let f = Fixture::new("polling");
    std::fs::create_dir_all(f.dir.join("auth/static")).unwrap();
    std::fs::create_dir_all(f.dir.join("auth/logs")).unwrap();
    let watcher = watching::start(&f.state);
    std::fs::write(
        f.dir.join("config.yaml"),
        format!("host: 127.0.0.1\nport: 0\nremote-management: {{allow-remote: false}}\nauth-dir: {}\napi-keys: [owner-fake-client]\nrequest-retry: 5\n", f.dir.join("auth").display()),
    ).unwrap();
    std::fs::write(
        f.dir.join("auth/fake.json"),
        r#"{"type":"claude","access_token":"fake","disabled":true}"#,
    )
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(4), async {
        loop {
            if f.rt.config().routing.retry.request_retry == 5 && f.rt.store().snapshot().len() == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(f.rt.config().api_keys, ["owner-fake-client"]);
    assert!(!f.rt.config().management.allow_remote);
    assert!(f.rt.store().get("fake.json").unwrap().disabled);
    assert!(f.file().contains("remote-management:"), "load must not migrate layout");
    watcher.abort();
}

#[tokio::test]
async fn nonlocal_socket_requires_allow_remote_even_with_a_valid_key() {
    let f = Fixture::new("nonlocal");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v8/management/config", listener.local_addr().unwrap());
    // Inject a nonlocal socket identity, not a forwarded header, into real HTTP.
    let app = management::router(f.state.clone()).layer(axum::middleware::from_fn(
        |mut req: axum::extract::Request, next: axum::middleware::Next| async move {
            let peer = req
                .headers()
                .get("X-Test-Peer")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("203.0.113.8:3456")
                .parse::<std::net::SocketAddr>()
                .unwrap();
            req.extensions_mut().insert(axum::extract::ConnectInfo(peer));
            next.run(req).await
        },
    ));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = wreq::Client::new();
    assert_eq!(
        client
            .get(&url)
            .bearer_auth("fake-management-only")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        client
            .get(&url)
            .header("X-Test-Peer", "[::ffff:127.0.0.1]:3456")
            .bearer_auth("fake-management-only")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let mut config = (*f.rt.config()).clone();
    config.management.allow_remote = true;
    f.rt.publish_config(config);
    assert_eq!(
        client
            .get(&url)
            .bearer_auth("fake-management-only")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(client.get(&url).send().await.unwrap().status(), 401);
    server.abort();
}

/// The binary's composition (`cpa_server::app`): API routes first, management and its
/// NoRoute behind them. The API keeps gin's NoRoute rules (bare 404, no `Allow`, HEAD on
/// `/healthz` only); management keeps its own HEAD handling, so a HEAD under
/// `/v0/management` reaches the guard before its bare 404.
#[tokio::test]
async fn served_app_keeps_api_and_management_no_route_rules() {
    let f = Fixture::new("served-app");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = cpa_server::app(f.rt.clone(), management::router(f.state.clone()));
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    let client = wreq::Client::new();
    let send = |method: wreq::Method, path: &str, key: Option<&str>| {
        let mut req = client.request(method, format!("{base}{path}"));
        if let Some(key) = key {
            req = req.bearer_auth(key);
        }
        async move { req.send().await.unwrap() }
    };
    let get = wreq::Method::GET;
    let head = wreq::Method::HEAD;
    assert_eq!(
        send(get.clone(), "/v8/management/config", Some("fake-management-only"))
            .await
            .status(),
        200
    );
    assert_eq!(send(get.clone(), "/v1/models", None).await.status(), 401);
    assert_eq!(send(get.clone(), "/v1/models", Some("fake-client")).await.status(), 200);
    for (method, path) in [
        (get.clone(), "/v1/chat/completions"),
        (head.clone(), "/v1/models"),
        (get.clone(), "/v2/nothing"),
        (head.clone(), "/v2/nothing"),
    ] {
        let res = send(method.clone(), path, Some("fake-client")).await;
        assert_eq!(res.status(), 404, "{method} {path}");
        assert!(
            res.headers().get("allow").is_none(),
            "{method} {path}: {:?}",
            res.headers()
        );
    }
    assert_eq!(send(head.clone(), "/healthz", None).await.status(), 200);
    // Go's NoRoute guards /v0/management before looking for a plugin route; with none,
    // the answer is a bare 404.
    assert_eq!(send(head.clone(), "/v0/management/nothing", None).await.status(), 401);
    let res = send(head, "/v0/management/nothing", Some("fake-management-only")).await;
    assert_eq!(res.status(), 404);
    server.abort();
}

#[tokio::test]
async fn config_api_keys_become_scheduled_credentials_on_write_and_reload() {
    use cpa_server::runtime::Selection;
    let f = Fixture::new("synth");
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    let put = |path: &str, value: Value| {
        client
            .put(format!("{base}/v8/management{path}"))
            .bearer_auth("fake-management-only")
            .json(&value)
    };
    let r = put(
        "/config/api-keys/claude",
        json!([{"name": "team", "base-url": "https://claude.example.invalid", "prefix": "team", "priority": 4,
                "keys": [{"api-key": "fake-key-a"}, {"api-key": "fake-key-b", "priority": 9, "weight": 0,
                          "disable-cooling": false, "excluded-models": ["Opus-*"]}]}]),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(r.status(), 200);
    // Published immediately by the write, without waiting for the watcher.
    let claude: Vec<_> =
        f.rt.store()
            .snapshot()
            .into_iter()
            .filter(|c| c.provider == "claude")
            .collect();
    assert_eq!(claude.len(), 2);
    let b = claude.iter().find(|c| c.attributes["api_key"] == "fake-key-b").unwrap();
    assert!(b.id.starts_with("claude:apikey:"));
    assert_eq!(b.attributes["priority"], "9");
    assert_eq!(b.attributes["weight"], "0");
    assert_eq!(b.attributes["prefix"], "team");
    assert_eq!(b.attributes["base_url"], "https://claude.example.invalid");
    assert_eq!(b.attributes["excluded_models"], "opus-*");
    assert_eq!(b.metadata["disable_cooling"], false);
    // The scheduler honours the synthesized priority and prefix.
    let sel = |model: &str| Selection {
        provider: "claude".into(),
        model: model.into(),
        ..Selection::default()
    };
    let lease = f.rt.store().select(sel("team/claude-sonnet-4-6")).unwrap();
    assert_eq!(lease.credential.attributes["api_key"], "fake-key-b", "priority 9 wins");
    assert_eq!(lease.execution_model, "claude-sonnet-4-6");
    drop(lease);
    let lease = f.rt.store().select(sel("team/opus-4")).unwrap();
    assert_eq!(
        lease.credential.attributes["api_key"], "fake-key-a",
        "b excludes opus-*"
    );
    drop(lease);
    // Credentials listing never shows config API keys (Go lists files only).
    let listed: Value = client
        .get(format!("{base}/v8/management/credentials"))
        .bearer_auth("fake-management-only")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed["files"], json!([]));

    // OAuth request-scoped rules reach the policy; invalid rules are dropped.
    let r = put(
        "/config/oauth/request-scoped-errors",
        json!({"Claude": [{"status": 400, "match": [" overloaded "], "action": " STOP "},
                          {"status": 0, "match": ["x"], "action": "stop"}]}),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(r.status(), 200);
    let rules = &f.rt.policy().oauth_request_scoped_errors["claude"];
    assert_eq!(rules.len(), 1);
    assert_eq!(
        (rules[0].r#match[0].as_str(), rules[0].action.as_str()),
        ("overloaded", "stop")
    );

    // A hand edit picked up by the watcher replaces the set; removed keys disappear.
    let text = f.file().replace("fake-key-a", "fake-key-c");
    std::fs::write(f.dir.join("config.yaml"), text).unwrap();
    watching::reload(&f.state).unwrap();
    let keys: Vec<String> =
        f.rt.store()
            .snapshot()
            .iter()
            .filter_map(|c| c.attributes.get("api_key").cloned())
            .collect();
    assert!(keys.contains(&"fake-key-c".to_owned()) && !keys.contains(&"fake-key-a".to_owned()));
    server.abort();
}

/// A number or bool written as the key is hashed as the string it decodes to (Go
/// `ParseConfigBytes`), so it authenticates at once, before any reload.
#[tokio::test]
async fn typed_secret_keys_are_hashed_as_the_string_they_decode_to() {
    let f = Fixture::new("typed-secret");
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    let get = |key: &str| {
        client
            .get(format!("{base}/v8/management/config/server/port"))
            .bearer_auth(key)
            .send()
    };
    let mut old = "fake-management-only";
    for (written, key) in [(json!(123), "123"), (json!(true), "true")] {
        let r = client
            .put(format!("{base}/v8/management/config/management/secret-key"))
            .bearer_auth(old)
            .json(&written)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{written}");
        assert_eq!(get(key).await.unwrap().status(), 200, "{written}: new key");
        assert_eq!(get(old).await.unwrap().status(), 401, "{written}: old key");
        let file: serde_yaml_ng::Value = serde_yaml_ng::from_str(&f.file()).unwrap();
        let hash = file["management"]["secret-key"].as_str().expect("stored as a string");
        assert!(bcrypt::verify(key, hash).unwrap(), "{written}: {hash}");
        old = key;
    }
    server.abort();
}

#[tokio::test]
async fn unreadable_auth_dir_never_blocks_secret_rotation_or_removal() {
    let f = Fixture::new("authfile");
    // A regular file where the auth directory should be: ReadDir fails, like Go's
    // synthesizer this is an empty file set and the config still publishes.
    let blocker = f.dir.join("not-a-dir");
    std::fs::write(&blocker, "x").unwrap();
    let text = f.file().replace(
        &f.dir.join("auth").display().to_string(),
        &blocker.display().to_string(),
    );
    std::fs::write(
        f.dir.join("config.yaml"),
        text + "claude-api-key: [{api-key: fake-cfg-key}]\n",
    )
    .unwrap();
    watching::reload(&f.state).unwrap();
    assert_eq!(f.rt.config().auth_dir, blocker);
    assert!(f.rt.store().snapshot().iter().any(|c| c.provider == "claude"));
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    let get = |key: &str| {
        client
            .get(format!("{base}/v8/management/config/server/port"))
            .bearer_auth(key)
            .send()
    };
    let r = client
        .put(format!("{base}/v8/management/config/management/secret-key"))
        .bearer_auth("fake-management-only")
        .json(&json!("fake-rotated"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        get("fake-management-only").await.unwrap().status(),
        401,
        "old key revoked"
    );
    assert_eq!(get("fake-rotated").await.unwrap().status(), 200);
    let text = f.file();
    assert!(!text.contains("fake-rotated"), "secret stored hashed");
    // Removing the secret by hand disables management entirely on the next reload.
    let mut doc: serde_yaml_ng::Value = serde_yaml_ng::from_str(&text).unwrap();
    doc["management"].as_mapping_mut().unwrap().remove("secret-key");
    std::fs::write(f.dir.join("config.yaml"), serde_yaml_ng::to_string(&doc).unwrap()).unwrap();
    watching::reload(&f.state).unwrap();
    let r = get("fake-rotated").await.unwrap();
    assert_eq!((r.status().as_u16(), r.text().await.unwrap()), (404, String::new()));
    server.abort();
}

/// A v8 write saves what it wrote, as Go v8.0.35 does (the runtime config still
/// sanitizes OAuth maps). Deliberate difference: Go re-encodes the whole file, while
/// untouched keys here stay byte-stable.
#[tokio::test]
async fn writes_normalize_only_what_they_touch() {
    let f = Fixture::new("scoped");
    let path = f.dir.join("config.yaml");
    let dirty = "  excluded-models:\n    Claude: [' Opus-X ', opus-x] # dirty\n    xai: [a]\n  model-alias:\n    claude:\n      - {name: a, alias: a} # self alias\n";
    let text = f.file().replacen("oauth:\n", &format!("oauth:\n{dirty}"), 1)
        + "observability:\n  pprof:\n    addr: null # keep\n";
    std::fs::write(&path, &text).unwrap();
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    let send = |method, at: &str, body: &str| {
        client
            .request(method, format!("{base}/v8/management{at}"))
            .bearer_auth("fake-management-only")
            .body(body.to_owned())
            .send()
    };
    let r = send(
        wreq::Method::PUT,
        "/config/oauth/excluded-models/xai",
        r#"[" B ", "b", "c"]"#,
    )
    .await
    .unwrap();
    assert_eq!(r.status(), 200);
    let file = f.file();
    assert!(file.contains("    Claude: [' Opus-X ', opus-x] # dirty\n"), "{file}");
    assert!(file.contains("      - {name: a, alias: a} # self alias\n"), "{file}");
    assert!(file.contains("    addr: null # keep\n"), "{file}");
    let config: Value = send(wreq::Method::GET, "/config", "")
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(config["oauth"]["excluded-models"]["xai"], json!([" B ", "b", "c"]));
    let r = send(
        wreq::Method::PATCH,
        "/config",
        r#"{"routing":{"retry":{"request-retry":1}}}"#,
    )
    .await
    .unwrap();
    assert_eq!(r.status(), 200);
    let after_patch = f.file();
    assert!(after_patch.contains("    addr: null # keep\n"), "{after_patch}");
    assert!(
        after_patch.contains("    Claude: [' Opus-X ', opus-x] # dirty\n"),
        "{after_patch}"
    );
    let r = send(wreq::Method::PATCH, "/config", "{}").await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(f.file(), after_patch, "an empty PATCH writes nothing");
    // A YAML 1.1 bool spelling is saved and read back as the string written; only the
    // runtime config decodes it.
    let r = send(wreq::Method::PUT, "/config/upstream/xai/inject-x-search", r#""on""#)
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let file: serde_yaml_ng::Value = serde_yaml_ng::from_str(&f.file()).unwrap();
    assert_eq!(
        file["upstream"]["xai"]["inject-x-search"],
        serde_yaml_ng::Value::from("on")
    );
    let got = send(wreq::Method::GET, "/config/upstream/xai/inject-x-search", "")
        .await
        .unwrap();
    assert_eq!(got.text().await.unwrap(), r#""on""#);
    assert_eq!(
        f.rt.config().document["upstream"]["xai"]["inject-x-search"],
        serde_yaml_ng::Value::Bool(true)
    );
    server.abort();
}

/// Go v8.0.35 `TestConfigV8ETagIfMatch`: reads carry the file's SHA-256 as a strong
/// ETag, and a write whose `If-Match` names another version fails with 412.
#[tokio::test]
async fn config_writes_honour_if_match_against_the_file_etag() {
    use sha2::{Digest, Sha256};
    let f = Fixture::new("etag");
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    let etag = || {
        let hex: String = Sha256::digest(f.file()).iter().map(|b| format!("{b:02x}")).collect();
        format!("\"{hex}\"")
    };
    let send = |method, at: &str, body: &str, if_match: &[&str]| {
        let mut req = client
            .request(method, format!("{base}/v8/management{at}"))
            .bearer_auth("fake-management-only")
            .body(body.to_owned());
        for v in if_match {
            req = req.header("If-Match", *v);
        }
        req.send()
    };
    // `TestConfigV8ETagExposedForCORS`: every answer lets a cross-origin page read it.
    let tag = |r: &wreq::Response| {
        let exposed = r.headers()["access-control-expose-headers"].to_str().unwrap();
        assert!(exposed.split(',').any(|h| h.trim() == "ETag"), "{exposed}");
        r.headers()["etag"].to_str().unwrap().to_owned()
    };
    let (put, patch, delete, get) = (
        wreq::Method::PUT,
        wreq::Method::PATCH,
        wreq::Method::DELETE,
        wreq::Method::GET,
    );

    let before = f.file();
    let r = send(
        patch.clone(),
        "/config",
        r#"{"routing":{"retry":{"request-retry":1}}}"#,
        &[r#""stale""#],
    )
    .await
    .unwrap();
    assert_eq!((r.status().as_u16(), tag(&r)), (412, etag()));
    assert_eq!(r.text().await.unwrap(), r#"{"error":"precondition_failed"}"#);
    assert_eq!(f.file(), before, "a stale PATCH writes nothing");
    let current = etag();
    let r = send(
        patch,
        "/config",
        r#"{"routing":{"retry":{"request-retry":1}}}"#,
        &[&current],
    )
    .await
    .unwrap();
    assert_eq!(r.status(), 200);
    assert_ne!(tag(&r), current);
    assert_eq!(tag(&r), etag(), "the write answers with the new version on disk");

    for at in ["/config.yaml", "/config", "/config/server"] {
        let r = send(get.clone(), at, "", &[r#""stale""#]).await.unwrap();
        assert_eq!(
            (r.status().as_u16(), tag(&r)),
            (200, etag()),
            "{at}: reads ignore If-Match"
        );
    }
    let stale = etag();
    let r = send(put.clone(), "/config/access/api-keys", r#"["other-key"]"#, &[])
        .await
        .unwrap();
    assert_eq!(
        (r.status().as_u16(), tag(&r)),
        (200, etag()),
        "absent If-Match stays unconditional"
    );
    let before = f.file();
    let r = send(
        put.clone(),
        "/config.yaml",
        &before.replace("other-key", "revoked-key"),
        &[&stale],
    )
    .await
    .unwrap();
    assert_eq!((r.status().as_u16(), tag(&r)), (412, etag()));
    let r = send(delete.clone(), "/config/access/api-keys", "", &[&stale])
        .await
        .unwrap();
    assert_eq!(r.status(), 412);
    assert_eq!(f.file(), before);

    // Strong comparison only; a quoted comma is tag content and `*` must be the whole field.
    let current = etag();
    let weak = format!("W/{current}");
    let comma = format!("{},\"", current.trim_end_matches('"'));
    for bad in [
        &weak,
        "not-an-etag",
        r#""unterminated"#,
        r#""a"b""#,
        r#""stale,*,tag""#,
        r#"W/"stale,*,tag""#,
        r#""stale", *"#,
        &comma,
        "",
        "  \t  ",
    ] {
        let r = send(put.clone(), "/config/server/port", "8318", &[bad]).await.unwrap();
        assert_eq!(r.status(), 412, "{bad:?}");
    }
    assert_eq!(f.file(), before);
    let r = send(put.clone(), "/config/server/port", "8318", &[r#""stale""#, &current])
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "repeated lines form one list");
    let list = format!(r#""stale", {} , W/{}"#, etag(), etag());
    let r = send(delete, "/config/access/api-keys", "", &[&list]).await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(!f.file().contains("other-key"));
    let r = send(put.clone(), "/config/server/port", "8320", &["*"]).await.unwrap();
    assert_eq!(r.status(), 200);
    let next = tag(&r);
    // Go `strings.TrimSpace` also trims Unicode white space (here U+00A0).
    let padded = format!("\u{a0}{next}\u{a0}");
    let r = send(put, "/config/server/port", "8321", &[&padded]).await.unwrap();
    assert_eq!(r.status(), 200, "the response ETag chains into the next write");
    server.abort();
}

/// The status toggle edits only the targeted key, located in the file it edits
/// (here reordered externally and not yet published), and keeps sibling text.
#[tokio::test]
async fn config_key_toggle_edits_only_the_target_key_in_the_current_file() {
    let groups = |first: &str, second: &str| {
        format!(
            "api-keys:\n  claude:\n    - base-url: https://claude.example.invalid # group one\n      keys:\n        {first}\n        {second}\n    - keys:\n        - api-key: fake-c # third\n"
        )
    };
    let (a, b) = ("- api-key: fake-a # first key", "- api-key: fake-b # second key");
    let f = Fixture::from_yaml("toggle", |auth, hash| {
        format!(
            "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\n{}",
            auth.display(),
            groups(a, b)
        )
    });
    let cfg = f.rt.config();
    let ids = cpa_core::config::credentials::from_config(&cfg);
    let id_b = ids
        .iter()
        .find(|c| c.attributes.get("api_key").map(String::as_str) == Some("fake-b"))
        .unwrap()
        .id
        .clone();
    // External edit: the two keys swap places; the watcher has not published it.
    let path = f.dir.join("config.yaml");
    let swapped = f.file().replace(&groups(a, b), &groups(b, a));
    assert_ne!(swapped, f.file());
    std::fs::write(&path, &swapped).unwrap();
    let (base, server) = f.server().await;
    let r = wreq::Client::new()
        .patch(format!("{base}/v8/management/credentials/status"))
        .bearer_auth("fake-management-only")
        .json(&json!({"name": id_b, "disabled": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let file = f.file();
    for kept in [
        "    - base-url: https://claude.example.invalid # group one\n",
        "        - api-key: fake-a # first key\n",
        "        - api-key: fake-c # third\n",
    ] {
        assert!(file.contains(kept), "lost {kept:?}:\n{file}");
    }
    let doc: serde_yaml_ng::Value = serde_yaml_ng::from_str(&file).unwrap();
    let keys = &doc["api-keys"]["claude"][0]["keys"];
    assert_eq!(keys[0]["api-key"], "fake-b");
    assert_eq!(keys[0]["excluded-models"], serde_yaml_ng::Value::from(vec!["*"]));
    assert!(
        keys[1].get("excluded-models").is_none(),
        "fake-a must stay enabled:\n{file}"
    );
    server.abort();
}

/// #28: a Management API write that marks a `codex-api-key` model `is-compat` is refused,
/// through the legacy list route, the v8 config path and a whole-config upload alike: the
/// file and the running config keep the last good value. Another provider's `is-compat`
/// still saves.
#[tokio::test]
async fn management_writes_refuse_a_codex_is_compat_model() {
    let f = Fixture::from_yaml("codex-compat", |auth, hash| {
        format!(
            "management:\n  secret-key: '{hash}'\nauth-dir: {}\ncodex-api-key:\n  - api-key: fake-codex\n    base-url: https://codex.example.invalid\n    models: [{{name: a, alias: b}}]\n",
            auth.display()
        )
    });
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    let before = f.file();
    let compat = json!([{"api-key": "fake-codex", "base-url": "https://codex.example.invalid", "models": [{"name": "a", "alias": "b", "is-compat": true}]}]);
    let writes = [
        client
            .put(format!("{base}/v0/management/codex-api-key"))
            .bearer_auth("fake-management-only")
            .json(&compat),
        client
            .put(format!("{base}/v8/management/config/api-keys/codex"))
            .bearer_auth("fake-management-only")
            .json(&json!([{"base-url": "https://codex.example.invalid", "models": [{"name": "a", "alias": "b", "is-compat": true}], "keys": [{"api-key": "fake-codex"}]}])),
        client
            .put(format!("{base}/v0/management/config.yaml"))
            .bearer_auth("fake-management-only")
            .header("content-type", "application/yaml")
            .body(before.replace("alias: b}", "alias: b, is-compat: true}")),
    ];
    for (i, write) in writes.into_iter().enumerate() {
        let r = write.send().await.unwrap();
        let status = r.status().as_u16();
        let text = r.text().await.unwrap();
        assert!((400..500).contains(&status), "write {i}: {status} {text}");
        assert!(text.contains("is-compat"), "write {i}: {text}");
        assert_eq!(f.file(), before, "write {i}: the file keeps the last good config");
        let models = cpa_core::config::credentials::api_key_entries(&f.rt.config(), "codex")[0]
            .models
            .clone();
        assert!(
            models.iter().all(|m| m.get("is-compat").is_none()),
            "write {i}: {models:?}"
        );
    }
    let r = client
        .put(format!("{base}/v0/management/claude-api-key"))
        .bearer_auth("fake-management-only")
        .json(&json!([{"api-key": "fake-claude", "base-url": "https://claude.example.invalid", "models": [{"name": "a", "alias": "b", "is-compat": true}]}]))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "other providers keep is-compat");
    server.abort();
}

/// Go d69962e4 and 3e309fb4: a config API key is not an auth file, so the fields patch
/// does not find it (404) and changes nothing, in memory or in config.yaml.
#[tokio::test]
async fn config_key_field_patch_is_not_found() {
    let f = Fixture::from_yaml("cfgfields", |auth, hash| {
        format!(
            "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\napi-keys:\n  claude:\n    - excluded-models: [config-*]\n      keys:\n        - api-key: fake-k\n",
            auth.display()
        )
    });
    let before = f.rt.store().snapshot()[0].clone();
    let file = f.file();
    let (base, server) = f.server().await;
    let r = wreq::Client::new()
        .patch(format!("{base}/v8/management/credentials/fields"))
        .bearer_auth("fake-management-only")
        .json(&json!({"name": before.id, "note": " updated ", "excluded_models": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    assert_eq!(
        r.json::<Value>().await.unwrap(),
        json!({"error": "auth file not found"})
    );
    let after = f.rt.store().snapshot()[0].clone();
    assert_eq!(
        (&after.attributes, &after.metadata),
        (&before.attributes, &before.metadata)
    );
    assert_eq!(f.file(), file, "config.yaml must not change");
    server.abort();
}

/// Go `TestCodexExcludedModelsCredentialPriorityAfterPatchAndReload` (b015f208,
/// 94d1a9a0): a credential's own excluded models replace the global
/// `oauth.excluded-models`; with none, the current global list applies, so a later
/// config change reaches the same credential without rewriting it.
#[tokio::test]
async fn credential_excluded_models_take_priority_over_the_global_list() {
    let empty = Config::parse("").unwrap();
    let probe = std::path::Path::new("/auth/codex.json");
    let probe =
        cpa_core::config::credentials::from_file(&empty, probe.parent().unwrap(), probe, br#"{"type":"codex"}"#)
            .unwrap()
            .unwrap();
    let models = cpa_core::registry::dynamic::models_for(&empty, &Default::default(), &probe, 0);
    let (global, own) = (models[0].id.clone(), models[models.len() - 1].id.clone());
    let f = Fixture::from_yaml("excl-priority", |auth, hash| {
        std::fs::write(auth.join("codex.json"), r#"{"type":"codex"}"#).unwrap();
        format!(
            "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\n  excluded-models:\n    codex: ['{global}']\n",
            auth.display()
        )
    });
    let id = f.rt.store().snapshot()[0].id.clone();
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    let available = |stage: &str, want_global: bool, want_own: bool| {
        let registry = f.rt.registry();
        let got = (
            registry.client_supports(&id, &global),
            registry.client_supports(&id, &own),
        );
        assert_eq!(got, (want_global, want_own), "{stage}");
    };
    let patch = |rules: Value| {
        client
            .patch(format!("{base}/v8/management/credentials/fields"))
            .bearer_auth("fake-management-only")
            .json(&json!({"name": "codex.json", "excluded_models": rules}))
            .send()
    };
    available("global fallback", false, true);
    assert_eq!(patch(json!([own.go_upper()])).await.unwrap().status(), 200);
    available("credential override", true, false);
    assert_eq!(patch(json!([])).await.unwrap().status(), 200);
    available("empty falls back to the global list", false, true);
    let cred = f.rt.store().snapshot().into_iter().find(|c| c.id == id).unwrap();
    assert!(
        !cred.attributes.contains_key("excluded_models"),
        "global rules are not copied in"
    );
    let r = client
        .put(format!("{base}/v8/management/config/oauth/excluded-models/codex"))
        .bearer_auth("fake-management-only")
        .body(json!([own]).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    available("updated global list", true, false);
    server.abort();
}

/// Go `auth_files_excluded_models_test` (1a28c61e, 3e309fb4): the fields patch takes a
/// string array or null and stores only the credential's normalized rules; any other
/// shape, and an upload with invalid rules, is a 400 that changes nothing.
#[tokio::test]
async fn excluded_models_writes_are_validated() {
    let f = Fixture::from_yaml("excl-writes", |auth, hash| {
        std::fs::write(
            auth.join("credential.json"),
            r#"{"type":"codex","excluded-models":["old-*"]}"#,
        )
        .unwrap();
        format!(
            "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\n  excluded-models:\n    codex: [global-*]\n",
            auth.display()
        )
    });
    let auth = f.dir.join("auth");
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    let patch = |fields: &str| {
        client
            .patch(format!("{base}/v8/management/credentials/fields"))
            .bearer_auth("fake-management-only")
            .header("content-type", "application/json")
            .body(format!(r#"{{"name":"credential.json",{fields}}}"#))
            .send()
    };
    let current = || {
        f.rt.store()
            .snapshot()
            .into_iter()
            .find(|c| c.id == "credential.json")
            .unwrap()
    };
    for (fields, want) in [
        (
            r#""excluded_models":[" Local-* ","LOCAL-*","b-*"]"#,
            Some("b-*,local-*"),
        ),
        (r#""excluded-models":["alias-*"]"#, Some("alias-*")),
        (r#""excluded_models":[],"excluded-models":["alias-*"]"#, None),
        (r#""excluded_models":["x"]"#, Some("x")),
        (r#""excluded_models":[" ","\t"]"#, None),
        (r#""excluded_models":["y"]"#, Some("y")),
        (r#""excluded_models":null"#, None),
    ] {
        let r = patch(fields).await.unwrap();
        assert_eq!(r.status(), 200, "{fields}");
        let c = current();
        assert_eq!(
            c.attributes.get("excluded_models").map(String::as_str),
            want,
            "{fields}"
        );
        assert!(
            !c.metadata.contains_key("excluded-models"),
            "{fields}: legacy alias retained"
        );
    }
    let before = (
        current().metadata.clone(),
        std::fs::read(auth.join("credential.json")).unwrap(),
    );
    for fields in [
        r#""excluded_models":123"#,
        r#""excluded-models":true"#,
        r#""excluded_models":{}"#,
        r#""excluded_models":["codex-new",null]"#,
        r#""excluded_models":[42,false]"#,
        r#""excluded_models":[["codex-new"]]"#,
        r#""excluded_models":"codex-new""#,
        r#""excluded-models":"codex-new""#,
        r#""excluded_models.child":["codex-new"]"#,
        r#""excluded-models.0":"codex-new""#,
    ] {
        let r = patch(&format!(r#""abc":true,{fields}"#)).await.unwrap();
        assert_eq!(r.status(), 400, "{fields}");
        let after = (
            current().metadata.clone(),
            std::fs::read(auth.join("credential.json")).unwrap(),
        );
        assert_eq!(after, before, "{fields}: a rejected patch changes nothing");
    }
    for content in [
        r#"{"type":"codex","excluded_models":123}"#,
        r#"{"type":"claude","excluded_models":123}"#,
        r#"{"type":"codex","excluded_models":["codex-new",1]}"#,
    ] {
        let r = client
            .post(format!("{base}/v8/management/credentials?name=bad.json"))
            .bearer_auth("fake-management-only")
            .body(content)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "{content}");
        let error = r.json::<Value>().await.unwrap()["error"].as_str().unwrap().to_owned();
        assert!(
            error.starts_with("invalid auth file: metadata excluded_models"),
            "{error}"
        );
        assert!(!auth.join("bad.json").exists(), "{content}: nothing is written");
    }
    // The alias is canonicalized first, as Go's NormalizeCredentialMetadata does.
    let r = client
        .post(format!("{base}/v8/management/credentials?name=bad.json"))
        .bearer_auth("fake-management-only")
        .body(r#"{"type":"codex","excluded-models":["a",true]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(
        r.text().await.unwrap(),
        r#"{"error":"invalid auth file: metadata excluded_models[1] must be a string, got bool"}"#
    );
    assert!(!auth.join("bad.json").exists());
    server.abort();
}

/// `/credentials/models` reports what the credential registers in the dynamic
/// registry (Go `GetModelsForClient`): config aliases here, nothing once disabled.
#[tokio::test]
async fn fields_refuses_values_its_auth_loader_cannot_read() {
    // Go writes these values (DIFFERENCES-FROM-GO.md): serde holds the file, so a
    // number past f64 or nesting past its 127 levels, counted from the file's root
    // object, is refused before anything is written.
    let f = Fixture::from_yaml("fields-limits", |auth, hash| {
        std::fs::write(auth.join("credential.json"), r#"{"type":"claude"}"#).unwrap();
        format!(
            "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\n",
            auth.display()
        )
    });
    let auth = f.dir.join("auth");
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    let nested = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n));
    let deep = ["q"; 128].join(".");
    let deep_error = format!("invalid field {deep}");
    for (fields, want) in [
        (format!(r#""x":{}"#, nested(126)), None),
        (format!(r#""x":{}"#, nested(127)), Some("invalid field x")),
        (format!(r#""a.b":{}"#, nested(125)), None),
        (format!(r#""a.b":{}"#, nested(126)), Some("invalid field a.b")),
        // A dotted path's objects count too, around a scalar as around a container.
        (format!(r#""{}":1"#, ["p"; 127].join(".")), None),
        (format!(r#""{deep}":1"#), Some(deep_error.as_str())),
        (r#""note":1e400"#.to_owned(), Some("invalid field note")),
        (r#""note":1e308"#.to_owned(), None),
    ] {
        let before = std::fs::read(auth.join("credential.json")).unwrap();
        let r = client
            .patch(format!("{base}/v8/management/credentials/fields"))
            .bearer_auth("fake-management-only")
            .body(format!(r#"{{"name":"credential.json",{fields}}}"#))
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        let body: Value = r.json().await.unwrap();
        let label = &fields[..fields.len().min(12)];
        match want {
            Some(message) => {
                assert_eq!((status, body), (400, json!({ "error": message })), "{label}");
                assert_eq!(std::fs::read(auth.join("credential.json")).unwrap(), before, "{label}");
            }
            None => {
                assert_eq!((status, body), (200, json!({ "status": "ok" })), "{label}");
                assert!(f.rt.store().get("credential.json").is_some(), "{label}: reloads");
            }
        }
    }
    server.abort();
}

/// Go's `PersistCodexAuthFile` keeps the disk's `siwc` subtree, and Go saves it at any
/// depth its decoder reads. A subtree added behind the watcher's back deeper than the
/// auth loader reads (127 levels) would make the saved file unreadable here, so the save
/// is refused (DIFFERENCES-FROM-GO.md, "Auth file saves") and nothing changes.
#[tokio::test]
async fn a_codex_save_refuses_a_disk_subtree_its_loader_cannot_read() {
    let f = Fixture::from_yaml("codex-deep", |auth, hash| {
        std::fs::write(auth.join("codex.json"), r#"{"type":"codex","access_token":"fake-a"}"#).unwrap();
        format!(
            "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\n",
            auth.display()
        )
    });
    let file = f.dir.join("auth").join("codex.json");
    let nested = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n));
    let before = f.rt.store().get("codex.json").unwrap();
    let (base, server) = f.server().await;
    // As edited, and already in the bytes the save would write (an equal save, which
    // writes nothing but would still publish what the bytes synthesize).
    let edited = format!(
        r#"{{"type":"codex","access_token":"fake-a","siwc":{{"x":{}}}}}"#,
        nested(200)
    );
    let canonical = format!(
        r#"{{"access_token":"fake-a","disabled":false,"siwc":{{"x":{}}},"type":"codex"}}"#,
        nested(200)
    );
    for (disk, disabled) in [(&edited, true), (&canonical, false)] {
        std::fs::write(&file, disk).unwrap();
        let r = wreq::Client::new()
            .patch(format!("{base}/v8/management/credentials/status"))
            .bearer_auth("fake-management-only")
            .body(format!(r#"{{"name":"codex.json","disabled":{disabled}}}"#))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 500, "disabled {disabled}");
        assert_eq!(
            r.json::<Value>().await.unwrap(),
            json!({"error": "failed to update auth: Io(\"the saved auth file could not be read back\")"})
        );
        assert_eq!(&std::fs::read_to_string(&file).unwrap(), disk);
        let after = f.rt.store().get("codex.json").unwrap();
        assert_eq!((after.revision, after.disabled), (before.revision, false));
    }
    server.abort();
}

#[tokio::test]
async fn credential_models_come_from_registrations() {
    let f = Fixture::from_yaml("models", |auth, hash| {
        format!(
            "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\napi-keys:\n  claude:\n    - models: [{{name: claude-sonnet-4-6, alias: sonnet}}]\n      keys:\n        - api-key: fake-k\n",
            auth.display()
        )
    });
    let id = f.rt.store().snapshot()[0].id.clone();
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    let models = |client: wreq::Client, base: String, id: String| async move {
        let v: Value = client
            .get(format!("{base}/v8/management/credentials/models?name={id}"))
            .bearer_auth("fake-management-only")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        v["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(models(client.clone(), base.clone(), id.clone()).await, ["sonnet"]);
    let r = client
        .patch(format!("{base}/v8/management/credentials/status"))
        .bearer_auth("fake-management-only")
        .json(&json!({"name": id, "disabled": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(models(client, base, id).await.is_empty());
    server.abort();
}

/// Go v8.0.35 `GetStaticModelDefinitionsByChannel` renders the live static catalog with
/// the Codex and xAI built-ins. Every channel answers Go's bytes, recorded by
/// `reference/manage` (its second output), with the GPT Image 2.6 forward port in the
/// Codex built-ins (docs/DIFFERENCES-FROM-GO.md).
#[tokio::test]
async fn model_definitions_match_go_by_channel() {
    let mut go: serde_json::Map<String, Value> =
        serde_json::from_str(include_str!("fixtures/model_definitions_go.json")).unwrap();
    forward_port::gpt_image_26::definitions_list(go.get_mut("codex").expect("codex channel"));
    forward_port::grok_video_lite::definitions_list(go.get_mut("xai").expect("xai channel"));
    for (channel, models) in go.iter_mut() {
        forward_port::k1_catalog::definitions_list(channel, models);
    }
    // The removed providers' channels (issue #49) are unknown here.
    let removed: Vec<String> = go
        .keys()
        .filter(|c| !matches!(c.as_str(), "claude" | "codex" | "xai" | "meta"))
        .cloned()
        .collect();
    go.retain(|c, _| !removed.contains(c));
    let f = Fixture::from_yaml("definitions-go", |auth, hash| {
        format!(
            "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\n",
            auth.display()
        )
    });
    let (base, server) = f.server().await;
    let get = |channel: String| {
        wreq::Client::new()
            .get(format!("{base}/v8/management/routing/model-definitions/{channel}"))
            .bearer_auth("fake-management-only")
            .send()
    };
    // Aliases answer their canonical section under the requested (lowercased) name.
    let aliases = [("x-ai", "xai"), ("GROK", "xai"), ("muse", "meta")];
    let requests = go.keys().map(|c| (c.as_str(), c.as_str())).chain(aliases);
    let mut compared = 0;
    for (channel, section) in requests {
        let r = get(channel.to_owned()).await.unwrap();
        assert_eq!(r.status(), 200, "{channel}");
        let want = format!(
            r#"{{"channel":{},"models":{}}}"#,
            Value::from(channel.go_lower()),
            serde_json::to_string(&go[section]).unwrap()
        );
        assert_eq!(r.text().await.unwrap(), want, "{channel}");
        compared += 1;
    }
    assert_eq!(compared, 7);
    for channel in removed {
        let r = get(channel.clone()).await.unwrap();
        assert_eq!(r.status(), 400, "{channel}");
    }
    server.abort();
}

/// Go v8.0.35 `injectV8APIKeyAuthIndexesLocked` (`0fb50a18`): JSON reads of
/// `api-keys` carry each key's `auth_index` (fixture recorded from Go), and writes drop
/// echoed `auth_index`/`auth-index` fields instead of saving them.
#[tokio::test]
async fn v8_api_keys_carry_auth_indexes_and_writes_drop_them() {
    let mut fixture: Value = serde_json::from_str(include_str!("fixtures/config_v8_auth_index_go.json")).unwrap();
    // The removed providers' groups go (issue #49).
    for family in ["gemini", "interactions", "vertex", "openai-compatibility"] {
        fixture["response"].as_object_mut().unwrap().remove(family);
    }
    let config = cpa_core::removed::strip_settings(fixture["config"].as_str().unwrap());
    let f = Fixture::from_yaml("authindex", |auth, hash| {
        config.replace(
            "config-version: 8\n",
            &format!(
                "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\n",
                auth.display()
            ),
        )
    });
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    let send = |method, at: &str, body: String| {
        client
            .request(method, format!("{base}/v8/management/config{at}"))
            .bearer_auth("fake-management-only")
            .body(body)
            .send()
    };
    let read = send(wreq::Method::GET, "/api-keys", String::new()).await.unwrap();
    let read: Value = read.json().await.unwrap();
    assert_eq!(read, fixture["response"]);
    // `TestConfigV8APIKeysExposeAuthIndex_Issue6287`: a provider sub-path and the root
    // carry the same indexes; the YAML document carries none.
    let codex: Value = send(wreq::Method::GET, "/api-keys/codex", String::new())
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(codex, fixture["response"]["codex"]);
    let root: Value = send(wreq::Method::GET, "", String::new())
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(root["api-keys"], fixture["response"]);
    let yaml = client
        .get(format!("{base}/v8/management/config.yaml"))
        .bearer_auth("fake-management-only")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(yaml.contains("fake-c2") && !yaml.contains("auth-index"), "{yaml}");

    // Echo the read back, at the provider list and at the root.
    let codex = read["codex"].clone();
    let r = send(wreq::Method::PUT, "/api-keys/codex", codex.to_string())
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = send(wreq::Method::PATCH, "", json!({"api-keys": read}).to_string())
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let file = f.file();
    assert!(!file.contains("auth_index") && !file.contains("auth-index"), "{file}");
    let again: Value = send(wreq::Method::GET, "/api-keys", String::new())
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(again, fixture["response"]);
    server.abort();
}

/// A relative auth-dir: an unclaimed upload is one fallback credential with a
/// relative ID, and once a field patch makes it claimable it is exactly one
/// synthesized credential (no leftover absolute-path fallback).
#[tokio::test]
async fn fallback_with_relative_auth_dir_is_retired_once_synthesized() {
    let f = Fixture::from_yaml("relfallback", |auth, hash| {
        let cwd = std::env::current_dir().unwrap();
        let up = "../".repeat(cwd.components().count() - 1);
        let relative = std::path::Path::new(&up).join(auth.strip_prefix("/").unwrap());
        format!(
            "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\n",
            relative.display()
        )
    });
    assert!(f.rt.config().auth_dir.is_relative());
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    let call = |method: wreq::Method, path: &str, body: &str| {
        client
            .request(method, format!("{base}/v8/management{path}"))
            .bearer_auth("fake-management-only")
            .body(body.to_owned())
            .send()
    };
    let ids = |f: &Fixture| {
        f.rt.store()
            .snapshot()
            .iter()
            .map(|c| (c.id.clone(), c.provider.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        call(wreq::Method::POST, "/credentials?name=a.json", "{}")
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(ids(&f), [("a.json".to_owned(), "unknown".to_owned())]);
    let r = call(
        wreq::Method::PATCH,
        "/credentials/fields",
        r#"{"name":"a.json","type":"claude"}"#,
    )
    .await
    .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(ids(&f), [("a.json".to_owned(), "claude".to_owned())]);
    server.abort();
}

/// Go v8.0.35 stale-reload guard (`1631875b`): a watcher reload that read the file
/// before a management save never publishes the older value after it. Here reloads
/// read and publish under the same lock as saves, so every save stays visible and Go's
/// paused-reload interleavings cannot occur
/// (`TestSetConfigIgnoresConfigLoadedBeforeManagementSave_Issue6631`,
/// `TestManagementWritesSurviveConcurrentReloadPublications_Issue6631`,
/// `TestManagementSequentialWritesSurviveStaleReload_Issue6631`,
/// `TestManagementWriteDuringReloadReachesRuntime_Issue6631`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_reloads_never_publish_a_value_older_than_a_save() {
    let f = Fixture::new("reloadfence");
    let (base, server) = f.server().await;
    let state = f.state.clone();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reloader = std::thread::spawn({
        let stop = stop.clone();
        move || {
            let mut reloads = 0;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                watching::reload(&state).unwrap();
                reloads += 1;
            }
            reloads
        }
    });
    let client = wreq::Client::new();
    let retry = || f.rt.config().document["routing"]["retry"]["request-retry"].clone();
    for i in 0..30 {
        let r = client
            .put(format!("{base}/v8/management/config/routing/retry/request-retry"))
            .bearer_auth("fake-management-only")
            .body(i.to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        for _ in 0..5 {
            assert_eq!(retry().as_i64(), Some(i), "save {i} was replaced by an older reload");
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(reloader.join().unwrap() > 30, "the reloads raced the saves");
    server.abort();
}

/// An auth-file field patch invalidates the cached registry: a new prefix is routable
/// at once.
#[tokio::test]
async fn file_credential_prefix_patch_refreshes_the_registry() {
    let f = Fixture::from_yaml("fileprefix", |auth, hash| {
        std::fs::write(auth.join("claude.json"), r#"{"type":"claude"}"#).unwrap();
        format!(
            "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\n",
            auth.display()
        )
    });
    assert!(
        !f.rt.registry().ids().any(|m| m.starts_with("new/")),
        "warm cache without the prefix"
    );
    let (base, server) = f.server().await;
    let r = wreq::Client::new()
        .patch(format!("{base}/v8/management/credentials/fields"))
        .bearer_auth("fake-management-only")
        .json(&json!({"name": "claude.json", "prefix": "new"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(f.rt.registry().ids().any(|m| m.starts_with("new/")));
    server.abort();
}

/// Update checks use exactly one credential-free HEAD, never follow redirects,
/// cache success, reject unrelated locations and honor the kill switch.
#[tokio::test]
async fn latest_version_is_anonymous_cached_and_explicit() {
    use axum::http::{HeaderMap, Method, StatusCode};
    use std::sync::Mutex;
    let seen: Arc<Mutex<Vec<(Method, HeaderMap)>>> = Arc::default();
    let record = seen.clone();
    let release = axum::Router::new().route(
        "/{case}",
        axum::routing::any(
            move |axum::extract::Path(case): axum::extract::Path<String>, method: Method, headers: HeaderMap| {
                let record = record.clone();
                async move {
                    record.lock().unwrap().push((method, headers));
                    let location = match case.as_str() {
                        "tag" => "https://github.com/vayungodara/cliproxy-rs/releases/tag/v1.2.3",
                        "other" => "https://example.invalid/releases/tag/v9",
                        "empty" => "https://github.com/vayungodara/cliproxy-rs/releases/tag/",
                        _ => "",
                    };
                    (
                        if case == "limited" {
                            StatusCode::FORBIDDEN
                        } else {
                            StatusCode::FOUND
                        },
                        [("location", location)],
                    )
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let release_server = tokio::spawn(async move { axum::serve(listener, release).await.unwrap() });

    let get = |case: &'static str, disabled: bool| {
        let url = format!("{upstream}/{case}");
        async move {
            let f = Fixture::new(&format!("latest-{}", case));
            let options = management::Options {
                latest_release_url: Some(url),
                update_check_disabled: Some(disabled),
                ..Default::default()
            };
            let state = Management::with_options(f.rt.clone(), f.dir.join("config.yaml"), options);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let app = management::router(state);
            let server = tokio::spawn(async move {
                axum::serve(
                    listener,
                    app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
                )
                .await
                .unwrap()
            });
            let mut out = (0, Value::Null);
            for _ in 0..2 {
                let r = wreq::Client::new()
                    .get(format!("{base}/v8/management/server/latest-version"))
                    .bearer_auth("fake-management-only")
                    .header("Cookie", "secret-cookie=FAKE")
                    .send()
                    .await
                    .unwrap();
                out = (r.status().as_u16(), r.json::<Value>().await.unwrap());
            }
            server.abort();
            out
        }
    };
    assert_eq!(
        get("tag", true).await,
        (
            503,
            json!({"error": "update_check_disabled", "message": "Update checks are disabled by CLIPROXY_NO_UPDATE_CHECK=1."})
        )
    );
    assert!(seen.lock().unwrap().is_empty(), "nothing is asked when disabled");
    assert_eq!(get("tag", false).await, (200, json!({"latest-version": "v1.2.3"})));
    assert_eq!(seen.lock().unwrap().len(), 1, "second click uses the cache");
    assert_eq!(
        get("empty", false).await,
        (
            502,
            json!({"error": "invalid_response", "message": "missing release version"})
        )
    );
    let (status, body) = get("other", false).await;
    assert_eq!((status, body["error"].as_str()), (502, Some("invalid_response")));
    assert_eq!(
        get("limited", false).await,
        (502, json!({"error": "unexpected_status", "message": "status 403"}))
    );
    assert!(seen.lock().unwrap().iter().all(|(method, h)| *method == Method::HEAD
        && h["user-agent"] == "cliproxy-rs"
        && !h.contains_key("authorization")
        && !h.contains_key("proxy-authorization")
        && !h.contains_key("cookie")));
    release_server.abort();
}

/// Go v8.0.35 `TestGitHubTokenConfigRoundTrip` (7c41cd05): `server.github-token` never
/// reaches the v0 config JSON (`json:"-"`), whichever spelling holds it. The v8 reads
/// and writes are the `server_github_token` scenario in `manage_go.json`.
#[tokio::test]
async fn github_token_stays_out_of_the_v0_config_json() {
    for (name, layout) in [
        (
            "ghv8",
            "config-version: 8\nserver:\n  github-token: fake-github-secret\n",
        ),
        ("ghlegacy", "github-token: fake-github-secret\n"),
    ] {
        let f = Fixture::from_yaml(name, |auth, hash| {
            private_config(layout, auth, hash, !layout.starts_with("config-version"))
        });
        let (base, server) = f.server().await;
        let (status, body) = call(&base, wreq::Method::GET, "/v0/management/config", "").await;
        assert_eq!(status, 200, "{name}: {body}");
        assert!(
            !body.contains("fake-github-secret") && !body.contains("github-token"),
            "{name}: {body}"
        );
        let (_, v8) = call(
            &base,
            wreq::Method::GET,
            "/v8/management/config/server/github-token",
            "",
        )
        .await;
        assert_eq!(v8, r#""fake-github-secret""#, "{name}");
        server.abort();
    }
}

/// Go v8.0.35 `TestModelCatalogConfigRoundTrip` with `migrate` false: a v0 save keeps
/// the `models` sources, which load back unchanged. `models` is a v8 root that Go's
/// `IsV8ConfigLayout` does not share with the legacy layout, so the save writes v8. The
/// v8 saves (`migrate` true), clearing and the refusals are the `model_catalog_sources`
/// scenario in `manage_go.json`; the load refusals are its `load_errors`. Unix only:
/// `/abs/models.json` is not absolute on Windows.
#[tokio::test]
async fn model_catalog_sources_survive_a_legacy_save() {
    let raw = "models:\n  catalog: /abs/models.json\n  codex-catalog: 'https://codex.example.invalid/codex.json'\n";
    let f = Fixture::from_yaml("catalog-legacy", |auth, hash| private_config(raw, auth, hash, true));
    let (base, server) = f.server().await;
    let (status, text) = call(
        &base,
        wreq::Method::PUT,
        "/v0/management/request-retry",
        r#"{"value":2}"#,
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let file = f.file();
    assert!(file.contains("config-version: 8"), "{file}");
    let models = Config::parse(&file).unwrap().document["models"].clone();
    assert_eq!(
        serde_json::to_value(models).unwrap(),
        json!({"catalog": "/abs/models.json", "codex-catalog": "https://codex.example.invalid/codex.json"}),
        "{file}"
    );
    server.abort();
}

/// A v0 setter edits the file on disk and validates the result, so a file a reload
/// refused (here a relative catalog source) refuses an unrelated v0 write and stays as
/// it is, while the running config keeps the last valid source. Go marshals its valid
/// in-memory config over the file and saves (DIFFERENCES: v0 setters over a refused file).
/// Unix only: `/ok/models.json` is not absolute on Windows.
#[tokio::test]
async fn a_v0_write_over_a_file_with_an_invalid_catalog_source_is_refused() {
    let raw = "models:\n  catalog: /ok/models.json\n";
    let f = Fixture::from_yaml("catalog-v0-refused", |auth, hash| private_config(raw, auth, hash, true));
    let (base, server) = f.server().await;
    let disk = f.file().replace("/ok/models.json", "relative.json");
    std::fs::write(f.dir.join("config.yaml"), &disk).unwrap();
    let (status, text) = call(
        &base,
        wreq::Method::PUT,
        "/v0/management/request-retry",
        r#"{"value":2}"#,
    )
    .await;
    assert_eq!(
        (status, text.as_str()),
        (
            422,
            r#"{"error":"invalid_config","message":"parse config payload: models.catalog must be an http(s) URL or an absolute local path"}"#
        )
    );
    assert_eq!(f.file(), disk);
    assert_eq!(f.rt.config().document["models"]["catalog"], "/ok/models.json");
    server.abort();
}

/// Separate processes give the environment fallback a fresh proxy snapshot without
/// changing the environment of concurrently running tests. Every address is local;
/// the unresolvable release host forces the check to use the configured transport.
#[tokio::test]
async fn latest_version_uses_configured_and_environment_proxies() {
    use axum::http::{HeaderMap, Method, Uri};
    use std::sync::Mutex;
    if let Ok(case) = std::env::var("CLIPROXY_TEST_RELEASE_PROXY") {
        let configured = std::env::var("CLIPROXY_TEST_PROXY_URL").unwrap_or_default();
        let f = Fixture::from_yaml(&format!("release-proxy-{case}"), |auth, hash| {
            format!(
                "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\nrequests:\n  proxy-url: '{configured}'\n",
                auth.display()
            )
        });
        let state = Management::with_options(
            f.rt.clone(),
            f.dir.join("config.yaml"),
            management::Options {
                latest_release_url: Some("http://release.example.invalid/latest".into()),
                update_check_disabled: Some(false),
                ..Default::default()
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                management::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap()
        });
        let client = wreq::Client::builder().no_proxy().build().unwrap();
        for _ in 0..2 {
            let response = client
                .get(format!("{base}/v8/management/server/latest-version"))
                .bearer_auth("fake-management-only")
                .header("Cookie", "FAKE-browser-cookie")
                .send()
                .await
                .unwrap();
            if case == "direct" {
                assert_eq!(
                    response.status(),
                    502,
                    "direct must bypass the proxy for an unresolvable origin"
                );
                continue;
            }
            assert_eq!(response.status(), 200);
            assert_eq!(
                response.json::<Value>().await.unwrap(),
                json!({"latest-version": "v4.5.6"})
            );
        }
        server.abort();
        return;
    }
    for case in ["configured", "environment", "direct"] {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        let proxy = axum::Router::new().fallback(move |method: Method, uri: Uri, headers: HeaderMap| {
            record.lock().unwrap().push((method, uri, headers));
            async {
                (
                    axum::http::StatusCode::FOUND,
                    [(
                        "location",
                        "https://github.com/vayungodara/cliproxy-rs/releases/tag/v4.5.6",
                    )],
                )
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_url = format!("http://proxy-user:proxy-password@{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, proxy).await.unwrap() });
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "latest_version_uses_configured_and_environment_proxies",
                "--nocapture",
            ])
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("SYSTEMROOT", std::env::var("SYSTEMROOT").unwrap_or_default())
            .env("HOME", std::env::temp_dir())
            .env("CLIPROXY_TEST_RELEASE_PROXY", case)
            .env(
                "CLIPROXY_TEST_PROXY_URL",
                match case {
                    "configured" => &proxy_url,
                    "direct" => "direct",
                    _ => "",
                },
            )
            .env(
                "HTTP_PROXY",
                if case != "configured" {
                    &proxy_url
                } else {
                    "http://127.0.0.1:9"
                },
            )
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("GITHUB_TOKEN", "FAKE-github-token")
            .env("GITSTORE_GIT_TOKEN", "FAKE-store-token")
            .env("OPENAI_API_KEY", "FAKE-provider-token")
            .kill_on_drop(true);
        let output = tokio::time::timeout(std::time::Duration::from_secs(30), command.output())
            .await
            .unwrap()
            .unwrap();
        server.abort();
        assert!(
            output.status.success(),
            "{case}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let seen = seen.lock().unwrap();
        if case == "direct" {
            assert!(seen.is_empty(), "direct must not reach the environment proxy");
            continue;
        }
        assert_eq!(
            seen.len(),
            1,
            "two clicks share one HEAD without following the redirect"
        );
        let (method, uri, headers) = &seen[0];
        assert_eq!(*method, Method::HEAD);
        assert_eq!(uri.to_string(), "http://release.example.invalid/latest");
        assert_eq!(
            headers["proxy-authorization"],
            "Basic cHJveHktdXNlcjpwcm94eS1wYXNzd29yZA=="
        );
        assert!(!headers.contains_key("authorization"));
        assert!(!headers.contains_key("cookie"));
        assert_eq!(headers["user-agent"], "cliproxy-rs");
    }
}

/// A valid but self-signed origin certificate must still fail behind an authenticated
/// CONNECT proxy. Disabling certificate checks would turn this response into a 200.
#[tokio::test]
async fn latest_version_keeps_tls_verification_through_proxy() {
    use btls::asn1::Asn1Time;
    use btls::bn::BigNum;
    use btls::ec::{EcGroup, EcKey};
    use btls::hash::MessageDigest;
    use btls::nid::Nid;
    use btls::pkey::PKey;
    use btls::ssl::{SslAcceptor, SslMethod};
    use btls::x509::extension::SubjectAlternativeName;
    use btls::x509::{X509, X509NameBuilder};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let key =
        PKey::from_ec_key(EcKey::generate(&EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap()).unwrap()).unwrap();
    let mut name = X509NameBuilder::new().unwrap();
    name.append_entry_by_nid(Nid::COMMONNAME, "127.0.0.1").unwrap();
    let name = name.build();
    let mut cert = X509::builder().unwrap();
    cert.set_version(2).unwrap();
    cert.set_serial_number(&BigNum::from_u32(1).unwrap().to_asn1_integer().unwrap())
        .unwrap();
    cert.set_subject_name(&name).unwrap();
    cert.set_issuer_name(&name).unwrap();
    cert.set_pubkey(&key).unwrap();
    cert.set_not_before(&Asn1Time::days_from_now(0).unwrap()).unwrap();
    cert.set_not_after(&Asn1Time::days_from_now(1).unwrap()).unwrap();
    cert.append_extension(
        &SubjectAlternativeName::new()
            .ip("127.0.0.1")
            .build(&cert.x509v3_context(None, None))
            .unwrap(),
    )
    .unwrap();
    cert.sign(&key, MessageDigest::sha256()).unwrap();
    let mut tls = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
    tls.set_certificate(&cert.build()).unwrap();
    tls.set_private_key(&key).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = listener.local_addr().unwrap();
    let tls_server = tokio::spawn(cpa_server::listener::serve(
        listener,
        axum::Router::new().fallback(|| async {
            (
                axum::http::StatusCode::FOUND,
                [(
                    "location",
                    "https://github.com/vayungodara/cliproxy-rs/releases/tag/v4.5.6",
                )],
            )
        }),
        Some(Arc::new(tls.build())),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            assert!(head.len() < 8192);
            head.push(socket.read_u8().await.unwrap());
        }
        socket
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .unwrap();
        let mut upstream = tokio::net::TcpStream::connect(origin).await.unwrap();
        let _ = tokio::io::copy_bidirectional(&mut socket, &mut upstream).await;
        String::from_utf8(head).unwrap()
    });
    let f = Fixture::from_yaml("release-proxy-tls", |auth, hash| {
        format!(
            "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\nrequests:\n  proxy-url: 'http://proxy-user:proxy-password@{proxy}'\n",
            auth.display()
        )
    });
    let state = Management::with_options(
        f.rt.clone(),
        f.dir.join("config.yaml"),
        management::Options {
            latest_release_url: Some(format!("https://{origin}/latest")),
            update_check_disabled: Some(false),
            ..Default::default()
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            management::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    let response = wreq::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("http://{base}/v8/management/server/latest-version"))
        .bearer_auth("fake-management-only")
        .header("Cookie", "FAKE-browser-cookie")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 502);
    assert_eq!(response.json::<Value>().await.unwrap()["error"], "request_failed");
    let head = tokio::time::timeout(std::time::Duration::from_secs(2), proxy_server)
        .await
        .unwrap()
        .unwrap()
        .to_ascii_lowercase();
    assert!(head.starts_with(&format!("connect {origin} http/1.1\r\n")));
    assert!(
        head.contains(&"\r\nProxy-Authorization: Basic cHJveHktdXNlcjpwcm94eS1wYXNzd29yZA==\r\n".to_ascii_lowercase())
    );
    assert!(!head.contains("\r\nauthorization:") && !head.contains("\r\ncookie:"));
    server.abort();
    tls_server.abort();
}

/// AI Studio relay credentials: disabling one through the status or fields endpoint
/// clears its cooldowns (Go `Manager.Update`), so re-enabling routes to it at once.
#[tokio::test]
async fn runtime_credential_disable_clears_its_cooldowns() {
    use cpa_core::credential::Credential;
    use cpa_core::exec::{ExecError, FailureScope};
    use cpa_server::runtime::{Outcome, Selection};
    let f = Fixture::new("runtime-cooldown");
    let id = "aistudio-0123456789abcdef";
    assert!(f.rt.store().add_runtime(Credential::relay_session(id)));
    let selection = || Selection {
        provider: "aistudio".into(),
        ..Selection::default()
    };
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    for endpoint in ["status", "fields"] {
        let lease = f.rt.store().select(selection()).unwrap();
        assert_eq!(lease.credential.id, id);
        let mut quota = ExecError::local(429, FailureScope::Credential, "rate limited");
        quota.retry_after = Some(std::time::Duration::from_secs(600));
        lease.complete(Outcome::Failure(quota));
        assert!(!f.rt.store().cooldowns(id).is_empty(), "{endpoint}: cooling");
        assert!(f.rt.store().select(selection()).is_none(), "{endpoint}: blocked");
        for disabled in [true, false] {
            let r = client
                .patch(format!("{base}/v8/management/credentials/{endpoint}"))
                .bearer_auth("fake-management-only")
                .json(&json!({"name": id, "disabled": disabled}))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 200, "{endpoint} disabled={disabled}");
            assert_eq!(f.rt.store().get(id).unwrap().disabled, disabled);
            assert!(f.rt.store().cooldowns(id).is_empty(), "{endpoint} disabled={disabled}");
        }
        let lease = f.rt.store().select(selection());
        assert_eq!(
            lease.map(|l| l.credential.id.clone()).as_deref(),
            Some(id),
            "{endpoint}"
        );
    }
    server.abort();
}

/// Deleting an AI Studio relay credential (no file) answers 404 and never removes a
/// file in `auth-dir` that shares its ID.
#[tokio::test]
async fn runtime_credential_delete_spares_same_named_file() {
    let f = Fixture::new("runtime-delete");
    let id = "aistudio-0123456789abcdef";
    assert!(
        f.rt.store()
            .add_runtime(cpa_core::credential::Credential::relay_session(id))
    );
    let sentinel = f.dir.join("auth").join(id);
    std::fs::write(&sentinel, "keep").unwrap();
    let (base, server) = f.server().await;
    let r = wreq::Client::new()
        .delete(format!("{base}/v8/management/credentials?name={id}"))
        .bearer_auth("fake-management-only")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    assert_eq!(r.json::<Value>().await.unwrap()["error"], "auth file not found");
    assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "keep");
    assert!(f.rt.store().get(id).is_some());
    server.abort();
}

/// `GET /observability/usage/queue` drains queued records oldest first; a record that
/// is not JSON comes back as a string. Disabling statistics stops queueing.
#[tokio::test]
async fn usage_queue_pops_records_in_order() {
    let f = Fixture::from_yaml("usageq", |auth, hash| {
        format!(
            "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\nobservability:\n  usage:\n    usage-statistics-enabled: true\n",
            auth.display()
        )
    });
    let (base, server) = f.server().await;
    let queue = f.rt.usage_queue();
    queue.enqueue(br#"{"model":"a","tokens":{"total_tokens":3}}"#.to_vec());
    queue.enqueue(b"not json".to_vec());
    queue.enqueue(br#"{"model":"c"}"#.to_vec());
    let client = wreq::Client::new();
    let pop = |q: &str| {
        client
            .get(format!("{base}/v8/management/observability/usage/queue{q}"))
            .bearer_auth("fake-management-only")
            .send()
    };
    let first: Value = pop("").await.unwrap().json().await.unwrap();
    assert_eq!(first, json!([{"model": "a", "tokens": {"total_tokens": 3}}]));
    let rest: Value = pop("?count=10").await.unwrap().json().await.unwrap();
    assert_eq!(rest, json!(["not json", {"model": "c"}]));
    let r = client
        .put(format!(
            "{base}/v8/management/config/observability/usage/usage-statistics-enabled"
        ))
        .bearer_auth("fake-management-only")
        .body("false")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(!queue.accepts());
    queue.enqueue(br#"{"model":"d"}"#.to_vec());
    let none: Value = pop("?count=10").await.unwrap().json().await.unwrap();
    assert_eq!(none, json!([]));
    server.abort();
}

/// The dashboard's `GET /observability/usage/live` over the served app, with records
/// produced by real `/v1/messages` requests to a local mock Claude upstream: it needs
/// the management key, two readers see the same records without competing, and the
/// queue collector (v8 and v0 spellings) still gets every record it gets with no reader.
#[tokio::test]
async fn usage_live_observes_without_taking_records_from_the_queue() {
    let upstream = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(|| async {
            axum::Json(json!({
                "id": "msg_fake", "type": "message", "role": "assistant", "model": "claude-fake",
                "content": [{"type": "text", "text": "ok"}], "stop_reason": "end_turn",
                "usage": {"input_tokens": 3, "output_tokens": 1}
            }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let f = Fixture::from_yaml("usagelive", |auth, hash| {
        format!(
            "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\naccess:\n  api-keys: [fake-client]\nobservability:\n  usage:\n    usage-statistics-enabled: true\nclaude-api-key:\n  - api-key: sk-fake-upstream\n    base-url: {upstream_url}\n",
            auth.display()
        )
    });
    cpa_server::install_registry(&f.rt);
    // As the binary composes it, minus layers this test does not exercise.
    let app = cpa_server::observability::router(
        &f.rt,
        cpa_server::app(f.rt.clone(), management::router(f.state.clone())),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let client = wreq::Client::new();
    let get = |path: String, key: Option<&'static str>| {
        let mut r = client.get(format!("{base}/{path}"));
        if let Some(key) = key {
            r = r.bearer_auth(key);
        }
        r.send()
    };
    let live = |after: Option<String>| {
        let q = after.map_or(String::new(), |a| format!("?after={a}"));
        get(
            format!("v8/management/observability/usage/live{q}"),
            Some("fake-management-only"),
        )
    };
    let request = || {
        client
            .post(format!("{base}/v1/messages"))
            .header("x-api-key", "fake-client")
            .json(
                &json!({"model": "claude-opus-5-5", "max_tokens": 8, "messages": [{"role": "user", "content": "hi"}]}),
            )
            .send()
    };
    let cursor = |v: &Value| v["cursor"].as_str().unwrap().to_owned();
    let ids = |records: &Value| -> Vec<String> {
        records
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["request_id"].as_str().unwrap().to_owned())
            .collect()
    };

    // Management auth: no key, a client key and a wrong key are all refused, and no
    // refused read counts as a viewer.
    for key in [None, Some("fake-client"), Some("wrong")] {
        let r = get("v8/management/observability/usage/live".into(), key).await.unwrap();
        assert_eq!(r.status(), 401, "key {key:?}");
    }
    for q in ["?after=x", "?after=1", "?after=-1", "?count=0", "?count=x"] {
        let r = get(
            format!("v8/management/observability/usage/live{q}"),
            Some("fake-management-only"),
        )
        .await
        .unwrap();
        assert_eq!(r.status(), 400, "{q}");
    }

    // Baseline with no viewer: the collector gets both records.
    for _ in 0..2 {
        let r = request().await.unwrap();
        assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    }
    let baseline: Value = get(
        "v8/management/observability/usage/queue?count=10".into(),
        Some("fake-management-only"),
    )
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(baseline.as_array().unwrap().len(), 2);

    // Two dashboard tabs start, then three requests arrive.
    let tab_a: Value = live(None).await.unwrap().json().await.unwrap();
    let tab_b: Value = live(None).await.unwrap().json().await.unwrap();
    assert_eq!(tab_a["records"], json!([]), "a new viewer starts at the newest record");
    assert_eq!(tab_a["usage_statistics_enabled"], true);
    for _ in 0..3 {
        assert_eq!(request().await.unwrap().status(), 200);
    }
    let a: Value = live(Some(cursor(&tab_a))).await.unwrap().json().await.unwrap();
    let b: Value = live(Some(cursor(&tab_b))).await.unwrap().json().await.unwrap();
    let seen = ids(&a["records"]);
    assert_eq!(seen.len(), 3);
    assert!(seen.iter().all(|id| !id.is_empty()) && seen[0] != seen[1] && seen[1] != seen[2]);
    assert_eq!(a["records"], b["records"], "both tabs see the same records");
    assert_eq!((a["gap"].as_bool(), a["more"].as_bool()), (Some(false), Some(false)));
    let again: Value = live(Some(cursor(&tab_a))).await.unwrap().json().await.unwrap();
    assert_eq!(again["records"], a["records"], "re-reading takes nothing");
    let empty: Value = live(Some(cursor(&a))).await.unwrap().json().await.unwrap();
    assert_eq!(empty["records"], json!([]));
    // A cursor from another process (another epoch) with an in-range number: a gap,
    // and every kept record, instead of silently skipping some.
    let (epoch, seq) = cursor(&a)
        .split_once('.')
        .map(|(e, s)| (e.to_owned(), s.parse::<u64>().unwrap()))
        .unwrap();
    let foreign_epoch = if epoch == "0000000000000000" {
        "0000000000000001"
    } else {
        "0000000000000000"
    };
    let foreign: Value = live(Some(format!("{foreign_epoch}.{}", seq - 2)))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(foreign["gap"], true);
    assert_eq!(foreign["records"], a["records"]);

    // The collector still gets every record, split across both queue spellings.
    let first: Value = get(
        "v8/management/observability/usage/queue?count=2".into(),
        Some("fake-management-only"),
    )
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let rest: Value = get(
        "v0/management/usage-queue?count=10".into(),
        Some("fake-management-only"),
    )
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let mut collected = ids(&first);
    collected.extend(ids(&rest));
    assert_eq!(collected, ids(&a["records"]), "same records, same order");
    // Records are byte-identical to the queue's (the schema is unchanged).
    assert_eq!(
        a["records"].as_array().unwrap().iter().collect::<Vec<_>>(),
        first
            .as_array()
            .unwrap()
            .iter()
            .chain(rest.as_array().unwrap())
            .collect::<Vec<_>>()
    );

    // A slow tab that never reads again: the feed stays capped and the queue intact.
    let slow = cursor(&a);
    let queue = f.rt.usage_queue();
    for i in 0..cpa_server::usage::LIVE_MAX_RECORDS + 10 {
        queue.enqueue(format!(r#"{{"request_id":"synthetic-{i}"}}"#).into_bytes());
    }
    let behind: Value = live(Some(slow)).await.unwrap().json().await.unwrap();
    assert_eq!(behind["gap"], true);
    assert_eq!(behind["more"], true);
    assert_eq!(
        behind["records"].as_array().unwrap().len(),
        cpa_server::usage::LIVE_MAX_PAGE
    );
    assert_eq!(
        behind["records"][0]["request_id"], "synthetic-10",
        "the oldest kept record"
    );
    let drained: Value = get(
        "v8/management/observability/usage/queue?count=5000".into(),
        Some("fake-management-only"),
    )
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(
        drained.as_array().unwrap().len(),
        cpa_server::usage::LIVE_MAX_RECORDS + 10,
        "the queue lost nothing to the slow viewer"
    );
    server.abort();
}

/// api-call HEAD: Go's transport never asks for gzip on HEAD, so the upstream sees no
/// Accept-Encoding and a gzip Content-Encoding on the answer is relayed untouched.
#[tokio::test]
async fn api_call_head_does_not_negotiate_gzip() {
    use axum::http::{HeaderMap, HeaderValue};
    let upstream = axum::Router::new().route(
        "/h",
        axum::routing::head(|headers: HeaderMap| async move {
            let ae = headers
                .get("accept-encoding")
                .cloned()
                .unwrap_or_else(|| HeaderValue::from_static("none"));
            (
                [("x-ae", ae), ("content-encoding", HeaderValue::from_static("gzip"))],
                "",
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/h", listener.local_addr().unwrap());
    let up = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let f = Fixture::new("apihead");
    let (base, server) = f.server().await;
    let r: Value = wreq::Client::new()
        .post(format!("{base}/v8/management/requests/api-call"))
        .bearer_auth("fake-management-only")
        .json(&json!({"method": "HEAD", "url": url}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["status_code"], 200);
    assert_eq!(r["header"]["X-Ae"], json!(["none"]));
    assert_eq!(r["header"]["Content-Encoding"], json!(["gzip"]));
    assert_eq!(r["body"], "");
    server.abort();
    up.abort();
}

/// Fake provider login endpoints (Claude, Codex, xAI) on one local server; records
/// the form or JSON bodies it receives.
async fn fake_logins() -> (
    String,
    Arc<std::sync::Mutex<Vec<(String, String)>>>,
    tokio::task::JoinHandle<()>,
) {
    use base64::Engine;
    let seen: Arc<std::sync::Mutex<Vec<(String, String)>>> = Arc::default();
    let record = seen.clone();
    let b64 = |v: &Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string());
    let id_token = format!(
        "{}.{}.sig",
        b64(&json!({"alg": "none"})),
        b64(&json!({"email": "c@example.invalid",
            "https://api.openai.com/auth": {"chatgpt_plan_type": "plus", "chatgpt_account_id": "acc-fake"}}))
    );
    let xai_id_token = format!(
        "{}.{}.sig",
        b64(&json!({"alg": "none"})),
        b64(&json!({"email": "x@example.invalid", "sub": "xai-user-1"}))
    );
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri, body: axum::body::Bytes| {
        let record = record.clone();
        let id_token = id_token.clone();
        let xai_id_token = xai_id_token.clone();
        async move {
            record
                .lock()
                .unwrap()
                .push((uri.path().to_owned(), String::from_utf8_lossy(&body).into_owned()));
            let v = match uri.path() {
                "/oauth/token" => json!({"id_token": id_token, "access_token": "fake-codex-at",
                    "refresh_token": "fake-codex-rt", "expires_in": 3600}),
                "/v1/oauth/token" => json!({"access_token": "sk-ant-oat-fake", "refresh_token": "fake-r",
                    "expires_in": 3600, "account": {"uuid": "acct-1", "email_address": "a@example.invalid"},
                    "organization": {"uuid": "org-1", "name": "Org"}}),
                "/api/oauth/profile" => json!({"account": {"uuid": "acct-1", "email": "a@example.invalid"},
                    "organization": {"uuid": "org-1", "name": "Org"}}),
                // xAI: discovery names auth.x.ai endpoints, which the test seam redirects here.
                "/.well-known/openid-configuration" => json!({
                    "device_authorization_endpoint": "https://auth.x.ai/oauth2/device/code",
                    "token_endpoint": "https://auth.x.ai/oauth2/token"}),
                "/oauth2/device/code" => json!({"device_code": "xdc", "user_code": "XU-1",
                    "verification_uri": "https://accounts.x.ai/device",
                    "verification_uri_complete": "https://accounts.x.ai/device?user_code=XU-1",
                    "expires_in": 600, "interval": 1}),
                "/oauth2/token" => json!({"access_token": "fake-xai-at", "refresh_token": "fake-xai-rt",
                    "id_token": xai_id_token, "token_type": "Bearer", "expires_in": 3600}),
                _ => json!({}),
            };
            axum::Json(v)
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, seen, handle)
}

/// Serves `state` like `Fixture::server` but with login endpoints redirected.
async fn login_server(f: &Fixture, login_base: &str) -> (String, Arc<Management>, tokio::task::JoinHandle<()>) {
    let options = management::Options {
        login_base: Some(login_base.to_owned()),
        ..Default::default()
    };
    serve_with(f, options).await
}

/// A management server over `f`'s runtime with `options`.
async fn serve_with(
    f: &Fixture,
    options: management::Options,
) -> (String, Arc<Management>, tokio::task::JoinHandle<()>) {
    let state = Management::with_options(f.rt.clone(), f.dir.join("config.yaml"), options);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = management::router(state.clone());
    let handle = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    (base, state, handle)
}

async fn oauth_get(base: &str, path: &str) -> Value {
    wreq::Client::new()
        .get(format!("{base}/v8/management{path}"))
        .bearer_auth("fake-management-only")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// A remote store that records every push and explicit delete; with `down`, every auth
/// push fails. An explicit delete removes the file, as the real stores do.
#[derive(Default)]
struct FakeStore {
    dir: PathBuf,
    down: bool,
    pushed: std::sync::Mutex<Vec<String>>,
}

impl cpa_server::persist::StorePersister for FakeStore {
    fn persist_config(&self) -> futures_util::future::BoxFuture<'_, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn persist_auth_files(
        &self,
        message: String,
        _: Vec<PathBuf>,
    ) -> futures_util::future::BoxFuture<'_, anyhow::Result<()>> {
        self.pushed.lock().unwrap().push(message);
        let down = self.down;
        Box::pin(async move {
            anyhow::ensure!(!down, "fake store unavailable");
            Ok(())
        })
    }
    fn delete_auth(&self, path: PathBuf) -> anyhow::Result<()> {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        self.pushed.lock().unwrap().push(format!("Delete auth {name}"));
        std::fs::remove_file(path)?;
        Ok(())
    }
    fn auth_dir(&self) -> PathBuf {
        self.dir.clone()
    }
}

/// Polls `/oauth/status` until it leaves `wait` (at most `secs`).
async fn final_status(base: &str, state: &str, secs: u64) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        let v = oauth_get(base, &format!("/oauth/status?state={state}")).await;
        if v["status"] != "wait" || std::time::Instant::now() > deadline {
            return v;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// xAI: Go `RequestXAIToken` answers with the device URL and code, and the poller
/// saves the credential through the file store and publishes it.
#[tokio::test]
async fn xai_login_polls_the_device_flow_and_saves_the_credential() {
    let (fake, seen, fake_server) = fake_logins().await;
    let f = Fixture::new("xailogin");
    let (base, _state, server) = login_server(&f, &fake).await;
    let started = oauth_get(&base, "/oauth/auth-url?provider=xai").await;
    let state = started["state"].as_str().unwrap().to_owned();
    assert!(state.starts_with("xai-"), "{state}");
    assert_eq!(started["status"], "ok");
    assert_eq!(started["flow"], "device");
    assert_eq!(started["url"], "https://accounts.x.ai/device?user_code=XU-1");
    assert_eq!(started["user_code"], "XU-1");
    assert_eq!(started["expires_in"], 600);
    assert_eq!(final_status(&base, &state, 10).await, json!({"status": "ok"}));
    let auth = f.dir.join("auth");
    let names: Vec<String> = std::fs::read_dir(&auth)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("xai-"))
        .collect();
    assert_eq!(names.len(), 1, "{names:?}");
    let path = auth.join(&names[0]);
    let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for (key, want) in [
        ("type", json!("xai")),
        ("access_token", json!("fake-xai-at")),
        ("refresh_token", json!("fake-xai-rt")),
        ("email", json!("x@example.invalid")),
        ("sub", json!("xai-user-1")),
        ("base_url", json!("https://api.x.ai/v1")),
        ("token_endpoint", json!("https://auth.x.ai/oauth2/token")),
        ("auth_kind", json!("oauth")),
        ("expires_in", json!(3600)),
        ("disabled", json!(false)),
    ] {
        assert_eq!(saved[key], want, "{key}");
    }
    let last_refresh = saved["last_refresh"].as_str().unwrap();
    assert!(chrono::DateTime::parse_from_rfc3339(last_refresh).is_ok() && last_refresh.ends_with('Z'));
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
    let forms = seen.lock().unwrap().clone();
    let device = forms.iter().find(|(p, _)| p == "/oauth2/device/code").unwrap();
    assert!(
        device.1.contains("client_id=b1a00492-073a-47ea-816f-4c329264a828"),
        "{}",
        device.1
    );
    let token = forms.iter().find(|(p, _)| p == "/oauth2/token").unwrap();
    assert!(token.1.contains("device_code=xdc"), "{}", token.1);
    assert!(f.rt.store().snapshot().iter().any(|c| c.provider == "xai"), "published");
    server.abort();
    fake_server.abort();
}

/// xAI (v8.0.67 d65840fb, issue #85): the login saves under the identity-hashed name,
/// keeps the operator settings of the account's v8.0.35 file, removes that file once
/// the new one is stored, and leaves another account's file at a colliding old name.
#[tokio::test]
async fn xai_login_migrates_the_accounts_v8_0_35_file() {
    let (fake, _seen, fake_server) = fake_logins().await;
    let f = Fixture::new("xaimigrate");
    let auth = f.dir.join("auth");
    let old = json!({"type": "xai", "email": "X@example.invalid", "sub": "xai-user-1", "access_token": "old",
        "proxy-url": "socks5://127.0.0.1:1080", "disabled": true, "priority": 7, "prefix": "team"});
    // v8.0.35 kept the email's case in the name.
    std::fs::write(auth.join("xai-X@example.invalid.json"), old.to_string()).unwrap();
    // Another account owns the name this account's lowercase email had in v8.0.35
    // (`+x@example.invalid` sanitizes to `x@example.invalid`).
    let other = json!({"type": "xai", "email": "+x@example.invalid", "access_token": "other"});
    std::fs::write(auth.join("xai-x@example.invalid.json"), other.to_string()).unwrap();
    let (base, _state, server) = login_server(&f, &fake).await;
    let started = oauth_get(&base, "/oauth/auth-url?provider=xai").await;
    let state = started["state"].as_str().unwrap().to_owned();
    assert_eq!(final_status(&base, &state, 10).await, json!({"status": "ok"}));
    let mut names: Vec<String> = std::fs::read_dir(&auth)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let hashed = cpa_exec::xai_auth::credential_file_name("x@example.invalid", "xai-user-1", 0);
    let mut want = vec!["xai-x@example.invalid.json".to_owned(), hashed.clone()];
    want.sort();
    assert_eq!(names, want);
    let saved: Value = serde_json::from_slice(&std::fs::read(auth.join(&hashed)).unwrap()).unwrap();
    for (key, want) in [
        ("access_token", json!("fake-xai-at")),
        ("email", json!("x@example.invalid")),
        ("proxy_url", json!("socks5://127.0.0.1:1080")),
        ("disabled", json!(true)),
        ("priority", json!(7)),
        ("prefix", json!("team")),
    ] {
        assert_eq!(saved[key], want, "{key}: {saved}");
    }
    let untouched: Value =
        serde_json::from_slice(&std::fs::read(auth.join("xai-x@example.invalid.json")).unwrap()).unwrap();
    assert_eq!(untouched, other);
    server.abort();
    fake_server.abort();
}

/// xAI (#85 review): a file at the login's hashed name that holds another account
/// (same email, another subject) is refused: the session fails with an actionable
/// message that names no file or account, and the file is untouched.
#[tokio::test]
async fn xai_login_refuses_another_accounts_file_at_its_name() {
    let (fake, _seen, fake_server) = fake_logins().await;
    let f = Fixture::new("xairefuse");
    let auth = f.dir.join("auth");
    let name = cpa_exec::xai_auth::credential_file_name("x@example.invalid", "xai-user-1", 0);
    let foreign = json!({"type": "xai", "email": "x@example.invalid", "sub": "someone-else",
        "access_token": "other", "proxy_url": "http://127.0.0.1:8087"})
    .to_string();
    std::fs::write(auth.join(&name), &foreign).unwrap();
    let (base, _state, server) = login_server(&f, &fake).await;
    let started = oauth_get(&base, "/oauth/auth-url?provider=xai").await;
    let state = started["state"].as_str().unwrap().to_owned();
    assert_eq!(
        final_status(&base, &state, 10).await,
        json!({"status": "error", "error": "Failed to save token to file: a file in the auth directory at this account's credential name is not this account's xAI credential; move it aside and sign in again"})
    );
    assert_eq!(std::fs::read_to_string(auth.join(&name)).unwrap(), foreign);
    let names: Vec<String> = std::fs::read_dir(&auth)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec![name]);
    server.abort();
    fake_server.abort();
}

/// A login worker that panics fails its session at once, with a fixed message,
/// instead of leaving the dashboard waiting until the session expires: here the panic
/// is on the blocking task that saves the credential, which the worker passes on.
#[tokio::test]
async fn a_panicking_login_worker_fails_the_session() {
    let (fake, _seen, fake_server) = fake_logins().await;
    let f = Fixture::new("panicklogin");
    let options = management::Options {
        login_base: Some(fake.clone()),
        login_panic: true,
        ..Default::default()
    };
    let (base, _state, server) = serve_with(&f, options).await;
    let started = oauth_get(&base, "/oauth/auth-url?provider=codex").await;
    let state = started["state"].as_str().unwrap().to_owned();
    let delivered = f.rt.deliver_oauth_callback(&cpa_server::runtime::OAuthCallback {
        provider: "codex",
        state: state.clone(),
        code: "fake-code".into(),
        error: String::new(),
    });
    assert_eq!(delivered, cpa_server::runtime::OAuthDelivery::Delivered);
    assert_eq!(
        final_status(&base, &state, 5).await,
        json!({"status": "error", "error": "Sign-in failed because of an internal error. Start the sign-in again."})
    );
    server.abort();
    fake_server.abort();
}

/// Codex: the main listener's `/codex/callback` hands the code to the pending login,
/// which exchanges it (PKCE verifier, Go's redirect URI) and saves the credential.
#[tokio::test]
async fn codex_login_completes_through_the_main_listener_callback() {
    let (fake, seen, fake_server) = fake_logins().await;
    let f = Fixture::new("codexlogin");
    let (base, _state, server) = login_server(&f, &fake).await;
    let started = oauth_get(&base, "/oauth/auth-url?provider=codex").await;
    let state = started["state"].as_str().unwrap().to_owned();
    assert!(
        started["url"]
            .as_str()
            .unwrap()
            .starts_with("https://auth.openai.com/oauth/authorize?")
    );
    let delivered = f.rt.deliver_oauth_callback(&cpa_server::runtime::OAuthCallback {
        provider: "codex",
        state: state.clone(),
        code: "fake-code".into(),
        error: String::new(),
    });
    assert_eq!(delivered, cpa_server::runtime::OAuthDelivery::Delivered);
    assert_eq!(final_status(&base, &state, 10).await, json!({"status": "ok"}));
    let token_request = seen
        .lock()
        .unwrap()
        .iter()
        .find(|(p, _)| p == "/oauth/token")
        .unwrap()
        .1
        .clone();
    assert!(token_request.contains("code=fake-code"), "{token_request}");
    assert!(
        token_request.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"),
        "{token_request}"
    );
    assert!(token_request.contains("code_verifier="), "{token_request}");
    let creds = f.rt.store().snapshot();
    let codex = creds
        .iter()
        .find(|c| c.provider == "codex")
        .expect("saved and published");
    assert_eq!(codex.str("email"), Some("c@example.invalid"));
    assert_ne!(
        f.rt.deliver_oauth_callback(&cpa_server::runtime::OAuthCallback {
            provider: "codex",
            state,
            code: "again".into(),
            error: String::new(),
        }),
        cpa_server::runtime::OAuthDelivery::Delivered
    );
    server.abort();
    fake_server.abort();
}

/// Claude: a pasted callback URL posted to `/oauth/callback` finishes the login.
#[tokio::test]
async fn claude_login_completes_from_a_posted_redirect_url() {
    let (fake, _seen, fake_server) = fake_logins().await;
    let f = Fixture::new("claudelogin");
    let (base, _state, server) = login_server(&f, &fake).await;
    let started = oauth_get(&base, "/oauth/auth-url?provider=claude").await;
    let state = started["state"].as_str().unwrap().to_owned();
    let r: Value = wreq::Client::new()
        .post(format!("{base}/v8/management/oauth/callback"))
        .json(&json!({"provider": "claude",
            "redirect_url": format!("http://localhost:54545/callback?code=fake-code&state={state}")}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r, json!({"status": "ok"}), "the callback needs no management key");
    assert_eq!(final_status(&base, &state, 10).await, json!({"status": "ok"}));
    let creds = f.rt.store().snapshot();
    let claude = creds
        .iter()
        .find(|c| c.provider == "claude")
        .expect("saved and published");
    assert_eq!(claude.str("access_token"), Some("sk-ant-oat-fake"));
    server.abort();
    fake_server.abort();
}

/// Go `saveTokenRecord` for Claude: the legacy file a new login replaces is deleted
/// through the store only after the canonical credential is stored.
#[tokio::test]
async fn claude_login_removes_the_legacy_file_only_after_the_store_has_the_new_one() {
    let (fake, _seen, fake_server) = fake_logins().await;
    for down in [true, false] {
        let f = Fixture::new(&format!("claudelegacy-{down}"));
        let legacy = f.dir.join("auth/claude-a@example.invalid.json");
        std::fs::write(
            &legacy,
            r#"{"type":"claude","email":"a@example.invalid","organization_uuid":"org-1"}"#,
        )
        .unwrap();
        let store = Arc::new(FakeStore {
            dir: f.dir.join("auth"),
            down,
            ..Default::default()
        });
        let options = management::Options {
            login_base: Some(fake.clone()),
            store: Some(store.clone()),
            ..Default::default()
        };
        let (base, _state, server) = serve_with(&f, options).await;
        let started = oauth_get(&base, "/oauth/auth-url?provider=claude").await;
        let state = started["state"].as_str().unwrap().to_owned();
        wreq::Client::new()
            .post(format!("{base}/v8/management/oauth/callback"))
            .json(&json!({"provider": "claude",
                "redirect_url": format!("http://localhost:54545/callback?code=fake-code&state={state}")}))
            .send()
            .await
            .unwrap();
        let status = final_status(&base, &state, 10).await;
        let pushed = store.pushed.lock().unwrap().clone();
        assert!(
            pushed[0].starts_with("Update auth claude-") && pushed[0].ends_with("-a@example.invalid.json"),
            "{pushed:?}"
        );
        if down {
            assert_eq!(
                status,
                json!({"status": "error", "error": "Failed to save authentication tokens"})
            );
            assert_eq!(pushed.len(), 1, "{pushed:?}");
            assert!(legacy.exists(), "kept while the canonical credential is not stored");
        } else {
            assert_eq!(status, json!({"status": "ok"}));
            assert_eq!(pushed[1..], ["Delete auth claude-a@example.invalid.json"]);
            assert!(!legacy.exists());
        }
        server.abort();
    }
    fake_server.abort();
}

/// A store whose pushes are really pending on the runtime's timers. When the Claude
/// push arrives (its save holds the disk lock), it starts a credential upload, whose
/// save takes the lock next and waits on its own push.
#[derive(Default)]
struct SlowStore {
    dir: PathBuf,
    upload: std::sync::Mutex<Option<(String, Vec<u8>)>>,
    imported: std::sync::Mutex<Option<tokio::task::JoinHandle<u16>>>,
}

impl cpa_server::persist::StorePersister for SlowStore {
    fn persist_config(&self) -> futures_util::future::BoxFuture<'_, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn persist_auth_files(
        &self,
        message: String,
        _: Vec<PathBuf>,
    ) -> futures_util::future::BoxFuture<'_, anyhow::Result<()>> {
        Box::pin(async move {
            if message.contains("claude") {
                let (url, body) = self.upload.lock().unwrap().take().unwrap();
                *self.imported.lock().unwrap() = Some(tokio::spawn(async move {
                    let response = wreq::Client::new()
                        .post(url)
                        .bearer_auth("fake-management-only")
                        .header("content-type", "multipart/form-data; boundary=b")
                        .body(body)
                        .send()
                        .await
                        .unwrap();
                    response.status().as_u16()
                }));
                // Long enough for the import to be waiting on the disk lock.
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            } else {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            Ok(())
        })
    }
    fn delete_auth(&self, _: PathBuf) -> anyhow::Result<()> {
        Ok(())
    }
    fn auth_dir(&self) -> PathBuf {
        self.dir.clone()
    }
}

/// A finished login publishes off the runtime workers: on a one-worker runtime, a save
/// holding the disk lock while its store push waits on a timer must not starve.
#[test]
fn a_finished_login_never_waits_for_the_disk_lock_on_a_runtime_worker() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let (done, finished) = std::sync::mpsc::channel();
    rt.spawn(async move {
        let key = r#"{"type":"codex","access_token":"fake-access"}"#;
        let (fake, _seen, fake_server) = fake_logins().await;
        let f = Fixture::new("slowstore");
        let store = Arc::new(SlowStore {
            dir: f.dir.join("auth"),
            ..Default::default()
        });
        let options = management::Options {
            login_base: Some(fake.clone()),
            store: Some(store.clone()),
            ..Default::default()
        };
        let (base, _state, server) = serve_with(&f, options).await;
        let body = format!(
            "--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"codex-upload.json\"\r\nContent-Type: application/json\r\n\r\n{key}\r\n--b--\r\n"
        );
        *store.upload.lock().unwrap() = Some((
            format!("{base}/v8/management/credentials"),
            body.into_bytes(),
        ));
        let started = oauth_get(&base, "/oauth/auth-url?provider=claude").await;
        let state = started["state"].as_str().unwrap().to_owned();
        wreq::Client::new()
            .post(format!("{base}/v8/management/oauth/callback"))
            .json(&json!({"provider": "claude",
                "redirect_url": format!("http://localhost:54545/callback?code=fake-code&state={state}")}))
            .send()
            .await
            .unwrap();
        let status = final_status(&base, &state, 10).await;
        let import = store.imported.lock().unwrap().take().unwrap();
        let imported = import.await.unwrap();
        server.abort();
        fake_server.abort();
        let _ = done.send((status, imported));
    });
    let Ok((status, imported)) = finished.recv_timeout(std::time::Duration::from_secs(20)) else {
        // The runtime's only worker is blocked: dropping it would wait forever.
        std::mem::forget(rt);
        panic!("the login and the import deadlocked on the disk lock");
    };
    assert_eq!((status, imported), (json!({"status": "ok"}), 200));
}

#[tokio::test]
async fn removed_oauth_maps_refuse_writes_and_keep_the_running_config_on_reload() {
    let f = Fixture::new("removed-oauth-maps");
    let (base, server) = f.server().await;
    let before = f.file();
    let running = f.rt.config().document.clone();
    let client = wreq::Client::new();
    for (field, entries) in [
        ("settings", json!([{"name":"model", "max-context-length":42}])),
        (
            "request-scoped-errors",
            json!([{"status":401, "match":["denied"], "action":"stop"}]),
        ),
    ] {
        let response = client
            .put(format!("{base}/v8/management/config/oauth/{field}"))
            .bearer_auth("fake-management-only")
            .json(&json!({" Gemini-CLI ": entries}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 422);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["error"], "invalid_config");
        assert!(body["message"].as_str().unwrap().contains(&format!("oauth.{field}")));
        assert_eq!(f.file(), before);
        assert_eq!(f.rt.config().document, running);

        let legacy = format!(
            "{before}\noauth-{field}:\n  gemini: {}\n",
            serde_json::to_string(&entries).unwrap()
        );
        std::fs::write(f.dir.join("config.yaml"), legacy).unwrap();
        let error = watching::reload(&f.state).unwrap_err().to_string();
        assert!(error.contains(&format!("oauth.{field}")), "{error}");
        assert_eq!(f.rt.config().document, running);
        std::fs::write(f.dir.join("config.yaml"), &before).unwrap();
        watching::reload(&f.state).unwrap();
    }
    server.abort();
}

/// The removed providers' logins, imports and key lists answer 410, and a failed Codex
/// exchange reports Go's "token exchange failed" text in the session.
#[tokio::test]
async fn removed_provider_operations_fail_and_codex_exchange_errors_read_like_go() {
    let (fake, _seen, fake_server) = fake_logins().await;
    let f = Fixture::new("loginselect");
    let (base, _state, server) = login_server(&f, &fake).await;
    // Logins, imports and key lists of the removed providers answer 410 with migration
    // guidance (issue #49), on both API generations.
    let client = wreq::Client::new();
    for (method, path) in [
        ("GET", "/v8/management/oauth/auth-url?provider=kimi&channel=ai"),
        ("GET", "/v8/management/oauth/auth-url?provider=antigravity"),
        ("GET", "/v8/management/oauth/auth-url?provider=devin"),
        ("POST", "/v8/management/oauth/import?provider=vertex"),
        ("GET", "/v0/management/kimi-auth-url"),
        ("GET", "/v0/management/kimi-ai-auth-url"),
        ("GET", "/v0/management/antigravity-auth-url"),
        ("GET", "/v0/management/devin-auth-url"),
        ("POST", "/v0/management/vertex/import"),
        ("GET", "/v0/management/gemini-api-key"),
        ("PUT", "/v0/management/openai-compatibility"),
        ("PATCH", "/v0/management/vertex-api-key"),
        ("DELETE", "/v0/management/interactions-api-key"),
    ] {
        let response = client
            .request(method.parse().unwrap(), format!("{base}{path}"))
            .bearer_auth("fake-management-only")
            .body("[]")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 410, "{method} {path}");
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["code"], "removed_provider", "{method} {path}: {body}");
        let error = body["error"].as_str().unwrap_or_default();
        assert!(
            error.contains("is no longer supported") && error.contains("Claude, Codex, Grok (xai) and Muse (meta)"),
            "{method} {path}: {body}"
        );
    }
    // An unknown provider still reads as not found.
    let unknown = client
        .get(format!("{base}/v8/management/oauth/auth-url?provider=nonesuch"))
        .bearer_auth("fake-management-only")
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status().as_u16(), 404);
    server.abort();

    // A token endpoint that rejects the code.
    let rejecting = axum::Router::new()
        .fallback(|| async { (axum::http::StatusCode::BAD_REQUEST, r#"{"error":"invalid_grant"}"#) });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bad = format!("http://{}", listener.local_addr().unwrap());
    let bad_server = tokio::spawn(async move { axum::serve(listener, rejecting).await.unwrap() });
    let (base, _state, server) = login_server(&f, &bad).await;
    let started = oauth_get(&base, "/oauth/auth-url?provider=codex").await;
    let state = started["state"].as_str().unwrap().to_owned();
    assert_eq!(
        f.rt.deliver_oauth_callback(&cpa_server::runtime::OAuthCallback {
            provider: "codex",
            state: state.clone(),
            code: "fake-code".into(),
            error: String::new(),
        }),
        cpa_server::runtime::OAuthDelivery::Delivered
    );
    // Go appends the upstream body; session errors are readable by anyone holding
    // the state, so cliproxy-rs stops at the status (deliberate hardening).
    assert_eq!(
        final_status(&base, &state, 10).await,
        json!({"status": "error", "error":
            "Failed to exchange authorization code for tokens: token exchange failed with status 400"})
    );
    server.abort();
    bad_server.abort();
    fake_server.abort();
}

/// Go v8.0.35 `TestConfigV8SaveNormalizesCollectionStylesToBlockYAML` (`0a23627d`):
/// JSON written through the v8 config API lands as block YAML with plain keys, and an
/// empty list stays `[]`. Untouched text keeps its own style (DIFFERENCES-FROM-GO.md).
#[tokio::test]
async fn config_writes_render_json_values_as_block_yaml() {
    let f = Fixture::new("styles");
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    let send = |method, at: &str, body: &str| {
        client
            .request(method, format!("{base}/v8/management{at}"))
            .bearer_auth("fake-management-only")
            .body(body.to_owned())
            .send()
    };
    let compat = |n: &str| {
        format!(
            r#"[{{"name":"compat-{n}","base-url":"https://api.example.com","headers":{{"User-Agent":"$User-Agent"}},"models":[{{"alias":"coding","name":"gpt-4o"}}],"keys":[{{"api-key":"fake-compat-{n}"}}]}}]"#
        )
    };
    let block = "api-keys:\n  claude:\n    - name: compat-N\n      base-url: https://api.example.com\n      headers:\n        User-Agent: $User-Agent\n      models:\n        - alias: coding\n          name: gpt-4o\n      keys:\n        - api-key: fake-compat-N\n";
    let r = send(wreq::Method::PUT, "/config/api-keys/claude", &compat("1"))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(f.file().ends_with(&block.replace('N', "1")), "{}", f.file());
    let patch = format!(
        r#"{{"api-keys":{{"claude":{}}},"server":{{"trusted-proxies":[]}}}}"#,
        compat("2")
    );
    let r = send(wreq::Method::PATCH, "/config", &patch).await.unwrap();
    assert_eq!(r.status(), 200);
    let file = f.file();
    assert!(file.ends_with(&block.replace('N', "2")), "{file}");
    assert!(file.contains("  trusted-proxies: []\n"), "{file}");
    assert!(
        file.contains("  api-keys: [fake-client]\n"),
        "untouched text keeps its style: {file}"
    );
    // A whole-config PUT is all JSON: block collections and plain keys throughout, and
    // the saved file loads.
    let root = format!(
        r#"{{"config-version":8,"management":{{"secret-key":"fake-management-only"}},"server":{{"port":8317}},"api-keys":{{"claude":{}}}}}"#,
        compat("3")
    );
    let r = send(wreq::Method::PUT, "/config", &root).await.unwrap();
    assert_eq!(r.status(), 200);
    let file = f.file();
    assert!(file.contains(&block.replace('N', "3")), "{file}");
    for quoted in ["\"config-version\":", "\"server\":", "\"api-keys\":", "[{", "{\""] {
        assert!(!file.contains(quoted), "{quoted} in {file}");
    }
    let loaded = cpa_core::config::Config::parse(&file).unwrap();
    let compat = &loaded.document["api-keys"]["claude"][0];
    assert_eq!(compat["headers"]["User-Agent"].as_str(), Some("$User-Agent"));
    server.abort();
}

/// Go `TestConfigV8SharedUpstreamRoundTrip` (52d5507d): a historical
/// `oauth.providers.codex` shared field reads at `upstream/...` without a rewrite,
/// upstream writes keep the OAuth-only header scope, and an invalid write to the
/// historical path is refused without touching the file.
#[tokio::test]
async fn config_shared_upstream_round_trip() {
    let f = Fixture::from_yaml("upstream", |auth, hash| {
        format!(
            "management: {{secret-key: '{hash}'}}\noauth: {{auth-dir: {}, providers: {{codex: {{stream-bootstrap-buffering: true, header-defaults: {{beta-features: oauth-beta}}}}}}}}\n",
            auth.display()
        )
    });
    let raw = f.file();
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    let send = |method, at: &str, body: &str| {
        client
            .request(method, format!("{base}/v8/management/config/{at}"))
            .bearer_auth("fake-management-only")
            .body(body.to_owned())
            .send()
    };
    let request = |method: wreq::Method, at: &'static str, body: &'static str, want: u16| {
        let r = send(method.clone(), at, body);
        async move {
            let r = r.await.unwrap();
            let status = r.status().as_u16();
            let text = r.text().await.unwrap();
            assert_eq!(status, want, "{method} {at}: {text}");
            text
        }
    };
    let got = request(wreq::Method::GET, "upstream/codex/stream-bootstrap-buffering", "", 200).await;
    assert_eq!(got.trim(), "true");
    assert_eq!(f.file(), raw, "GET rewrote the historical document");
    request(
        wreq::Method::PUT,
        "upstream/codex/stream-bootstrap-buffering",
        "false",
        200,
    )
    .await;
    request(wreq::Method::PUT, "oauth/auth-auto-refresh-workers", "3", 200).await;
    request(
        wreq::Method::PATCH,
        "upstream/claude",
        r#"{"header-defaults":{"timezone":"Asia/Singapore","stabilize-device-profile":false}}"#,
        200,
    )
    .await;
    let loaded = Config::parse(&f.file()).unwrap();
    let api = loaded.for_api_key();
    for cfg in [&loaded, &*api] {
        let d = &cfg.document;
        assert_eq!(
            d["upstream"]["codex"]["stream-bootstrap-buffering"],
            serde_yaml_ng::Value::Bool(false)
        );
        assert_eq!(d["upstream"]["claude"]["header-defaults"]["timezone"], "Asia/Singapore");
        assert_eq!(d["oauth"]["auth-auto-refresh-workers"], 3);
    }
    assert_eq!(
        loaded.document["oauth"]["providers"]["codex"]["header-defaults"]["beta-features"],
        "oauth-beta"
    );
    assert_eq!(
        api.document["oauth"]["providers"]["codex"]["header-defaults"]["beta-features"],
        ""
    );
    let before = f.file();
    // v8.0.35 (3be5fa44) accepts the historical path as an alias; an invalid value is
    // still refused.
    request(
        wreq::Method::PUT,
        "oauth/providers/codex/stream-bootstrap-buffering",
        r#""invalid""#,
        422,
    )
    .await;
    assert_eq!(
        f.file(),
        before,
        "a rejected historical-path write changed the document"
    );
    request(wreq::Method::DELETE, "upstream/claude", "", 200).await;
    assert!(!request(wreq::Method::GET, "upstream/claude", "", 404).await.is_empty());
    let loaded = Config::parse(&f.file()).unwrap();
    assert!(
        loaded.document.get("upstream").and_then(|u| u.get("claude")).is_none(),
        "{}",
        f.file()
    );
    server.abort();
}

/// Legacy files whose migration the text form cannot finish alone (a `codex` block
/// holding a shared `upstream.codex` field, a bare or empty `claude`) keep the comments
/// of their other sections through a v8 setting save, and save fully migrated; a v0
/// save leaves them in the legacy layout.
#[tokio::test]
async fn legacy_migration_with_shared_upstream_moves_keeps_other_comments() {
    for (tail, check) in [
        (
            "codex: {response-steering: true}\n",
            &["upstream", "codex", "response-steering"][..],
        ),
        ("claude:\n", &["upstream", "claude"][..]),
        ("claude: {}\n", &["upstream", "claude"][..]),
    ] {
        let f = Fixture::from_yaml("shared-moves", |auth, hash| {
            format!(
                "remote-management:\n  # keep this comment\n  secret-key: '{hash}'\nauth-dir: {}\n{tail}",
                auth.display()
            )
        });
        let raw = f.file();
        let (base, server) = f.server().await;
        let client = wreq::Client::new();
        for (at, body) in [
            ("/v8/management/config/routing/retry/request-retry", "3"),
            ("/v0/management/debug", r#"{"value":true}"#),
        ] {
            std::fs::write(f.dir.join("config.yaml"), &raw).unwrap();
            let r = client
                .put(format!("{base}{at}"))
                .bearer_auth("fake-management-only")
                .body(body)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 200, "{tail} {at}");
            let file = f.file();
            assert!(
                file.contains("management:\n  # keep this comment\n"),
                "{tail} {at}: {file}"
            );
            if at.starts_with("/v0/") {
                // A v0 save keeps a legacy-only file in its layout (Go v8.0.35).
                assert_eq!(file, format!("{raw}debug: true\n"), "{tail} {at}");
                continue;
            }
            // Nothing is left to migrate: the saved values are the canonical ones.
            let saved: serde_yaml_ng::Value = serde_yaml_ng::from_str(&file).unwrap();
            let canonical = cpa_core::config::ConfigDocument::parse(&file).unwrap();
            assert_eq!(&saved, canonical.value(), "{tail} {at}: {file}");
            let value = check.iter().try_fold(&saved, |v, k| v.get(*k));
            assert!(value.is_some_and(|v| !v.is_null()), "{tail} {at}: {file}");
        }
        server.abort();
    }
}

/// Go v8.0.35 Sign in with ChatGPT binding (`d50a1995`, `ae3ee3be`) against a local
/// token endpoint: start validation, the legacy-callback refusal, callback forms and
/// conflicts, the bound file, the v8 summary, failures, cancellation, and the v8-only
/// invalidation by a credential upload.
#[tokio::test]
async fn siwc_binding_flows_follow_go() {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    use std::time::Duration;
    let forms: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let (exchanging, resume) = (
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Semaphore::new(0)),
    );
    let app = axum::Router::new().route(
        "/api/accounts/oauth/token",
        axum::routing::post({
            let (forms, exchanging, resume) = (forms.clone(), exchanging.clone(), resume.clone());
            move |body: String| async move {
                forms.lock().unwrap().push(body.clone());
                let code = url::form_urlencoded::parse(body.as_bytes())
                    .find(|(k, _)| k == "code")
                    .map(|(_, v)| v.into_owned())
                    .unwrap_or_default();
                if code == "fail" {
                    return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "secret-sentinel".to_owned());
                }
                // `hang` never answers (its exchange is expected to be dropped);
                // `hold` waits for a permit.
                if code == "hang" {
                    exchanging.notify_one();
                    std::future::pending::<()>().await;
                }
                if code == "hold" {
                    exchanging.notify_one();
                    // Consumed: a dropped permit would release the next hold too.
                    resume.acquire().await.unwrap().forget();
                }
                (
                    axum::http::StatusCode::OK,
                    r#"{"access_token":"siwc-at","refresh_token":"siwc-rt","id_token":"siwc-id","scope":"openid chatgpt.tokens.use.direct","expires_in":3600}"#.to_owned(),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fake = format!("http://{}", listener.local_addr().unwrap());
    let upstream = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let original = r#"{"type":"codex","email":"a@example.invalid","account_id":"acct","access_token":"outer","refresh_token":"outer-rt","note":"keep","big":123456789012345678901234567890,"siwc":{"device_id":"E61BBE28-07EF-466D-8E5D-A344F94AB305"}}"#;
    let f = Fixture::from_yaml("siwc", |auth, hash| {
        std::fs::write(auth.join("codex.json"), original).unwrap();
        std::fs::write(
            auth.join("claude.json"),
            r#"{"type":"claude","access_token":"a","refresh_token":"b"}"#,
        )
        .unwrap();
        std::fs::write(auth.join("no-refresh.json"), r#"{"type":"codex","access_token":"a"}"#).unwrap();
        format!(
            "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\n  providers:\n    codex:\n      signin-with-chatgpt-identifier: '  Custom Agent  '\n",
            auth.display()
        )
    });
    let file = f.dir.join("auth").join("codex.json");
    let (settled_tx, mut settled) = tokio::sync::mpsc::unbounded_channel();
    let options = management::Options {
        login_base: Some(fake.clone()),
        login_settled: Some(settled_tx),
        ..Default::default()
    };
    let (base, _management, server) = serve_with(&f, options).await;
    let client = wreq::Client::new();
    let send = |method: wreq::Method, path: String, body: String| {
        let request = client
            .request(method, format!("{base}{path}"))
            .bearer_auth("fake-management-only")
            .header("content-type", "application/json")
            .body(body);
        async move {
            let response = request.send().await.unwrap();
            let status = response.status().as_u16();
            let text = response.text().await.unwrap();
            (status, text)
        }
    };
    let bind = |body: &str| send(wreq::Method::POST, "/v8/management/oauth/siwc/bind".into(), body.into());
    let callback = |body: Value| {
        send(
            wreq::Method::POST,
            "/v8/management/oauth/callback".into(),
            body.to_string(),
        )
    };
    let redirect = |state: &str, code: &str| {
        format!("http://127.0.0.1:1455/auth/callback?state={state}&code={code}&client_id=oaiapp_issued")
    };
    let parsed = |text: &str| serde_json::from_str::<Value>(text).unwrap();
    async fn wait_settled(settled: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> String {
        tokio::time::timeout(std::time::Duration::from_secs(5), settled.recv())
            .await
            .unwrap()
            .unwrap()
    }
    let bound_file = || std::fs::read_to_string(&file).unwrap();

    // Start validation (gin `ShouldBindJSON`, then the target).
    for (body, status, want) in [
        ("", 400, json!({"error": "invalid request body"})),
        ("[1]", 400, json!({"error": "invalid request body"})),
        (r#"{"name":1}"#, 400, json!({"error": "invalid request body"})),
        ("{}", 400, json!({"error": "name is required"})),
        (
            r#"{"name":"missing.json"}"#,
            404,
            json!({"error": "auth file not found"}),
        ),
        (
            r#"{"name":"claude.json"}"#,
            400,
            json!({"error": "SIWC binding requires a file-backed Codex OAuth credential"}),
        ),
        (
            r#"{"name":"no-refresh.json"}"#,
            400,
            json!({"error": "SIWC binding requires a file-backed Codex OAuth credential"}),
        ),
    ] {
        let (got, text) = bind(body).await;
        assert_eq!((got, parsed(&text)), (status, want), "{body}");
    }
    let (status, _) = send(
        wreq::Method::POST,
        "/v0/management/oauth/siwc/bind".into(),
        r#"{"name":"codex.json"}"#.into(),
    )
    .await;
    assert_eq!(status, 404, "v8 only");
    let keyless = client
        .post(format!("{base}/v8/management/oauth/siwc/bind"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(keyless.status().as_u16(), 401);

    // A binding starts with server-held PKCE, device ID and redirect.
    let (status, text) = bind(r#"{"NAME":" codex.json "}"#).await;
    assert_eq!(status, 200, "{text}");
    let started = parsed(&text);
    let sid = started["state"].as_str().unwrap().to_owned();
    assert_eq!(
        started,
        json!({"status": "ok", "url": started["url"], "state": sid, "flow": "siwc_bind", "expires_in": 300})
    );
    let url = url::Url::parse(started["url"].as_str().unwrap()).unwrap();
    assert_eq!(
        url.as_str().split('?').next(),
        Some("https://auth.openai.com/api/accounts/authorize")
    );
    let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(query["client_id"], "dynamic_agent_client");
    assert_eq!(query["redirect_uri"], "http://127.0.0.1:1455/auth/callback");
    assert_eq!(query["agent_name_hint"], "Custom Agent");
    assert_eq!(
        query["ext_agent_host_id"],
        "urn:uuid:e61bbe28-07ef-466d-8e5d-a344f94ab305"
    );
    assert_eq!(query["state"], sid);
    assert_eq!(query["code_challenge_method"], "S256");
    assert!(!query["nonce"].is_empty() && query["nonce"] != sid);
    let (status, text) = bind(r#"{"name":"codex.json"}"#).await;
    assert_eq!(
        (status, parsed(&text)),
        (
            409,
            json!({"error": "a SIWC binding is already pending for this credential"})
        )
    );

    // Legacy sinks refuse the state and leave the binding pending.
    let (status, text) = send(
        wreq::Method::POST,
        "/v0/management/oauth-callback".into(),
        json!({"provider": "codex", "state": sid, "code": "issued-code"}).to_string(),
    )
    .await;
    assert_eq!(
        (status, parsed(&text)),
        (409, json!({"status": "error", "error": "oauth flow is not pending"}))
    );
    assert_eq!(
        f.rt.deliver_oauth_callback(&cpa_server::runtime::OAuthCallback {
            provider: "codex",
            state: sid.clone(),
            code: "issued-code".into(),
            error: String::new(),
        }),
        cpa_server::runtime::OAuthDelivery::RequiresV8
    );

    // Incomplete or conflicting callbacks are refused before any exchange.
    let evil = redirect(&sid, "issued-code").replace("127.0.0.1", "evil.example");
    let userinfo = redirect(&sid, "issued-code").replace("http://", "http://u@");
    for body in [
        json!({"state": sid, "code": "c"}),
        json!({"state": sid, "client_id": "client"}),
        json!({"state": sid, "code": "c", "client_id": "client", "provider": "claude"}),
        json!({"state": sid, "redirect_url": redirect("other-state", "c")}),
        json!({"state": sid, "redirect_url": evil}),
        json!({"redirect_url": userinfo}),
        json!({"redirect_url": format!("{}#frag", redirect(&sid, "c"))}),
        json!({"redirect_url": format!("{}&client_id=other", redirect(&sid, "c"))}),
        json!({"state": sid, "client_id": "other", "redirect_url": redirect(&sid, "c")}),
    ] {
        let (status, text) = callback(body.clone()).await;
        assert_eq!(status, 400, "{body}: {text}");
    }
    // Go compares the decoded `URL.Path`: `&`, `;` and `+` are literal path bytes.
    for path in [
        "/auth/callback&junk",
        "/auth/callback;junk",
        "/auth/call+back",
        "/auth/callback/",
    ] {
        let url = redirect(&sid, "c").replace("/auth/callback", path);
        let (status, text) = callback(json!({"redirect_url": url})).await;
        assert_eq!(
            (status, parsed(&text)),
            (400, json!({"status": "error", "error": "unexpected SIWC callback URL"})),
            "{path}"
        );
    }
    let (status, _) = send(
        wreq::Method::GET,
        format!("/v8/management/oauth/callback?state={sid}&code=c&client_id=a&client_id=b"),
        String::new(),
    )
    .await;
    assert_eq!(status, 400);
    assert!(forms.lock().unwrap().is_empty());

    // The pasted loopback URL completes it with the issued client ID.
    // An escaped path is the same path to Go.
    let encoded = redirect(&sid, "issued-code").replace("/auth/callback", "/auth/%63allback");
    let (status, text) = callback(json!({"provider": "openai", "redirect_url": encoded})).await;
    assert_eq!((status, parsed(&text)), (200, json!({"status": "ok"})));
    assert_eq!(wait_settled(&mut settled).await, sid);
    assert_eq!(final_status(&base, &sid, 5).await, json!({"status": "ok"}));
    let form = forms.lock().unwrap().pop().unwrap();
    let form: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(form.as_bytes()).into_owned().collect();
    assert_eq!(form["client_id"], "oaiapp_issued");
    assert_eq!(form["code"], "issued-code");
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["redirect_uri"], "http://127.0.0.1:1455/auth/callback");
    assert_eq!(form["resource"], "https://api.openai.com/v1");
    let verifier = &form["code_verifier"];
    assert_eq!(
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
        query["code_challenge"],
        "the verifier the session kept"
    );
    assert!(!text.contains(verifier.as_str()));
    // Go `json.Marshal` of the merged file: sorted, compact, other members verbatim.
    let bound = bound_file();
    assert!(
        bound.starts_with(r#"{"access_token":"outer","account_id":"acct","big":123456789012345678901234567890,"email":"a@example.invalid","note":"keep","refresh_token":"outer-rt","siwc":{"access_token":"siwc-at","client_id":"oaiapp_issued","device_id":"e61bbe28-07ef-466d-8e5d-a344f94ab305","expired":""#),
        "{bound}"
    );
    assert!(
        bound.ends_with(r#""refresh_token":"siwc-rt","scope":"openid chatgpt.tokens.use.direct","scopes":["openid","chatgpt.tokens.use.direct"]},"type":"codex","use_siwc":true}"#),
        "{bound}"
    );
    let published =
        f.rt.store()
            .snapshot()
            .into_iter()
            .find(|c| c.id == "codex.json")
            .unwrap();
    assert_eq!(published.metadata["use_siwc"], true);
    assert_eq!(published.metadata["siwc"]["client_id"], "oaiapp_issued");
    // The v8 list summarizes it without a token; v0 is unchanged.
    let (_, listed) = send(wreq::Method::GET, "/v8/management/credentials".into(), String::new()).await;
    for secret in ["siwc-at", "siwc-rt", "siwc-id", "oaiapp_issued"] {
        assert!(!listed.contains(secret), "{secret}");
    }
    let listed = parsed(&listed);
    let entry = |listed: &Value, name: &str| {
        listed["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == name)
            .unwrap()
            .clone()
    };
    let codex = entry(&listed, "codex.json");
    assert_eq!(
        (&codex["siwc_bind_supported"], &codex["use_siwc"], &codex["siwc_bound"]),
        (&json!(true), &json!(true), &json!(true))
    );
    let expires = chrono::DateTime::parse_from_rfc3339(codex["siwc_expires_at"].as_str().unwrap()).unwrap();
    assert!(codex["siwc_expires_at"].as_str().unwrap().ends_with('Z'));
    let left = (expires.to_utc() - chrono::Utc::now()).num_seconds();
    assert!((3300..=3420).contains(&left), "{left}");
    assert_eq!(entry(&listed, "no-refresh.json")["siwc_bind_supported"], false);
    assert!(entry(&listed, "claude.json").get("siwc_bound").is_none());
    let (_, legacy) = send(wreq::Method::GET, "/v0/management/auth-files".into(), String::new()).await;
    assert!(entry(&parsed(&legacy), "codex.json").get("siwc_bound").is_none());
    // A second callback and a late cancel change nothing.
    let (status, text) = callback(json!({"redirect_url": redirect(&sid, "issued-code")})).await;
    assert_eq!(
        (status, parsed(&text)),
        (
            409,
            json!({"status": "error", "error": "SIWC session is no longer accepting callbacks"})
        )
    );
    let (_, text) = send(
        wreq::Method::DELETE,
        format!("/v8/management/oauth/session?state={sid}"),
        String::new(),
    )
    .await;
    assert_eq!(parsed(&text), json!({"status": "ok", "cancelled": false}));

    let start = || async {
        let (status, text) = bind(r#"{"name":"codex.json"}"#).await;
        assert_eq!(status, 200, "{text}");
        parsed(&text)["state"].as_str().unwrap().to_owned()
    };
    // A denied authorization and a failed exchange report fixed messages and save nothing.
    let sid = start().await;
    let (status, _) = send(
        wreq::Method::GET,
        format!("/v8/management/oauth/callback?state={sid}&error=access_denied%3A+secret-data"),
        String::new(),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(wait_settled(&mut settled).await, sid);
    assert_eq!(
        final_status(&base, &sid, 5).await,
        json!({"status": "error", "error": "SIWC authorization was denied or failed"})
    );
    let sid = start().await;
    let (status, _) = callback(json!({"redirect_url": redirect(&sid, "fail")})).await;
    assert_eq!(status, 200);
    assert_eq!(wait_settled(&mut settled).await, sid);
    assert_eq!(
        final_status(&base, &sid, 5).await,
        json!({"status": "error", "error": "Failed to exchange SIWC authorization code; start a new binding"})
    );
    assert_eq!(bound_file(), bound);

    // Cancelling during the exchange drops it; the state is then unknown.
    let sid = start().await;
    assert_eq!(callback(json!({"redirect_url": redirect(&sid, "hang")})).await.0, 200);
    tokio::time::timeout(Duration::from_secs(5), exchanging.notified())
        .await
        .unwrap();
    let (_, text) = send(
        wreq::Method::DELETE,
        format!("/v8/management/oauth/session?state={sid}"),
        String::new(),
    )
    .await;
    assert_eq!(parsed(&text), json!({"status": "ok", "cancelled": true}));
    let (status, text) = callback(json!({"redirect_url": redirect(&sid, "hang")})).await;
    assert_eq!(
        (status, parsed(&text)),
        (404, json!({"status": "error", "error": "unknown or expired state"}))
    );

    // A v8 upload of the file, even unchanged, fails a pending binding; a v0 one does not.
    let upload = |version: &str| {
        let path = match version {
            "v8" => "/v8/management/credentials?name=codex.json",
            _ => "/v0/management/auth-files?name=codex.json",
        };
        send(wreq::Method::POST, path.into(), bound_file())
    };
    let sid = start().await;
    assert_eq!(callback(json!({"redirect_url": redirect(&sid, "hang")})).await.0, 200);
    tokio::time::timeout(Duration::from_secs(5), exchanging.notified())
        .await
        .unwrap();
    assert_eq!(upload("v8").await.0, 200);
    assert_eq!(
        final_status(&base, &sid, 5).await,
        json!({"status": "error", "error": "SIWC binding target changed; start a new binding"})
    );
    let sid = start().await;
    assert_eq!(callback(json!({"redirect_url": redirect(&sid, "hold")})).await.0, 200);
    tokio::time::timeout(Duration::from_secs(5), exchanging.notified())
        .await
        .unwrap();
    assert_eq!(upload("v0").await.0, 200);
    resume.add_permits(1);
    assert_eq!(wait_settled(&mut settled).await, sid);
    assert_eq!(final_status(&base, &sid, 5).await, json!({"status": "ok"}));

    // A v0 upload that swaps in another account does not fail the binding, but the
    // tokens never land on that account.
    let sid = start().await;
    assert_eq!(callback(json!({"redirect_url": redirect(&sid, "hold")})).await.0, 200);
    tokio::time::timeout(Duration::from_secs(5), exchanging.notified())
        .await
        .unwrap();
    let other = original.replace(r#""account_id":"acct""#, r#""account_id":"other""#);
    let (status, _) = send(
        wreq::Method::POST,
        "/v0/management/auth-files?name=codex.json".into(),
        other.clone(),
    )
    .await;
    assert_eq!(status, 200);
    let swapped = bound_file();
    assert!(swapped.contains(r#""account_id":"other""#));
    resume.add_permits(1);
    assert_eq!(wait_settled(&mut settled).await, sid);
    assert_eq!(
        final_status(&base, &sid, 5).await,
        json!({"status": "error", "error": "Failed to save SIWC binding; the target may have changed"})
    );
    assert_eq!(bound_file(), swapped);
    assert!(!swapped.contains("siwc-at"));

    // A successful binding clears the target's unauthorized cooldowns and keeps its
    // quota blocks (Go `BindCodexSIWC`), and leaves other credentials alone. Recording
    // a result rewrites the file the way Go's `MarkResult` persist does.
    let fail = |id: &str, model: &str, status: u16, scope| {
        use cpa_core::exec::ExecError;
        let mut selection = cpa_server::runtime::Selection::new("codex", model);
        selection.exclude = ["codex.json", "no-refresh.json"]
            .into_iter()
            .filter(|other| *other != id)
            .map(str::to_owned)
            .collect();
        let lease = f.rt.store().select(selection).unwrap();
        assert_eq!(lease.credential.id, id);
        let mut error = ExecError::local(status, scope, "upstream said no");
        error.retry_after = Some(Duration::from_secs(600));
        lease.complete(cpa_server::runtime::Outcome::Failure(error));
    };
    use cpa_core::exec::FailureScope;
    fail("codex.json", "m-unauthorized", 401, FailureScope::Credential);
    fail("codex.json", "m-quota", 429, FailureScope::Model);
    fail("codex.json", "m-forbidden", 403, FailureScope::Credential);
    fail("no-refresh.json", "m-unauthorized", 401, FailureScope::Credential);
    let cooling = |id: &str| {
        f.rt.store()
            .cooldowns(id)
            .into_iter()
            .map(|c| (c.model, c.status))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        cooling("codex.json"),
        [
            ("m-forbidden".into(), 403),
            ("m-quota".into(), 429),
            ("m-unauthorized".into(), 401)
        ]
    );

    // A binding never lands under an outer token refresh: one that read the
    // credential first commits its rotated tokens, then the binding follows on top.
    let sid = start().await;
    let refreshing = f.rt.store().prepare_lock("codex.json");
    let paused = refreshing.lock().await;
    let read = f.rt.store().get("codex.json").unwrap();
    assert_eq!(
        callback(json!({"redirect_url": redirect(&sid, "issued-code")})).await.0,
        200
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        oauth_get(&base, &format!("/oauth/status?state={sid}")).await,
        json!({"status": "wait"})
    );
    let rotated = cpa_core::credential::MetadataPatch {
        set: serde_json::Map::from_iter([
            ("access_token".into(), "rotated-outer".into()),
            ("refresh_token".into(), "rotated-outer-rt".into()),
        ]),
        remove: vec![],
    };
    f.rt.store().apply_patch("codex.json", read.revision, &rotated).unwrap();
    drop(paused);
    assert_eq!(wait_settled(&mut settled).await, sid);
    assert_eq!(final_status(&base, &sid, 5).await, json!({"status": "ok"}));
    assert_eq!(
        cooling("codex.json"),
        [("m-forbidden".into(), 403), ("m-quota".into(), 429)]
    );
    assert_eq!(cooling("no-refresh.json"), [("m-unauthorized".into(), 401)]);
    let disk: Value = serde_json::from_str(&bound_file()).unwrap();
    let memory = f.rt.store().get("codex.json").unwrap().metadata.clone();
    for meta in [&disk, &Value::Object(memory)] {
        assert_eq!(
            (
                &meta["access_token"],
                &meta["refresh_token"],
                &meta["use_siwc"],
                &meta["siwc"]["access_token"]
            ),
            (
                &json!("rotated-outer"),
                &json!("rotated-outer-rt"),
                &json!(true),
                &json!("siwc-at")
            ),
            "{meta}"
        );
    }

    // So does a v8 delete; the file is then gone.
    let sid = start().await;
    assert_eq!(callback(json!({"redirect_url": redirect(&sid, "hang")})).await.0, 200);
    tokio::time::timeout(Duration::from_secs(5), exchanging.notified())
        .await
        .unwrap();
    let (status, _) = send(
        wreq::Method::DELETE,
        "/v8/management/credentials?name=codex.json".into(),
        String::new(),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        final_status(&base, &sid, 5).await,
        json!({"status": "error", "error": "SIWC binding target changed; start a new binding"})
    );
    assert!(!file.exists());
    let (status, _) = send(
        wreq::Method::POST,
        "/v8/management/credentials?name=codex.json".into(),
        original.into(),
    )
    .await;
    assert_eq!(status, 200);

    // A binding whose exchange is in flight, by its state.
    let pending = || async {
        let sid = start().await;
        assert_eq!(callback(json!({"redirect_url": redirect(&sid, "hang")})).await.0, 200);
        tokio::time::timeout(Duration::from_secs(5), exchanging.notified())
            .await
            .unwrap();
        sid
    };
    let changed = json!({"status": "error", "error": "SIWC binding target changed; start a new binding"});
    let other = f.dir.join("auth").join("other.json");

    // Go `TestSIWCFileMutationScopeAndValidation`: a refused v8 upload, a v8 upload of
    // another file and a v0 upload keep the binding pending; a v8 reupload fails it.
    let sid = pending().await;
    let before = bound_file();
    for (path, body, want) in [
        (
            "/v8/management/credentials?name=codex.json",
            r#"{"type":"codex","excluded_models":123}"#.to_owned(),
            400,
        ),
        ("/v8/management/credentials?name=other.json", before.clone(), 200),
        ("/v0/management/auth-files?name=codex.json", before.clone(), 200),
    ] {
        let (status, text) = send(wreq::Method::POST, path.into(), body).await;
        assert_eq!(status, want, "{path}: {text}");
        assert_eq!(
            oauth_get(&base, &format!("/oauth/status?state={sid}")).await,
            json!({"status": "wait"}),
            "{path}"
        );
    }
    assert_eq!(upload("v8").await.0, 200);
    assert_eq!(final_status(&base, &sid, 5).await, changed);

    // Go `TestSIWCFileMutationRejectsLateExchange`, by route: each v8 mutation of the
    // file fails a binding whose exchange is in flight, and stops that exchange. A
    // result that returns while the mutation runs is
    // `management::siwc::tests::a_late_exchange_lands_nothing_after_a_v8_mutation`.
    for operation in ["upload", "delete", "delete-all", "delete-batch"] {
        let sid = pending().await;
        let before = bound_file();
        if operation.starts_with("delete-") {
            std::fs::write(&other, &before).unwrap();
        }
        let (method, path, body) = match operation {
            "upload" => (
                wreq::Method::POST,
                "/v8/management/credentials?name=codex.json",
                before.clone(),
            ),
            "delete" => (
                wreq::Method::DELETE,
                "/v8/management/credentials?name=codex.json",
                String::new(),
            ),
            "delete-all" => (
                wreq::Method::DELETE,
                "/v8/management/credentials?all=true",
                String::new(),
            ),
            _ => (
                wreq::Method::DELETE,
                "/v8/management/credentials",
                r#"["codex.json","other.json"]"#.to_owned(),
            ),
        };
        let (status, text) = send(method, path.into(), body).await;
        assert_eq!(status, 200, "{operation}: {text}");
        assert_eq!(final_status(&base, &sid, 5).await, changed, "{operation}");
        tokio::time::sleep(Duration::from_millis(300)).await;
        if operation == "upload" {
            assert_eq!(bound_file(), before, "{operation}");
            continue;
        }
        assert!(!file.exists(), "{operation}: file resurrected");
        assert!(
            f.rt.store().get("codex.json").is_none(),
            "{operation}: registration resurrected"
        );
        let (status, _) = send(
            wreq::Method::POST,
            "/v8/management/credentials?name=codex.json".into(),
            original.into(),
        )
        .await;
        assert_eq!(status, 200);
    }

    // The credential file itself may not be a symlink.
    {
        let target = f.dir.join("auth").join("target.json");
        std::fs::rename(&file, &target).unwrap();
        std::os::unix::fs::symlink(&target, &file).unwrap();
        let (status, text) = bind(r#"{"name":"codex.json"}"#).await;
        assert_eq!(
            (status, parsed(&text)),
            (400, json!({"error": "SIWC binding target is unavailable or invalid"}))
        );
    }
    server.abort();
    upstream.abort();
}

/// `raw` with this fixture's management key and private auth-dir added, in the v8
/// layout or, for `legacy`, as legacy root keys (so a legacy file stays legacy).
fn private_config(raw: &str, auth: &std::path::Path, hash: &str, legacy: bool) -> String {
    let auth = auth.display();
    if legacy {
        return format!("remote-management: {{secret-key: '{hash}'}}\nauth-dir: {auth}\n{raw}");
    }
    let mut out = format!("management: {{secret-key: '{hash}'}}\n");
    if raw.contains("oauth: {") {
        out += &raw.replacen("oauth: {", &format!("oauth: {{auth-dir: {auth}, "), 1);
    } else if let Some(line) = raw.lines().find(|l| l.starts_with("oauth:")) {
        out += &raw.replacen(&format!("{line}\n"), &format!("{line}\n  auth-dir: {auth}\n"), 1);
    } else {
        out += raw;
        out += &format!("oauth:\n  auth-dir: {auth}\n");
    }
    out
}

/// One management request: its status and body.
async fn call(base: &str, method: wreq::Method, path: &str, body: &str) -> (u16, String) {
    let r = wreq::Client::new()
        .request(method, format!("{base}{path}"))
        .bearer_auth("fake-management-only")
        .body(body.to_owned())
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.text().await.unwrap())
}

/// Every saved v8 file is fully canonical: nothing is left for a migration to move.
fn assert_canonical(file: &str) {
    let saved: serde_yaml_ng::Value = serde_yaml_ng::from_str(file).unwrap();
    let canonical = cpa_core::config::ConfigDocument::parse(file).unwrap();
    assert_eq!(&saved, canonical.value(), "{file}");
    assert!(file.contains("config-version: 8"), "{file}");
}

/// Go v8.0.35 `TestConfigV8HistoricalFieldPaths` (3be5fa44): every historical v8 field
/// path reads, patches, replaces and deletes its canonical field.
#[tokio::test]
async fn config_v8_historical_field_paths() {
    for (historical, current, value) in [
        (
            "oauth/providers/codex/disable-codex-cloaking",
            "upstream/codex/disable-codex-cloaking",
            "true",
        ),
        (
            "oauth/providers/codex/stream-bootstrap-buffering",
            "upstream/codex/stream-bootstrap-buffering",
            "true",
        ),
        (
            "oauth/providers/codex/stream-bootstrap-timeout",
            "upstream/codex/stream-bootstrap-timeout",
            r#""10s""#,
        ),
        (
            "oauth/providers/codex/orphan-delegation-compatibility",
            "upstream/codex/orphan-delegation-compatibility",
            "true",
        ),
        (
            "oauth/providers/codex/model-level-cooling",
            "upstream/codex/model-level-cooling",
            "true",
        ),
        (
            "oauth/providers/codex/response-steering",
            "upstream/codex/response-steering",
            "true",
        ),
        (
            "oauth/providers/claude/model-level-cooling",
            "upstream/claude/model-level-cooling",
            "true",
        ),
        (
            "oauth/providers/claude/disable-claude-cloak-mode",
            "upstream/claude/disable-claude-cloak-mode",
            "true",
        ),
        (
            "oauth/providers/claude/header-defaults/user-agent",
            "upstream/claude/header-defaults/user-agent",
            r#""test-agent""#,
        ),
        (
            "oauth/providers/claude/header-defaults/package-version",
            "upstream/claude/header-defaults/package-version",
            r#""0.2.0""#,
        ),
        (
            "oauth/providers/claude/header-defaults/runtime-version",
            "upstream/claude/header-defaults/runtime-version",
            r#""v22""#,
        ),
        (
            "oauth/providers/claude/header-defaults/os",
            "upstream/claude/header-defaults/os",
            r#""Windows""#,
        ),
        (
            "oauth/providers/claude/header-defaults/arch",
            "upstream/claude/header-defaults/arch",
            r#""amd64""#,
        ),
        (
            "oauth/providers/claude/header-defaults/timeout",
            "upstream/claude/header-defaults/timeout",
            r#""300""#,
        ),
        (
            "oauth/providers/claude/header-defaults/timezone",
            "upstream/claude/header-defaults/timezone",
            r#""Asia/Shanghai""#,
        ),
        (
            "oauth/providers/claude/header-defaults/stabilize-device-profile",
            "upstream/claude/header-defaults/stabilize-device-profile",
            "true",
        ),
        (
            "oauth/providers/claude/claude-code/disable-cloaking-model-list",
            "upstream/claude/disable-cloaking-model-list",
            "false",
        ),
        (
            "oauth/providers/xai/inject-x-search",
            "upstream/xai/inject-x-search",
            "true",
        ),
        (
            "oauth/providers/codex/optimize-multi-agent-v2",
            "client/codex/optimize-multi-agent-v2",
            "true",
        ),
    ] {
        let f = Fixture::from_yaml("historical-fields", |auth, hash| {
            private_config("server: {port: 8317}\n", auth, hash, false)
        });
        let (base, server) = f.server().await;
        let at = |p: &str| format!("/v8/management/config/{p}");
        assert_eq!(
            call(&base, wreq::Method::PUT, &at(current), value).await.0,
            200,
            "{current}"
        );
        assert_eq!(
            call(&base, wreq::Method::GET, &at(historical), "").await,
            (200, value.to_owned()),
            "{historical}"
        );
        assert_eq!(
            call(&base, wreq::Method::PATCH, &at(historical), value).await.0,
            200,
            "{historical}"
        );
        assert_eq!(
            call(&base, wreq::Method::PUT, &at(historical), "null").await.0,
            200,
            "{historical}"
        );
        // v8.0.35 saves the validated tree directly: an explicit null keeps through
        // either path.
        assert_eq!(
            call(&base, wreq::Method::GET, &at(current), "").await,
            (200, "null".to_owned()),
            "{historical}"
        );
        assert_eq!(
            call(&base, wreq::Method::PUT, &at(current), value).await.0,
            200,
            "{current}"
        );
        assert_eq!(
            call(&base, wreq::Method::PUT, &at(current), "null").await.0,
            200,
            "{current}"
        );
        assert_eq!(
            call(&base, wreq::Method::GET, &at(current), "").await,
            (200, "null".to_owned()),
            "{current}"
        );
        assert_eq!(
            call(&base, wreq::Method::DELETE, &at(historical), "").await.0,
            200,
            "{historical}"
        );
        assert_eq!(
            call(&base, wreq::Method::GET, &at(current), "").await.0,
            404,
            "{historical}"
        );
        assert_eq!(
            call(&base, wreq::Method::GET, &at("server/port"), "").await,
            (200, "8317".to_owned())
        );
        assert_canonical(&f.file());
        server.abort();
    }
}

/// Go `TestConfigV8HistoricalProviderSubtrees`: a historical provider subtree shows the
/// shared and client values it once held, and PATCH merges into, PUT replaces and
/// DELETE removes all of them, leaving unrelated settings and OAuth scope alone.
#[tokio::test]
async fn config_v8_historical_provider_subtrees() {
    let raw = "upstream: {codex: {response-steering: true, stream-bootstrap-buffering: true}, xai: {inject-x-search: true}}\noauth: {providers: {codex: {header-defaults: {beta-features: oauth-beta}}}}\nclient: {codex: {optimize-multi-agent-v2: true, enable-apply-patch: true}}\n";
    for (method, path, body, steering, buffering, optimize, agent) in [
        (
            wreq::Method::PATCH,
            "oauth/providers/codex",
            r#"{"response-steering":false,"header-defaults":{"beta-features":"updated"}}"#,
            false,
            true,
            true,
            "updated",
        ),
        (
            wreq::Method::PUT,
            "oauth/providers/codex",
            r#"{"response-steering":true}"#,
            true,
            false,
            false,
            "",
        ),
        (
            wreq::Method::DELETE,
            "oauth/providers/codex",
            "",
            false,
            false,
            false,
            "",
        ),
        (
            wreq::Method::PUT,
            "upstream/codex",
            r#"{"response-steering":false}"#,
            false,
            false,
            true,
            "oauth-beta",
        ),
    ] {
        let f = Fixture::from_yaml("historical-subtrees", |auth, hash| {
            private_config(raw, auth, hash, false)
        });
        let original = f.file();
        let (base, server) = f.server().await;
        let (status, view) = call(
            &base,
            wreq::Method::GET,
            "/v8/management/config/oauth/providers/codex",
            "",
        )
        .await;
        let view: Value = serde_json::from_str(&view).unwrap();
        assert_eq!(status, 200);
        assert_eq!(
            (&view["response-steering"], &view["optimize-multi-agent-v2"]),
            (&json!(true), &json!(true)),
            "{view}"
        );
        assert_eq!(f.file(), original, "a historical GET changed the file");
        let (status, text) = call(&base, method.clone(), &format!("/v8/management/config/{path}"), body).await;
        assert_eq!(status, 200, "{method} {path}: {text}");
        let file = f.file();
        assert_canonical(&file);
        let cfg = Config::parse(&file).unwrap();
        let d = &cfg.document;
        let flag = |v: &serde_yaml_ng::Value| v.as_bool().unwrap_or(false);
        assert_eq!(
            (
                flag(&d["upstream"]["codex"]["response-steering"]),
                flag(&d["upstream"]["codex"]["stream-bootstrap-buffering"]),
                flag(&d["client"]["codex"]["optimize-multi-agent-v2"]),
                d["oauth"]["providers"]["codex"]["header-defaults"]["beta-features"]
                    .as_str()
                    .unwrap_or(""),
            ),
            (steering, buffering, optimize, agent),
            "{method} {path}: {file}"
        );
        assert!(flag(&d["upstream"]["xai"]["inject-x-search"]) && flag(&d["client"]["codex"]["enable-apply-patch"]));
        let api = cfg.for_api_key();
        assert!(
            api.document["oauth"]["providers"]["codex"]["header-defaults"]["beta-features"]
                .as_str()
                .unwrap_or("")
                .is_empty()
        );
        server.abort();
    }
}

/// Go `TestConfigV8HistoricalConfigurationBodies` and
/// `TestConfigV8HistoricalNullContainerPatch`: whole-config bodies with historical
/// paths are normalized, a canonical value in the same body wins, and a null
/// historical container resets its fields.
#[tokio::test]
async fn config_v8_historical_configuration_bodies() {
    for (method, route, body, steering, buffering) in [
        (
            wreq::Method::PATCH,
            "config",
            r#"{"oauth":{"providers":{"codex":{"response-steering":false}}}}"#,
            false,
            true,
        ),
        (
            wreq::Method::PATCH,
            "config",
            r#"{"oauth":{"providers":{"codex":{"response-steering":true}}},"upstream":{"codex":{"response-steering":false}}}"#,
            false,
            true,
        ),
        (
            wreq::Method::PATCH,
            "config",
            r#"{"oauth":{"providers":{"codex":{"response-steering":true}}},"upstream":{"codex":{"response-steering":null}}}"#,
            false,
            true,
        ),
        (
            wreq::Method::PUT,
            "config",
            r#"{"oauth":{"providers":{"codex":{"response-steering":true}}}}"#,
            true,
            false,
        ),
        (
            wreq::Method::PUT,
            "config.yaml",
            "# Keep this comment\noauth: {providers: {codex: {response-steering: false}}}\n",
            false,
            false,
        ),
    ] {
        let f = Fixture::from_yaml("historical-bodies", |auth, hash| {
            private_config(
                "upstream: {codex: {response-steering: true, stream-bootstrap-buffering: true}}\n",
                auth,
                hash,
                false,
            )
        });
        let (base, server) = f.server().await;
        // Root writes replace the whole file, so the body carries its own access.
        let body = if method == wreq::Method::PUT {
            let auth = f.dir.join("auth");
            let hash = f.rt.config().management.secret_key.clone();
            if route == "config" {
                let mut v: Value = serde_json::from_str(body).unwrap();
                v["management"] = json!({"secret-key": hash});
                v["oauth"]["auth-dir"] = json!(auth.display().to_string());
                v.to_string()
            } else {
                private_config(body, &auth, &hash, false)
            }
        } else {
            body.to_owned()
        };
        let (status, text) = call(&base, method.clone(), &format!("/v8/management/{route}"), &body).await;
        assert_eq!(status, 200, "{body}: {text}");
        let file = f.file();
        assert_canonical(&file);
        let d = Config::parse(&file).unwrap().document;
        let flag = |v: &serde_yaml_ng::Value| v.as_bool().unwrap_or(false);
        assert_eq!(
            (
                flag(&d["upstream"]["codex"]["response-steering"]),
                flag(&d["upstream"]["codex"]["stream-bootstrap-buffering"])
            ),
            (steering, buffering),
            "{body}: {file}"
        );
        if route == "config.yaml" {
            assert!(file.contains("# Keep this comment"), "{file}");
        }
        server.abort();
    }
    let f = Fixture::from_yaml("historical-null", |auth, hash| {
        private_config(
            "upstream: {claude: {model-level-cooling: true, header-defaults: {user-agent: agent, timezone: UTC, stabilize-device-profile: true}}}\n",
            auth,
            hash,
            false,
        )
    });
    let (base, server) = f.server().await;
    let body = r#"{"oauth":{"providers":{"claude":{"header-defaults":null}}}}"#;
    assert_eq!(
        call(&base, wreq::Method::PATCH, "/v8/management/config", body).await.0,
        200
    );
    let d = Config::parse(&f.file()).unwrap().document;
    let claude = &d["upstream"]["claude"];
    for field in ["user-agent", "timezone"] {
        assert!(
            claude["header-defaults"][field].as_str().unwrap_or("").is_empty(),
            "{}",
            f.file()
        );
    }
    assert_eq!(claude["model-level-cooling"], serde_yaml_ng::Value::Bool(true));
    server.abort();

    // Request normalization is not the loader's validation: a missing canonical field
    // replaces a non-mapping ancestor (Go `setYAMLPath`), and a malformed historical
    // container is left for the config parse, which answers 422.
    let f = Fixture::from_yaml("historical-malformed", |auth, hash| {
        private_config(
            "upstream: {codex: {response-steering: true}}
",
            auth,
            hash,
            false,
        )
    });
    let (base, server) = f.server().await;
    let body = r#"{"upstream":null,"oauth":{"providers":{"xai":{"inject-x-search":true}}}}"#;
    let (status, text) = call(&base, wreq::Method::PATCH, "/v8/management/config", body).await;
    assert_eq!(status, 200, "{text}");
    let file = f.file();
    assert_canonical(&file);
    let d = Config::parse(&file).unwrap().document;
    assert_eq!(
        d["upstream"]["xai"]["inject-x-search"],
        serde_yaml_ng::Value::Bool(true),
        "{file}"
    );
    assert_eq!(
        d["upstream"]["codex"]["response-steering"],
        serde_yaml_ng::Value::Bool(true),
        "{file}"
    );
    let (status, text) = call(
        &base,
        wreq::Method::PUT,
        "/v8/management/config/oauth/providers/claude",
        r#""bad""#,
    )
    .await;
    assert_eq!(status, 422, "{text}");
    let err: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(err["error"], "invalid_config", "{text}");
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .ends_with("oauth.providers.claude must be a mapping"),
        "{text}"
    );
    assert_eq!(f.file(), file, "a refused write changed the file");
    server.abort();
}

/// Go `TestLegacyConfigYAMLSavesBySubmittedVersion`: `PUT /v0/management/config.yaml`
/// keeps a legacy upload as submitted and saves a v8 one in the latest layout.
#[tokio::test]
async fn v0_config_yaml_saves_by_submitted_version() {
    for (body, steering, v8) in [
        ("# Keep this comment\ncodex: {response-steering: true}\n", true, false),
        (
            "# Keep this comment\noauth: {providers: {codex: {response-steering: true}}}\n",
            true,
            true,
        ),
        (
            "# Keep this comment\nconfig-version: 8\nupstream: {codex: {response-steering: false}}\n",
            false,
            true,
        ),
        (
            "# Keep this comment\nrequest-retry: 3\nupstream: {codex: {response-steering: false}}\n",
            false,
            true,
        ),
    ] {
        let f = Fixture::new("v0-yaml-version");
        let (base, server) = f.server().await;
        let hash = f.rt.config().management.secret_key.clone();
        let body = private_config(body, &f.dir.join("auth"), &hash, !v8);
        let (status, text) = call(&base, wreq::Method::PUT, "/v0/management/config.yaml", &body).await;
        assert_eq!(status, 200, "{body}: {text}");
        let file = f.file();
        if v8 {
            assert_canonical(&file);
        } else {
            assert_eq!(file, body, "v0 changed the submitted legacy layout");
        }
        assert!(file.contains("# Keep this comment"), "{file}");
        let api = f.rt.config();
        let api = api.for_api_key();
        assert_eq!(
            api.document["upstream"]["codex"]["response-steering"].as_bool(),
            Some(steering),
            "{file}"
        );
        server.abort();
    }
}

/// A v8 YAML upload keeps string settings written as plain numbers or dates as written:
/// the saved file holds `0x1F`, `1.50` and `2001-12-14`, the runtime loads that text,
/// and the JSON view answers what yaml.v3 decodes into `any`, as Go does: the numbers,
/// and the date as a `time.Time`'s RFC 3339 text.
#[tokio::test]
async fn v8_yaml_upload_keeps_string_settings_written_as_numbers() {
    let f = Fixture::new("v8-yaml-literals");
    let (base, server) = f.server().await;
    let hash = f.rt.config().management.secret_key.clone();
    let body = private_config(
        "server: {github-token: 0x1F}\noauth: {model-alias: {codex: [{name: 1.50, alias: 2001-12-14}]}}\n",
        &f.dir.join("auth"),
        &hash,
        false,
    );
    let (status, text) = call(&base, wreq::Method::PUT, "/v8/management/config.yaml", &body).await;
    assert_eq!(status, 200, "{text}");
    let file = f.file();
    assert!(
        file.contains("github-token: 0x1F") && file.contains("name: 1.50") && file.contains("alias: 2001-12-14"),
        "{file}"
    );
    let cfg = cpa_core::config::Config::parse(&file).unwrap();
    assert_eq!(cfg.document["server"]["github-token"].as_str(), Some("0x1F"));
    assert_eq!(
        cfg.document["oauth"]["model-alias"]["codex"][0]["name"].as_str(),
        Some("1.50")
    );
    assert_eq!(
        cfg.document["oauth"]["model-alias"]["codex"][0]["alias"].as_str(),
        Some("2001-12-14")
    );
    let (_, view) = call(&base, wreq::Method::GET, "/v8/management/config", "").await;
    let view: serde_json::Value = serde_json::from_str(&view).unwrap();
    assert_eq!(view["server"]["github-token"], serde_json::json!(31), "{view}");
    assert_eq!(
        view["oauth"]["model-alias"]["codex"][0]["name"],
        serde_json::json!(1.5),
        "{view}"
    );
    assert_eq!(
        view["oauth"]["model-alias"]["codex"][0]["alias"],
        serde_json::json!("2001-12-14T00:00:00Z"),
        "{view}"
    );
    server.abort();
}

/// A JSON number written into a string setting saves as Go saves the body's node, its
/// text: `1.50` and `1e3` load as written, not as 1.5 and 1000.0. One the save writes
/// back as it is (`5`) stays a number in the file and the views.
#[tokio::test]
async fn json_numbers_in_string_settings_keep_their_text() {
    let f = Fixture::new("json-literals");
    let (base, server) = f.server().await;
    let (status, text) = call(
        &base,
        wreq::Method::PUT,
        "/v8/management/config/server/github-token",
        "1.50",
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let body = r#"{"oauth":{"model-alias":{"codex":[{"name":1e3,"alias":"b"}]}}}"#;
    let (status, text) = call(&base, wreq::Method::PATCH, "/v8/management/config", body).await;
    assert_eq!(status, 200, "{text}");
    let file = f.file();
    let cfg = cpa_core::config::Config::parse(&file).unwrap();
    assert_eq!(cfg.document["server"]["github-token"].as_str(), Some("1.50"), "{file}");
    assert_eq!(
        cfg.document["oauth"]["model-alias"]["codex"][0]["name"].as_str(),
        Some("1e3"),
        "{file}"
    );
    // A JSON `<<` key is a merge for Go, which saves it plain; its number keeps the text.
    let body = r#"{"oauth":{"model-alias":{"codex":[{"<<":{"name":1.50},"alias":"b"}]}}}"#;
    let (status, text) = call(&base, wreq::Method::PATCH, "/v8/management/config", body).await;
    assert_eq!(status, 200, "{text}");
    let file = f.file();
    let cfg = cpa_core::config::Config::parse(&file).unwrap();
    assert_eq!(
        cfg.document["oauth"]["model-alias"]["codex"][0]["name"].as_str(),
        Some("1.50"),
        "{file}"
    );
    let (status, text) = call(
        &base,
        wreq::Method::PUT,
        "/v8/management/config/server/github-token",
        "5",
    )
    .await;
    assert_eq!(status, 200, "{text}");
    assert!(f.file().contains("github-token: 5\n"), "{}", f.file());
    // An integer beyond 64 bits, which Go reads as its text too.
    let big = "123456789012345678901234567890";
    let (status, text) = call(
        &base,
        wreq::Method::PUT,
        "/v8/management/config/server/github-token",
        big,
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let cfg = cpa_core::config::Config::parse(&f.file()).unwrap();
    assert_eq!(
        cfg.document["server"]["github-token"].as_str(),
        Some(big),
        "{}",
        f.file()
    );
    let (status, text) = call(
        &base,
        wreq::Method::PUT,
        "/v8/management/config/server/github-token",
        "5",
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let (_, view) = call(
        &base,
        wreq::Method::GET,
        "/v8/management/config/server/github-token",
        "",
    )
    .await;
    assert_eq!(view, "5");
    server.abort();
}

/// The v8 JSON views write a YAML float as Go's `json.Marshal` writes a float64.
#[tokio::test]
async fn v8_views_write_floats_as_go() {
    let f = Fixture::from_yaml("view-floats", |auth, hash| {
        format!(
            "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\nrequests:\n  payload:\n    override:\n      - models: [{{name: m}}]\n        params: {{a: 1.0, b: -0.0, c: 1e20, d: 1.5, e: 1e21, f: 0.0000001, g: 7}}\n",
            auth.display()
        )
    });
    let (base, server) = f.server().await;
    let (status, view) = call(
        &base,
        wreq::Method::GET,
        "/v8/management/config/requests/payload/override",
        "",
    )
    .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(
        view,
        r#"[{"models":[{"name":"m"}],"params":{"a":1,"b":-0,"c":100000000000000000000,"d":1.5,"e":1e+21,"f":1e-7,"g":7}}]"#
    );
    server.abort();
}

/// v0 saves of a legacy file keep the text of its `!!binary` values (an alias of one
/// included), and the runtime holds their decoded text.
#[tokio::test]
async fn v0_writes_keep_binary_values() {
    let f = Fixture::new("v0-binary");
    let hash = f.rt.config().management.secret_key.clone();
    let legacy = format!(
        "port: 0\nremote-management:\n  secret-key: '{hash}'\nauth-dir: {}\nproxy-url: !!binary aHR0cDovL3guZXhhbXBsZTo4MA==\napi-keys: [fake-client, &k !!binary ZmFrZS1iaW5hcnk=, *k]\nrequest-retry: 1\n",
        f.dir.join("auth").display()
    );
    std::fs::write(f.dir.join("config.yaml"), &legacy).unwrap();
    let (base, server) = f.server().await;
    let (status, text) = call(
        &base,
        wreq::Method::PUT,
        "/v0/management/request-retry",
        r#"{"value":2}"#,
    )
    .await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(f.file(), legacy.replace("request-retry: 1", "request-retry: 2"));
    let cfg = f.rt.config();
    assert_eq!(cfg.document["requests"]["proxy-url"], "http://x.example:80");
    assert_eq!(
        cfg.document["access"]["api-keys"],
        serde_yaml_ng::from_str::<serde_yaml_ng::Value>("[fake-client, fake-binary, fake-binary]").unwrap()
    );
    server.abort();
}

/// Go `TestV0SetterSavesByExistingVersion` (v8 cases), `TestV0SaveUpgradesHistoricalV8Layout`
/// and `TestV8MovedCommentsSurviveV0Saves`: a v0 setter saves an existing v8 file in the
/// latest layout, and the comments of a moved historical field (and of the ancestors
/// it leaves empty) survive repeated saves exactly once. Legacy-only files:
/// [`v0_setters_keep_a_legacy_layout`].
#[tokio::test]
async fn v0_setters_save_v8_files_in_the_latest_layout() {
    for raw in [
        "config-version: 8\n# Preserve provider settings\noauth: {providers: {codex: {response-steering: true, header-defaults: {beta-features: oauth-beta}}, xai: {inject-x-search: true}}}\n",
        "# Preserve provider settings\noauth: {providers: {codex: {response-steering: true, header-defaults: {beta-features: oauth-beta}}, xai: {inject-x-search: true}}}\n",
        "config-version: 8\nupstream: {codex: {response-steering: true}, xai: {inject-x-search: true}}\nrouting: {retry: {request-retry: 3}}\n",
    ] {
        let f = Fixture::from_yaml("v0-existing-version", |auth, hash| {
            private_config(raw, auth, hash, false)
        });
        let (base, server) = f.server().await;
        let (status, text) = call(
            &base,
            wreq::Method::PUT,
            "/v0/management/request-retry",
            r#"{"value":2}"#,
        )
        .await;
        assert_eq!(status, 200, "{text}");
        let file = f.file();
        assert_canonical(&file);
        let cfg = Config::parse(&file).unwrap();
        let api = cfg.for_api_key();
        assert_eq!(api.document["routing"]["retry"]["request-retry"], 2);
        assert_eq!(
            api.document["upstream"]["codex"]["response-steering"],
            serde_yaml_ng::Value::Bool(true)
        );
        assert_eq!(
            api.document["upstream"]["xai"]["inject-x-search"],
            serde_yaml_ng::Value::Bool(true)
        );
        if raw.contains("oauth-beta") {
            // The Codex header defaults stay OAuth-only through the save.
            let ua = |d: &serde_yaml_ng::Value| {
                d["oauth"]["providers"]["codex"]["header-defaults"]["beta-features"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned()
            };
            assert_eq!(
                (ua(&cfg.document), ua(&api.document)),
                ("oauth-beta".to_owned(), String::new()),
                "{file}"
            );
        }
        if raw.contains('#') {
            assert!(file.contains("# Preserve provider settings"), "{file}");
        }
        server.abort();
    }
    let raw = "# DOCUMENT HEAD\nconfig-version: 8\noauth: # OAUTH INLINE\n  providers: # PROVIDERS INLINE\n    xai: # PROVIDER INLINE\n      # FIELD HEAD\n      inject-x-search: true # FIELD INLINE\n\n      # FIELD FOOT\n\n    # PROVIDER FOOT\nserver:\n  port: 8317 # UNRELATED INLINE\n# DOCUMENT FOOT\n";
    let f = Fixture::from_yaml("v0-moved-comments", |auth, hash| {
        let raw = raw.replacen(
            "oauth: # OAUTH INLINE\n",
            &format!("oauth: # OAUTH INLINE\n  auth-dir: {}\n", auth.display()),
            1,
        );
        format!("management: {{secret-key: '{hash}'}}\n{raw}")
    });
    let (base, server) = f.server().await;
    for (method, retry) in [(wreq::Method::PUT, 2), (wreq::Method::PATCH, 1), (wreq::Method::PUT, 0)] {
        let (status, body) = call(
            &base,
            method,
            "/v0/management/request-retry",
            &format!(r#"{{"value":{retry}}}"#),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let file = f.file();
        assert_canonical(&file);
        assert_eq!(
            Config::parse(&file).unwrap().document["routing"]["retry"]["request-retry"],
            retry,
            "{file}"
        );
        for marker in [
            "DOCUMENT HEAD",
            "OAUTH INLINE",
            "PROVIDERS INLINE",
            "PROVIDER INLINE",
            "FIELD HEAD",
            "FIELD INLINE",
            "FIELD FOOT",
            "PROVIDER FOOT",
            "UNRELATED INLINE",
            "DOCUMENT FOOT",
        ] {
            assert_eq!(file.matches(marker).count(), 1, "{marker}: {file}");
        }
    }
    server.abort();
}

/// Go `TestConfigV8FieldUpdatesKeepComments`: scalar field updates through the
/// historical or canonical path keep the field's and its section's comments.
#[tokio::test]
async fn config_v8_field_updates_keep_comments() {
    for method in [wreq::Method::PUT, wreq::Method::PATCH] {
        for path in [
            "oauth/providers/codex/response-steering",
            "upstream/codex/response-steering",
        ] {
            let f = Fixture::from_yaml("field-comments", |auth, hash| {
                format!(
                    "management: {{secret-key: '{hash}'}}\noauth:\n  auth-dir: {}\n  providers:\n    codex: # PROVIDER INLINE\n      # FIELD HEAD\n      response-steering: true # FIELD INLINE\n\n      # FIELD FOOT\n",
                    auth.display()
                )
            });
            let (base, server) = f.server().await;
            for body in ["false", "null", "true"] {
                let (status, text) = call(&base, method.clone(), &format!("/v8/management/config/{path}"), body).await;
                assert_eq!(status, 200, "{text}");
                let file = f.file();
                for marker in ["PROVIDER INLINE", "FIELD HEAD", "FIELD INLINE", "FIELD FOOT"] {
                    assert_eq!(
                        file.matches(marker).count(),
                        1,
                        "{method} {path} {body} {marker}: {file}"
                    );
                }
            }
            server.abort();
        }
    }
}

/// Go `TestV0SetterSavesByExistingVersion` (legacy case) and
/// `TestV0RepeatedSavesKeepLegacyClientAlias`: a v0 setter on a file without v8
/// sections keeps the legacy layout and its comments, so every setting keeps its
/// meaning; a legacy `codex-header-defaults` still applies to API-key credentials.
#[tokio::test]
async fn v0_setters_keep_a_legacy_layout() {
    for raw in [
        "request-retry: 3\ncodex: {response-steering: true}\nxai: {inject-x-search: true}\nrouting: {strategy: fill-first}\n",
        "request-retry: 3\ncodex: {response-steering: true}\nxai: {inject-x-search: true}\ncodex-header-defaults: {beta-features: legacy-beta}\n",
    ] {
        let f = Fixture::from_yaml("v0-legacy-version", |auth, hash| private_config(raw, auth, hash, true));
        let (base, server) = f.server().await;
        let (status, text) = call(
            &base,
            wreq::Method::PUT,
            "/v0/management/request-retry",
            r#"{"value":2}"#,
        )
        .await;
        assert_eq!(status, 200, "{text}");
        let file = f.file();
        assert!(
            !file.contains("config-version:") && !file.contains("upstream:"),
            "v0 migrated a legacy-only file: {file}"
        );
        let cfg = Config::parse(&file).unwrap();
        let api = cfg.for_api_key();
        let d = &api.document;
        assert_eq!(d["routing"]["retry"]["request-retry"], 2, "{file}");
        assert_eq!(
            d["upstream"]["codex"]["response-steering"],
            serde_yaml_ng::Value::Bool(true),
            "{file}"
        );
        assert_eq!(
            d["upstream"]["xai"]["inject-x-search"],
            serde_yaml_ng::Value::Bool(true),
            "{file}"
        );
        if raw.contains("legacy-beta") {
            let ua = &d["oauth"]["providers"]["codex"]["header-defaults"]["beta-features"];
            assert_eq!(
                ua.as_str(),
                Some("legacy-beta"),
                "API keys lost the legacy header default: {file}"
            );
        }
        server.abort();
    }
    let raw = "# LEGACY DOCUMENT\ncodex:\n  # OPTIMIZE HEAD\n  optimize-multi-agent-v2: true # OPTIMIZE INLINE\n  response-steering: true\nrequest-retry: 3\n";
    let f = Fixture::from_yaml("v0-legacy-client", |auth, hash| private_config(raw, auth, hash, true));
    let (base, server) = f.server().await;
    for retry in [2, 1, 0] {
        let body = format!(r#"{{"value":{retry}}}"#);
        let (status, text) = call(&base, wreq::Method::PUT, "/v0/management/request-retry", &body).await;
        assert_eq!(status, 200, "{text}");
        let file = f.file();
        for absent in ["config-version:", "upstream:", "client:"] {
            assert!(!file.contains(absent), "v0 save changed the legacy layout: {file}");
        }
        assert!(file.contains("optimize-multi-agent-v2: true"), "{file}");
        for marker in ["LEGACY DOCUMENT", "OPTIMIZE HEAD", "OPTIMIZE INLINE"] {
            assert_eq!(file.matches(marker).count(), 1, "{marker}: {file}");
        }
        let cfg = Config::parse(&file).unwrap();
        let api = cfg.for_api_key();
        assert_eq!(api.document["routing"]["retry"]["request-retry"], retry, "{file}");
        assert_eq!(
            api.document["client"]["codex"]["optimize-multi-agent-v2"],
            serde_yaml_ng::Value::Bool(true),
            "{file}"
        );
        assert_eq!(
            api.document["upstream"]["codex"]["response-steering"],
            serde_yaml_ng::Value::Bool(true),
            "{file}"
        );
    }
    server.abort();
}

/// A v0 legacy save works on Go's flattened tree: an inherited (`<<`) setting can be
/// deleted, and a setter below an inherited parent keeps its untouched siblings. A key
/// edited into the file as plaintext is saved and published hashed, whether the save
/// sets, deletes or keeps a value, as Go saves its in-memory, hashed config.
#[tokio::test]
async fn v0_legacy_saves_expand_merges_and_hash_the_key() {
    let merged = |name: &str| {
        Fixture::from_yaml(name, |auth, hash| {
            private_config(
                "<<:\n  oauth-excluded-models:\n    codex: [blocked-model]\n  quota-exceeded:\n    switch-project: true\n    switch-preview-model: true\n",
                auth,
                hash,
                true,
            )
        })
    };
    let f = merged("v0-legacy-merge-delete");
    let (base, server) = f.server().await;
    let excluded = |f: &Fixture| {
        f.rt.config().document["oauth"]["excluded-models"]
            .get("codex")
            .is_some()
    };
    assert!(excluded(&f));
    let (status, text) = call(
        &base,
        wreq::Method::DELETE,
        "/v0/management/oauth-excluded-models?provider=codex",
        "",
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let file = f.file();
    assert!(
        !file.contains("blocked-model") && !file.contains("config-version:"),
        "{file}"
    );
    assert!(!excluded(&f), "{file}");
    server.abort();

    // A fresh merged file: the edit lands under the inherited parent itself.
    let f = merged("v0-legacy-merge-sibling");
    let (base, server) = f.server().await;
    let (status, text) = call(
        &base,
        wreq::Method::PATCH,
        "/v0/management/quota-exceeded/switch-project",
        r#"{"value":false}"#,
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let file = f.file();
    let d = Config::parse(&file).unwrap().document;
    assert_eq!(
        d["quota-exceeded"]["switch-project"],
        serde_yaml_ng::Value::Bool(false),
        "{file}"
    );
    assert_eq!(
        d["quota-exceeded"]["switch-preview-model"],
        serde_yaml_ng::Value::Bool(true),
        "{file}"
    );
    assert!(file.contains("blocked-model"), "{file}");
    server.abort();

    for (name, method, route, body) in [
        (
            "set",
            wreq::Method::PUT,
            "/v0/management/request-retry",
            r#"{"value":2}"#,
        ),
        (
            "delete",
            wreq::Method::DELETE,
            "/v0/management/oauth-excluded-models?provider=codex",
            "",
        ),
        (
            "keep",
            wreq::Method::PUT,
            "/v0/management/oauth-excluded-models",
            r#"{"codex":["blocked-model"]}"#,
        ),
    ] {
        let f = Fixture::from_yaml(&format!("v0-legacy-secret-{name}"), |auth, hash| {
            private_config(
                "request-retry: 3\noauth-excluded-models:\n  codex: [blocked-model]\n",
                auth,
                hash,
                true,
            )
        });
        let (base, server) = f.server().await;
        // Hand-edited after startup, before any reload (the fixture runs no watcher).
        let hash = f.rt.config().management.secret_key.clone();
        std::fs::write(
            f.dir.join("config.yaml"),
            f.file().replace(&hash, "fake-management-only"),
        )
        .unwrap();
        let (status, text) = call(&base, method, route, body).await;
        assert_eq!(status, 200, "{name}: {text}");
        let file = f.file();
        assert_eq!(file.contains("blocked-model"), name != "delete", "{name}: {file}");
        let saved = Config::parse(&file).unwrap().management.secret_key;
        assert!(
            bcrypt::verify("fake-management-only", &saved).unwrap(),
            "{name}: {file}"
        );
        let live = f.rt.config().management.secret_key.clone();
        assert!(bcrypt::verify("fake-management-only", &live).unwrap(), "{name}: {live}");
        assert_eq!(
            call(&base, wreq::Method::GET, "/v0/management/request-retry", "")
                .await
                .0,
            200,
            "{name}"
        );
        server.abort();
    }
}

/// Go `ForceRefreshAuth` and `ForceRefreshAll` (`authHasRefreshCredential`): a Codex
/// credential whose only refresh credential is its Sign in with ChatGPT partition
/// refreshes by name and in bulk.
#[tokio::test]
async fn siwc_only_codex_credentials_refresh_by_name_and_in_bulk() {
    let forms = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let app = axum::Router::new().route(
        "/api/accounts/oauth/token",
        axum::routing::post({
            let forms = forms.clone();
            move |body: String| async move {
                forms.lock().unwrap().push(body);
                r#"{"access_token":"siwc-new","refresh_token":"siwc-rt-new","scope":"openid chatgpt.tokens.use.direct","expires_in":3600}"#
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let token_url = format!("http://{}/api/accounts/oauth/token", listener.local_addr().unwrap());
    let upstream = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let oauth = cpa_exec::codex_oauth::CodexOAuth::with_endpoints(wreq::Client::new(), "http://127.0.0.1:1", "", "")
        .with_siwc_token_url(&token_url)
        .with_retry_delay(std::time::Duration::from_millis(1));
    let codex = cpa_exec::codex::CodexExecutor::with_client(wreq::Client::new(), oauth);
    let f = Fixture::from_yaml_with(
        "siwc-refresh",
        |auth, hash| {
            let siwc = |rt: &str, active: bool| {
                // A loopback base URL lets the test runtime's guard reach the token mock.
                json!({"type": "codex", "use_siwc": active, "access_token": "outer-at", "base_url": "http://127.0.0.1:1",
                       "siwc": {"access_token": "siwc-old", "refresh_token": rt, "client_id": "client-1",
                                "expires_at": "2000-01-01T00:00:00Z"}})
                .to_string()
            };
            std::fs::write(auth.join("active.json"), siwc("siwc-rt-1", true)).unwrap();
            std::fs::write(auth.join("bound.json"), siwc("siwc-rt-2", false)).unwrap();
            std::fs::write(auth.join("plain.json"), r#"{"type":"codex","access_token":"a"}"#).unwrap();
            format!(
                "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\n",
                auth.display()
            )
        },
        codex,
    );
    let (base, server) = f.server().await;
    let refresh = |body: &'static str| {
        let request = wreq::Client::new()
            .post(format!("{base}/v8/management/credentials/refresh"))
            .bearer_auth("fake-management-only")
            .header("content-type", "application/json")
            .body(body);
        async move {
            let response = request.send().await.unwrap();
            (response.status().as_u16(), response.json::<Value>().await.unwrap())
        }
    };
    let token = |name: &str| {
        let c =
            f.rt.store()
                .snapshot()
                .into_iter()
                .find(|c| c.id.ends_with(name))
                .unwrap();
        c.metadata["siwc"]["access_token"].as_str().unwrap().to_owned()
    };
    let refreshed_with = |forms: &[String]| {
        let mut tokens: Vec<String> = forms
            .iter()
            .map(|f| {
                url::form_urlencoded::parse(f.as_bytes())
                    .find(|(k, _)| k == "refresh_token")
                    .unwrap()
                    .1
                    .into_owned()
            })
            .collect();
        tokens.sort();
        tokens
    };

    let (status, body) = refresh(r#"{"name":"active.json"}"#).await;
    assert_eq!((status, body["ok"].clone()), (200, json!(true)), "{body}");
    assert_eq!(token("active.json"), "siwc-new");
    assert_eq!(
        refreshed_with(&forms.lock().unwrap().drain(..).collect::<Vec<_>>()),
        ["siwc-rt-1"]
    );

    let (status, body) = refresh(r#"{"all":true}"#).await;
    assert_eq!(status, 200, "{body}");
    let mut results: Vec<(String, bool)> = body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["id"].as_str().unwrap().rsplit('/').next().unwrap().to_owned(),
                r["success"] == true,
            )
        })
        .collect();
    results.sort();
    assert_eq!(
        results,
        [("active.json".to_owned(), true), ("bound.json".to_owned(), true)],
        "plain.json has nothing to refresh"
    );
    assert_eq!(token("bound.json"), "siwc-new");
    assert_eq!(
        refreshed_with(&forms.lock().unwrap().drain(..).collect::<Vec<_>>()),
        ["siwc-rt-2", "siwc-rt-new"]
    );
    // #91: both refresh filters must retain the real grant expiry with the rotated
    // tokens. Reload the persisted credential so status cannot rely on memory alone.
    for name in ["active.json", "bound.json"] {
        let path = f.dir.join("auth").join(name);
        let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let credential =
            cpa_core::credential::Credential::from_file(&f.dir.join("auth"), &path, saved.as_object().unwrap().clone())
                .unwrap();
        let expected_status = if name == "active.json" { "valid" } else { "off" };
        assert_eq!(
            cpa_exec::codex_siwc::status(&credential, false, chrono::Utc::now()),
            expected_status,
            "{name}: persisted refresh must replace the original grant expiry"
        );
        let expiry = chrono::DateTime::parse_from_rfc3339(saved["siwc"]["expires_at"].as_str().unwrap()).unwrap();
        assert!((expiry.to_utc() - chrono::Utc::now()).num_seconds() > 3500);
        let loaded =
            f.rt.store()
                .snapshot()
                .into_iter()
                .find(|c| c.id.ends_with(name))
                .unwrap();
        assert_eq!(loaded.metadata["siwc"]["expires_at"], saved["siwc"]["expires_at"]);
        let response = wreq::Client::new()
            .get(format!("{base}/v8/management/credentials/siwc/status?name={name}"))
            .bearer_auth("fake-management-only")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(
            response.json::<Value>().await.unwrap(),
            json!({"status": expected_status})
        );
    }
    server.abort();
    upstream.abort();
}

#[tokio::test]
async fn retired_quota_probe_refuses_load_upload_and_field_edits() {
    let legacy = br#"{"type":"codex","quota_probe":{"kind":"fake"},"unknown":{"keep":true}}"#;
    let f = Fixture::from_yaml("quota-probe-removed", |auth, hash| {
        std::fs::write(auth.join("legacy.json"), legacy).unwrap();
        std::fs::write(
            auth.join("active.json"),
            br#"{"type":"codex","unknown":{"quota_probe":"keep"}}"#,
        )
        .unwrap();
        format!(
            "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\n",
            auth.display()
        )
    });
    assert!(f.rt.store().get("legacy.json").is_none());
    let before = f.rt.store().get("active.json").unwrap();
    let auth = f.dir.join("auth");
    let unchanged = std::fs::read(auth.join("active.json")).unwrap();
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    for value in [json!({"kind":"fake"}), Value::Null, json!(false)] {
        let response = client
            .post(format!("{base}/v8/management/credentials?name=uploaded.json"))
            .bearer_auth("fake-management-only")
            .json(&json!({"type":"codex", "quota_probe":value}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        let body: Value = response.json().await.unwrap();
        assert!(body["error"].as_str().unwrap().contains("quota_probe"));
        assert!(body["error"].as_str().unwrap().contains("requests/api-call"));
        assert!(!auth.join("uploaded.json").exists());
    }
    for field in ["quota_probe", "quota_probe.url", " quota_probe . url "] {
        let response = client
            .patch(format!("{base}/v8/management/credentials/fields"))
            .bearer_auth("fake-management-only")
            .json(&json!({"name":"active.json", field:"fake-value"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        let body: Value = response.json().await.unwrap();
        assert!(body["error"].as_str().unwrap().contains("quota_probe"));
        assert_eq!(std::fs::read(auth.join("active.json")).unwrap(), unchanged);
        assert_eq!(f.rt.store().get("active.json").unwrap().revision, before.revision);
    }
    assert_eq!(std::fs::read(auth.join("legacy.json")).unwrap(), legacy);
    assert_eq!(before.metadata["unknown"]["quota_probe"], "keep");
    server.abort();
}

/// Native plugins are removed (docs/DIFFERENCES-FROM-GO.md "Native plugins"): every
/// plugin, plugin store and plugin quota route answers `410 plugins_removed` behind the
/// management guard, plugin resources answer it without one, the API reports no plugin
/// support, and the v0 config view has no `plugins` section.
#[tokio::test]
async fn retired_plugin_routes_answer_gone_behind_the_guard() {
    let f = Fixture::new("plugins-removed");
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    for (method, path) in [
        (wreq::Method::GET, "/v8/management/plugins"),
        (wreq::Method::DELETE, "/v8/management/plugins/acme"),
        (wreq::Method::GET, "/v8/management/plugins/store"),
        (wreq::Method::POST, "/v8/management/plugins/store/acme/install"),
        (wreq::Method::POST, "/v8/management/plugins/acme/quota"),
        (wreq::Method::GET, "/v0/management/plugins"),
        (wreq::Method::PATCH, "/v0/management/plugins/acme/enabled"),
        (wreq::Method::PUT, "/v0/management/plugins/acme/config"),
        (wreq::Method::POST, "/v0/management/plugins/acme/quota/reset"),
        (wreq::Method::GET, "/v0/management/plugin-store"),
        (wreq::Method::POST, "/v0/management/plugin-store/acme/install"),
        (wreq::Method::GET, "/v0/management/quota/providers"),
        (wreq::Method::POST, "/v0/management/quota/fetch"),
        (wreq::Method::POST, "/v0/management/quota/reset"),
    ] {
        let anonymous = client
            .request(method.clone(), format!("{base}{path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(anonymous.status().as_u16(), 401, "{method} {path}: guarded");
        let r = client
            .request(method.clone(), format!("{base}{path}"))
            .bearer_auth("fake-management-only")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 410, "{method} {path}");
        let body: Value = r.json().await.unwrap();
        assert_eq!(body["error"], "plugins_removed", "{method} {path}");
        assert!(
            body["message"].as_str().unwrap().contains("MIGRATING-FROM-GO.md"),
            "{body}"
        );
    }
    let r = client
        .get(format!("{base}/v0/resource/plugins/acme/app.js"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 410);
    // Paths that only resemble the retired ones keep gin's bare 404.
    for path in ["/v8/management/quota/fetch", "/v8/management/plugin-store"] {
        let r = client
            .get(format!("{base}{path}"))
            .bearer_auth("fake-management-only")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 404, "{path}");
    }
    let r = client
        .get(format!("{base}/v0/management/config"))
        .bearer_auth("fake-management-only")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.headers()["x-cpa-support-plugin"], "0");
    let view: Value = r.json().await.unwrap();
    assert!(view.get("plugins").is_none(), "{view}");
    server.abort();
}

/// A management write that would add a `plugins` section is refused with the
/// migration error and leaves the file and the running config as they were.
#[tokio::test]
async fn management_writes_refuse_a_plugins_section() {
    let f = Fixture::new("plugins-write");
    let (base, server) = f.server().await;
    let before = f.file();
    for (method, path, body) in [
        (
            wreq::Method::PUT,
            "/v8/management/config/plugins",
            r#"{"enabled":false}"#,
        ),
        (
            wreq::Method::PATCH,
            "/v8/management/config",
            r#"{"plugins":{"enabled":true,"dir":"plugins"}}"#,
        ),
        (
            wreq::Method::PUT,
            "/v8/management/config.yaml",
            "config-version: 8\nplugins:\n  enabled: false\n",
        ),
    ] {
        let (status, text) = call(&base, method.clone(), path, body).await;
        assert_eq!(status, 422, "{method} {path}: {text}");
        assert!(text.contains(r#""error":"invalid_config""#), "{method} {path}: {text}");
        assert!(
            text.contains("plugins section is not supported"),
            "{method} {path}: {text}"
        );
        assert_eq!(f.file(), before, "{method} {path} changed the file");
    }
    assert!(f.rt.config().document.get("plugins").is_none());
    server.abort();
}

/// The v0 spellings of the OAuth status and cancel routes stay built in: an empty state
/// is ok, an unknown one is reported, and a cancel needs a state.
#[tokio::test]
async fn v0_oauth_status_and_session_routes_answer_without_plugins() {
    let f = Fixture::new("v0-oauth-routes");
    let (base, server) = f.server().await;
    let (status, body) = call(&base, wreq::Method::GET, "/v0/management/get-auth-status", "").await;
    assert_eq!((status, body.contains(r#""status":"ok""#)), (200, true), "{body}");
    let state = "a".repeat(64);
    let (status, body) = call(
        &base,
        wreq::Method::GET,
        &format!("/v0/management/get-auth-status?state={state}"),
        "",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("unknown or expired state"), "{body}");
    let (status, body) = call(&base, wreq::Method::DELETE, "/v0/management/oauth-session", "").await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("missing state"), "{body}");
    server.abort();
}
