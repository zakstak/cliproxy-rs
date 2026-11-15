//! Independent contracts for supported v0 management APIs and their v8 aliases.
//! Typed request tables verify configuration persistence and failure atomicity.

use std::net::SocketAddr;
use std::sync::Arc;

use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use cpa_server::management::{self, Management, Options};
use serde_json::Value;

/// Keep every test away from the operator's real credential directory.
fn with_auth_dir(yaml: &str, dir: &std::path::Path) -> String {
    if yaml.lines().any(|l| l.trim_start().starts_with("auth-dir:")) {
        return yaml.to_owned();
    }
    let auth = dir.join("auth");
    std::fs::create_dir_all(&auth).unwrap();
    let mut out = yaml.to_owned();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!("auth-dir: '{}'\n", auth.display()));
    out
}

async fn serve(path: &std::path::Path) -> (String, tokio::task::JoinHandle<()>) {
    // A private auth-dir, so a config without one never reads real credentials.
    let auth = path.parent().unwrap().join("auth");
    let mut config = Config::load(path).unwrap();
    config.auth_dir = auth.clone();
    // Load configured keys as credentials, as the binary does at startup.
    let credentials = cpa_core::config::credentials::load(&config);
    let rt = Arc::new(cpa_server::testing::runtime(
        config,
        credentials,
        Executors {
            claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            meta: Default::default(),
            xai: Default::default(),
        },
    ));
    let state = Management::with_options(
        rt,
        path.to_path_buf(),
        Options {
            management_password: Some(String::new()),
            auth_dir: Some(auth),
            // Sign-in flows that start a device login hit this closed port, never a provider.
            login_base: Some("http://127.0.0.1:1".into()),
            ..Options::default()
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = management::router(state).layer(axum::middleware::from_fn(
        |mut req: axum::extract::Request, next: axum::middleware::Next| async move {
            req.extensions_mut()
                .insert(axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 1))));
            next.run(req).await
        },
    ));
    (
        base,
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
    )
}

/// v0 aliases must answer exactly like the v8 routes they wrap, and list entries carry
/// the same `auth_index` the credentials listing reports for that key.
#[tokio::test]
async fn v0_aliases_match_v8_and_lists_carry_live_auth_indexes() {
    let hash = bcrypt::hash("fake-secret", 4).unwrap();
    let dir = std::env::temp_dir().join(format!("cpa-legacy-alias-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.yaml");
    let yaml = "config-version: 8\nmanagement:\n  secret-key: '$HASH'\napi-keys:\n  claude:\n    - name: g\n      keys: [{api-key: fake-g1}, {api-key: fake-g2}]\n  codex:\n    - name: v\n      base-url: http://127.0.0.1:9/v1\n      keys: [{api-key: fake-v1}]\n";
    std::fs::write(&path, with_auth_dir(&yaml.replace("$HASH", &hash), &dir)).unwrap();
    let (base, server) = serve(&path).await;
    let client = wreq::Client::new();
    let get = |method: &str, path: &str| {
        client
            .request(method.parse::<wreq::Method>().unwrap(), format!("{base}{path}"))
            .bearer_auth("fake-secret")
            .send()
    };
    // Each sign-in answers a fresh state and PKCE challenge; the rest of the answer
    // matches.
    let mut started = Vec::new();
    for path in [
        "/v0/management/anthropic-auth-url",
        "/v8/management/oauth/auth-url?provider=claude",
    ] {
        let v: Value = get("GET", path).await.unwrap().json().await.unwrap();
        let url = v["url"].as_str().unwrap();
        started.push((v["status"].clone(), url.split('?').next().unwrap().to_owned()));
    }
    assert_eq!(started[0], started[1]);
    assert_eq!(started[0].0, "ok");
    for (v0, v8) in [
        (
            "/v0/management/xai-auth-url?is_webui=true",
            "/v8/management/oauth/auth-url?provider=xai&is_webui=true",
        ),
        // A removed provider's login: the same migration error on both (issue #49).
        (
            "/v0/management/kimi-auth-url",
            "/v8/management/oauth/auth-url?provider=kimi",
        ),
        ("/v0/management/logs", "/v8/management/observability/logs"),
        (
            "/v0/management/request-error-logs",
            "/v8/management/observability/logs/errors",
        ),
        (
            "/v0/management/request-log-by-id/nope",
            "/v8/management/observability/logs/requests/nope",
        ),
    ] {
        let (a, b) = (get("GET", v0).await.unwrap(), get("GET", v8).await.unwrap());
        assert_eq!(a.status(), b.status(), "{v0}");
        assert_eq!(a.text().await.unwrap(), b.text().await.unwrap(), "{v0}");
    }
    let (a, b) = (
        get("POST", "/v0/management/vertex/import").await.unwrap(),
        get("POST", "/v8/management/oauth/import?provider=vertex")
            .await
            .unwrap(),
    );
    assert_eq!(
        (a.status(), a.text().await.unwrap()),
        (b.status(), b.text().await.unwrap())
    );

    let lists: Value = get("GET", "/v0/management/claude-api-key")
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entries = lists["claude-api-key"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    let config = Config::load(&path).unwrap();
    let expected = |key: &str| {
        let creds = cpa_core::config::credentials::from_config(&config);
        let c = creds
            .iter()
            .find(|c| c.attributes.get("api_key").map(String::as_str) == Some(key))
            .unwrap_or_else(|| panic!("credential for {key}"));
        cpa_core::config::credentials::auth_index(c)
    };
    for (entry, key) in entries.iter().zip(["fake-g1", "fake-g2"]) {
        assert_eq!(entry["auth-index"].as_str(), Some(expected(key).as_str()), "{entry}");
    }
    let codex: Value = get("GET", "/v0/management/codex-api-key")
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        codex["codex-api-key"][0]["auth-index"].as_str(),
        Some(expected("fake-v1").as_str())
    );
    server.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Failed writes return a useful error, and unchanged groups keep their comments.
#[tokio::test]
async fn v0_writes_preserve_comments_and_report_failed_saves() {
    let hash = bcrypt::hash("fake-secret", 4).unwrap();
    let dir = std::env::temp_dir().join(format!("cpa-legacy-save-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.yaml");
    let yaml = format!(
        "config-version: 8\nmanagement:\n  secret-key: '{hash}'\napi-keys:\n  claude:\n    # keep this comment\n    - name: c\n      base-url: https://c.example.invalid\n      keys: [{{api-key: fake-1}}, {{api-key: fake-2}}]\n"
    );
    std::fs::write(&path, with_auth_dir(&yaml, &dir)).unwrap();
    let (base, server) = serve(&path).await;
    let client = wreq::Client::new();
    let send = |method: &str, route: &str, body: &str| {
        client
            .request(method.parse().unwrap(), format!("{base}/v0/management{route}"))
            .bearer_auth("fake-secret")
            .body(body.to_owned())
            .send()
    };
    let res = send("DELETE", "/claude-api-key?api-key=missing", "").await.unwrap();
    assert_eq!(res.status().as_u16(), 200);
    assert_eq!(res.text().await.unwrap(), r#"{"status":"ok"}"#);
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(
        saved.contains("# keep this comment") && saved.contains("name: c"),
        "{saved}"
    );

    // A directory at the configured file path fails deterministically, including
    // when the runner has permission to override read-only file modes.
    let backup = dir.join("config.saved.yaml");
    std::fs::rename(&path, &backup).unwrap();
    std::fs::create_dir(&path).unwrap();
    for (method, route, body) in [
        ("PATCH", "/claude-api-key", r#"{"index":0,"value":{"priority":3}}"#),
        ("PUT", "/debug", r#"{"value":true}"#),
    ] {
        let res = send(method, route, body).await.unwrap();
        assert_eq!(res.status().as_u16(), 500, "{method} {route} {body}");
        let value: Value = res.json().await.unwrap();
        assert!(
            value["error"]
                .as_str()
                .is_some_and(|message| message.starts_with("failed to save config:")),
            "{value}"
        );
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), saved);
    }
    let value: Value = send("GET", "/debug", "").await.unwrap().json().await.unwrap();
    assert_eq!(
        value["debug"], false,
        "failed writes cannot publish a new runtime value"
    );
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(&backup, &path).unwrap();
    server.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

/// YAML uploads preserve operator content, hash plaintext management keys, reject
/// malformed documents atomically and disable management after removing its key.
#[tokio::test]
async fn v0_yaml_upload_validates_before_replacing_hashes_keys_and_reloads_access() {
    let hash = bcrypt::hash("fake-secret", 4).unwrap();
    let dir = std::env::temp_dir().join(format!("cpa-legacy-yaml-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.yaml");
    let yaml = format!("config-version: 8\nmanagement:\n  secret-key: '{hash}'\n");
    std::fs::write(&path, with_auth_dir(&yaml, &dir)).unwrap();
    let (base, server) = serve(&path).await;
    let client = wreq::Client::new();
    let put = |body: &str| {
        client
            .put(format!("{base}/v0/management/config.yaml"))
            .bearer_auth("fake-secret")
            .body(body.to_owned())
            .send()
    };
    let upload = "config-version: 8\nmanagement:\n  secret-key: fake-secret\n    # indented comment\nrouting:\n  strategy: fill-first\n";
    let res = put(&with_auth_dir(upload, &dir)).await.unwrap();
    assert_eq!(res.status().as_u16(), 200);
    assert_eq!(res.text().await.unwrap(), r#"{"changed":["config"],"ok":true}"#);
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.contains("# indented comment"), "{saved}");
    let download = client
        .get(format!("{base}/v0/management/config.yaml"))
        .bearer_auth("fake-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(download.status().as_u16(), 200);
    let downloaded: serde_yaml_ng::Value = serde_yaml_ng::from_str(&download.text().await.unwrap()).unwrap();
    assert_eq!(downloaded["routing"]["strategy"].as_str(), Some("fill-first"));
    assert!(
        downloaded["management"]["secret-key"]
            .as_str()
            .is_some_and(|key| key.starts_with("$2"))
    );
    let block = with_auth_dir("routing: [unterminated\n", &dir);
    let res = put(&block).await.unwrap();
    assert_eq!(res.status().as_u16(), 400);
    assert!(res.text().await.unwrap().starts_with(r#"{"error":"invalid_yaml""#));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), saved);
    assert!(saved.contains("strategy: fill-first"), "{saved}");
    assert!(!saved.contains("secret-key: fake-secret"), "the key is hashed: {saved}");
    // The hashed key still authenticates.
    let res = client
        .get(format!("{base}/v0/management/routing/strategy"))
        .bearer_auth("fake-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(res.text().await.unwrap(), r#"{"strategy":"fill-first"}"#);
    // Removing the management key disables the authenticated management routes.
    let keyless = with_auth_dir("", &dir);
    let res = put(&keyless).await.unwrap();
    assert_eq!(res.status().as_u16(), 200);
    assert_eq!(res.text().await.unwrap(), r#"{"changed":["config"],"ok":true}"#);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), keyless);
    let res = client
        .get(format!("{base}/v0/management/debug"))
        .bearer_auth("fake-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 404);
    server.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A private local server and configuration for independently authored API contracts.
struct ContractHarness {
    dir: std::path::PathBuf,
    path: std::path::PathBuf,
    base: String,
    client: wreq::Client,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for ContractHarness {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl ContractHarness {
    async fn new(legacy: bool) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("cpa-v0-contract-{}-{unique}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("config.yaml");
        let hash = bcrypt::hash("fake-secret", 4).unwrap();
        let seed = if legacy {
            format!("# operator comment\nremote-management:\n  secret-key: '{hash}'\n")
        } else {
            format!("# operator comment\nconfig-version: 8\nmanagement:\n  secret-key: '{hash}'\n")
        };
        std::fs::write(&path, with_auth_dir(&seed, &dir)).unwrap();
        let (base, server) = serve(&path).await;
        Self {
            dir,
            path,
            base,
            client: wreq::Client::new(),
            server,
        }
    }

    async fn request(&self, method: &str, route: &str, body: &str) -> (u16, Value) {
        let response = self
            .client
            .request(
                method.parse::<wreq::Method>().unwrap(),
                format!("{}/v0/management{route}", self.base),
            )
            .bearer_auth("fake-secret")
            .body(body.to_owned())
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap();
        let value =
            serde_json::from_str(&text).unwrap_or_else(|error| panic!("{method} {route}: {status} {error}: {text}"));
        (status, value)
    }

    async fn json(&self, method: &str, route: &str, body: Value, status: u16) -> Value {
        let (got, value) = self.request(method, route, &body.to_string()).await;
        assert_eq!(got, status, "{method} {route} {body}: {value}");
        value
    }

    async fn read(&self, route: &str, key: &str) -> Value {
        let (status, body) = self.request("GET", route, "").await;
        assert_eq!(status, 200, "{route}: {body}");
        assert_eq!(body.as_object().unwrap().len(), 1, "{route}: response envelope");
        body.get(key)
            .unwrap_or_else(|| panic!("{route}: missing {key}: {body}"))
            .clone()
    }

    fn persisted(&self, path: &str) -> Value {
        let config = Config::load(&self.path).unwrap();
        let document = serde_json::to_value(config.document).unwrap();
        path.split('/').fold(&document, |v, key| &v[key]).clone()
    }
}

#[tokio::test]
async fn contract_scalar_routes_round_trip_typed_values_in_both_config_layouts() {
    // Route, response key, canonical saved path, default, first value, replacement.
    let fields = [
        (
            "debug",
            "debug",
            "observability/logs/debug",
            serde_json::json!(false),
            serde_json::json!(true),
            serde_json::json!(false),
        ),
        (
            "logging-to-file",
            "logging-to-file",
            "observability/logs/logging-to-file",
            serde_json::json!(false),
            serde_json::json!(true),
            serde_json::json!(false),
        ),
        (
            "logs-max-total-size-mb",
            "logs-max-total-size-mb",
            "observability/logs/logs-max-total-size-mb",
            serde_json::json!(0),
            serde_json::json!(32),
            serde_json::json!(64),
        ),
        (
            "error-logs-max-files",
            "error-logs-max-files",
            "observability/logs/error-logs-max-files",
            serde_json::json!(10),
            serde_json::json!(4),
            serde_json::json!(8),
        ),
        (
            "usage-statistics-enabled",
            "usage-statistics-enabled",
            "observability/usage/usage-statistics-enabled",
            serde_json::json!(false),
            serde_json::json!(true),
            serde_json::json!(false),
        ),
        (
            "proxy-url",
            "proxy-url",
            "requests/proxy-url",
            serde_json::json!(""),
            serde_json::json!("http://127.0.0.1:9?a=<b>&x=2"),
            serde_json::json!(""),
        ),
        (
            "quota-exceeded/switch-project",
            "switch-project",
            "quota-exceeded/switch-project",
            serde_json::json!(false),
            serde_json::json!(true),
            serde_json::json!(false),
        ),
        (
            "quota-exceeded/switch-preview-model",
            "switch-preview-model",
            "quota-exceeded/switch-preview-model",
            serde_json::json!(false),
            serde_json::json!(true),
            serde_json::json!(false),
        ),
        (
            "request-log",
            "request-log",
            "observability/logs/request-log",
            serde_json::json!(false),
            serde_json::json!(true),
            serde_json::json!(false),
        ),
        (
            "request-retry",
            "request-retry",
            "routing/retry/request-retry",
            serde_json::json!(0),
            serde_json::json!(2),
            serde_json::json!(3),
        ),
        (
            "max-retry-credentials",
            "max-retry-credentials",
            "routing/retry/max-retry-credentials",
            serde_json::json!(0),
            serde_json::json!(2),
            serde_json::json!(3),
        ),
        (
            "max-retry-interval",
            "max-retry-interval",
            "routing/retry/max-retry-interval",
            serde_json::json!(0),
            serde_json::json!(12),
            serde_json::json!(24),
        ),
        (
            "force-model-prefix",
            "force-model-prefix",
            "routing/force-model-prefix",
            serde_json::json!(false),
            serde_json::json!(true),
            serde_json::json!(false),
        ),
        (
            "routing/strategy",
            "strategy",
            "routing/strategy",
            serde_json::json!("soonest-reset"),
            serde_json::json!("fill-first"),
            serde_json::json!("soonest-reset"),
        ),
    ];
    for legacy in [false, true] {
        let h = ContractHarness::new(legacy).await;
        for (route, key, path, default, first, next) in &fields {
            let route = format!("/{route}");
            let before = std::fs::read(&h.path).unwrap();
            assert_eq!(h.read(&route, key).await, *default, "{route} layout={legacy}");
            assert_eq!(std::fs::read(&h.path).unwrap(), before, "reads never save");
            for (method, value) in [("PUT", first), ("PATCH", next)] {
                let answer = h.json(method, &route, serde_json::json!({"value":value}), 200).await;
                assert_eq!(answer, serde_json::json!({"status":"ok"}));
                assert_eq!(h.read(&route, key).await, *value, "{route} {method}");
                assert_eq!(h.persisted(path), *value, "{route}: saved config reload");
                let (status, config) = h.request("GET", "/config", "").await;
                assert_eq!(status, 200);
                let from_config = if route.starts_with("/quota-exceeded/") {
                    &config["quota-exceeded"][*key]
                } else if route == "/routing/strategy" {
                    &config["routing"]["strategy"]
                } else {
                    &config[*key]
                };
                assert_eq!(from_config, value, "{route}: full config view");
            }
            let saved = std::fs::read(&h.path).unwrap();
            let wrong = if next.is_string() {
                serde_json::json!(true)
            } else {
                serde_json::json!("invalid-type")
            };
            for invalid in [
                serde_json::json!({}),
                serde_json::json!({"value":null}),
                serde_json::json!({"value":wrong}),
            ] {
                let answer = h.json("PUT", &route, invalid, 400).await;
                assert!(answer["error"].is_string());
                assert_eq!(std::fs::read(&h.path).unwrap(), saved, "invalid {route} changed file");
            }
            let (status, _) = h.request("PATCH", &route, "{bad").await;
            assert_eq!(status, 400);
            assert_eq!(h.read(&route, key).await, *next);
            assert_eq!(std::fs::read(&h.path).unwrap(), saved);
        }
        assert!(std::fs::read_to_string(&h.path).unwrap().contains("# operator comment"));
        for (route, key, value) in [
            ("/logs-max-total-size-mb", "logs-max-total-size-mb", 0),
            ("/error-logs-max-files", "error-logs-max-files", 10),
        ] {
            h.json("PUT", route, serde_json::json!({"value":-1}), 200).await;
            assert_eq!(h.read(route, key).await, value);
        }
        for (alias, canonical) in [
            ("rr", "round-robin"),
            ("wrr", "weighted-round-robin"),
            ("ff", "fill-first"),
            ("reset-first", "soonest-reset"),
        ] {
            h.json("PUT", "/routing/strategy", serde_json::json!({"value":alias}), 200)
                .await;
            assert_eq!(h.read("/routing/strategy", "strategy").await, canonical);
        }
        let saved = std::fs::read(&h.path).unwrap();
        h.json("PUT", "/routing/strategy", serde_json::json!({"value":"unknown"}), 400)
            .await;
        assert_eq!(std::fs::read(&h.path).unwrap(), saved);
        h.json(
            "PUT",
            "/proxy-url",
            serde_json::json!({"value":"http://127.0.0.1:9"}),
            200,
        )
        .await;
        assert_eq!(h.request("DELETE", "/proxy-url", "").await.0, 200);
        assert_eq!(h.read("/proxy-url", "proxy-url").await, "");
    }
}

#[tokio::test]
async fn contract_client_keys_support_replace_patch_append_delete_and_reject_invalid_bodies() {
    for legacy in [false, true] {
        let h = ContractHarness::new(legacy).await;
        h.json("PUT", "/api-keys", serde_json::json!(["a", "b", "a"]), 200)
            .await;
        assert_eq!(
            h.read("/api-keys", "api-keys").await,
            serde_json::json!(["a", "b", "a"])
        );
        h.json(
            "PATCH",
            "/api-keys",
            serde_json::json!({"index":1,"value":"changed"}),
            200,
        )
        .await;
        h.json("PATCH", "/api-keys", serde_json::json!({"old":"a","new":"first"}), 200)
            .await;
        h.json(
            "PATCH",
            "/api-keys",
            serde_json::json!({"old":"missing","new":"appended"}),
            200,
        )
        .await;
        assert_eq!(
            h.read("/api-keys", "api-keys").await,
            serde_json::json!(["first", "changed", "a", "appended"])
        );
        assert_eq!(h.request("DELETE", "/api-keys?index=1", "").await.0, 200);
        assert_eq!(h.request("DELETE", "/api-keys?value=a", "").await.0, 200);
        let expected = serde_json::json!(["first", "appended"]);
        assert_eq!(h.read("/api-keys", "api-keys").await, expected);
        assert_eq!(h.persisted("access/api-keys"), expected);
        let saved = std::fs::read(&h.path).unwrap();
        for (method, route, body) in [
            ("PUT", "/api-keys", "[1]"),
            ("PUT", "/api-keys", "{}"),
            ("PATCH", "/api-keys", "{}"),
            ("PATCH", "/api-keys", r#"{"index":0,"value":true}"#),
            ("DELETE", "/api-keys?index=9999", ""),
        ] {
            assert_eq!(h.request(method, route, body).await.0, 400, "{method} {body}");
            assert_eq!(std::fs::read(&h.path).unwrap(), saved);
        }
        h.json("PUT", "/api-keys", serde_json::json!({"items":["wrapped"]}), 200)
            .await;
        assert_eq!(h.read("/api-keys", "api-keys").await, serde_json::json!(["wrapped"]));
        h.json("PUT", "/api-keys", serde_json::json!([]), 200).await;
        assert!(h.read("/api-keys", "api-keys").await.is_null());
    }
}

#[tokio::test]
async fn contract_provider_lists_support_independent_crud_and_persist_v8_groups() {
    for legacy in [false, true] {
        let h = ContractHarness::new(legacy).await;
        for provider in ["claude", "codex", "xai", "meta"] {
            let key = format!("{provider}-api-key");
            let route = format!("/{key}");
            let entries = serde_json::json!([
                {"api-key":"alpha","base-url":"http://127.0.0.1:9/v1","priority":2,"weight":3},
                {"api-key":"beta","base-url":"http://127.0.0.1:9/v1","priority":1}
            ]);
            h.json("PUT", &route, entries, 200).await;
            let read = h.read(&route, &key).await;
            let list = read.as_array().unwrap();
            assert_eq!(list.len(), 2);
            assert_eq!(list[0]["api-key"], "alpha");
            assert_eq!(list[0]["weight"], 3);
            assert_eq!(list[0]["priority"], 2);
            assert_eq!(list[1]["api-key"], "beta");
            assert!(
                list.iter()
                    .all(|entry| entry["auth-index"].as_str().is_some_and(|v| !v.is_empty()))
            );
            h.json(
                "PATCH",
                &route,
                serde_json::json!({"index":0,"value":{"priority":7}}),
                200,
            )
            .await;
            h.json(
                "PATCH",
                &route,
                serde_json::json!({"match":"beta","value":{"priority":8}}),
                200,
            )
            .await;
            let list = h.read(&route, &key).await;
            assert_eq!(list[0]["priority"], 7);
            assert_eq!(list[1]["priority"], 8);
            let groups = h.persisted(&format!("api-keys/{provider}"));
            assert_eq!(groups.as_array().unwrap().len(), 2);
            assert_eq!(groups[0]["keys"][0]["api-key"], "alpha");
            assert_eq!(groups[0]["priority"], 7);
            let saved = std::fs::read(&h.path).unwrap();
            for (method, body, status) in [
                ("PUT", "{}", 400),
                ("PUT", "[true]", 400),
                ("PATCH", "{}", 400),
                ("PATCH", r#"{"index":9999,"value":{"priority":1}}"#, 404),
                ("PATCH", r#"{"match":"missing","value":{}}"#, 404),
                ("PATCH", r#"{"index":0,"value":{"weight":1000001}}"#, 400),
                ("PATCH", r#"{"index":0,"value":{"priority":"bad"}}"#, 400),
            ] {
                assert_eq!(h.request(method, &route, body).await.0, status, "{provider} {body}");
                assert_eq!(
                    std::fs::read(&h.path).unwrap(),
                    saved,
                    "rejected {provider} write saved"
                );
            }
            assert_eq!(h.request("DELETE", &format!("{route}?index=0"), "").await.0, 200);
            assert_eq!(h.read(&route, &key).await.as_array().unwrap().len(), 1);
            assert_eq!(h.request("DELETE", &format!("{route}?api-key=beta"), "").await.0, 200);
            let empty = h.read(&route, &key).await;
            assert!(empty.is_null() || empty == serde_json::json!([]));
            // A duplicated secret is not enough to identify a deletion target.
            // An operator must disambiguate it with the upstream endpoint.
            h.json(
                "PUT",
                &route,
                serde_json::json!([
                    {"api-key":"shared","base-url":"http://127.0.0.1:9/first"},
                    {"api-key":"shared","base-url":"http://127.0.0.1:9/second"}
                ]),
                200,
            )
            .await;
            let saved = std::fs::read(&h.path).unwrap();
            assert_eq!(h.request("DELETE", &format!("{route}?api-key=shared"), "").await.0, 400);
            assert_eq!(std::fs::read(&h.path).unwrap(), saved);
            assert_eq!(
                h.request(
                    "DELETE",
                    &format!("{route}?api-key=shared&base-url=http%3A%2F%2F127.0.0.1%3A9%2Ffirst"),
                    ""
                )
                .await
                .0,
                200
            );
            let remaining = h.read(&route, &key).await;
            assert_eq!(remaining.as_array().unwrap().len(), 1);
            assert_eq!(remaining[0]["base-url"], "http://127.0.0.1:9/second");
            h.json(
                "PUT",
                &route,
                serde_json::json!({"items":[{"api-key":"wrapped","base-url":"http://127.0.0.1:9/v1"}]}),
                200,
            )
            .await;
            assert_eq!(h.read(&route, &key).await[0]["api-key"], "wrapped");
        }
    }
}

#[tokio::test]
async fn contract_oauth_maps_normalize_entries_patch_one_channel_and_delete_without_harming_others() {
    let cases = [
        (
            "oauth-excluded-models",
            "oauth/excluded-models",
            "provider",
            "models",
            serde_json::json!([" MODEL-A ", "model-a", "", "Model-B"]),
            serde_json::json!(["model-a", "model-b"]),
        ),
        (
            "oauth-model-alias",
            "oauth/model-alias",
            "channel",
            "aliases",
            serde_json::json!([
                {"name":" model-a ","alias":" friendly ","display-name":" Friendly "},
                {"name":"other","alias":"FRIENDLY"}, {"name":"same","alias":"same"}, {"name":"","alias":"bad"}
            ]),
            serde_json::json!([{"name":"model-a","alias":"friendly","display-name":"Friendly"}]),
        ),
        (
            "oauth-request-scoped-errors",
            "oauth/request-scoped-errors",
            "channel",
            "rules",
            serde_json::json!([
                {"status":429,"match":[" rate limited ", ""],"action":" CONTINUE-AND-COOLDOWN "},
                {"status":0,"match":["invalid"],"action":"stop"}, {"status":400,"action":"stop"}
            ]),
            serde_json::json!([{"status":429,"match":["rate limited"],"action":"continue-and-cooldown"}]),
        ),
    ];
    for legacy in [false, true] {
        let h = ContractHarness::new(legacy).await;
        for (key, path, selector, list_name, input, normalized) in &cases {
            let route = format!("/{key}");
            h.json("PUT", &route, serde_json::json!({" CLAUDE ":input}), 200).await;
            assert_eq!(
                h.read(&route, key).await,
                serde_json::json!({"claude":normalized}),
                "{key}"
            );
            assert_eq!(
                h.persisted(path),
                serde_json::json!({"claude":normalized}),
                "saved {key}"
            );
            let mut patch = serde_json::Map::new();
            patch.insert((*selector).into(), serde_json::json!(" XAI "));
            patch.insert((*list_name).into(), input.clone());
            h.json("PATCH", &route, Value::Object(patch), 200).await;
            assert_eq!(
                h.read(&route, key).await,
                serde_json::json!({"claude":normalized,"xai":normalized})
            );
            let saved = std::fs::read(&h.path).unwrap();
            for invalid in ["not json", "42"] {
                assert_eq!(h.request("PUT", &route, invalid).await.0, 400, "{key} {invalid}");
                assert_eq!(std::fs::read(&h.path).unwrap(), saved);
            }
            assert_eq!(
                h.request("DELETE", &format!("{route}?{selector}=claude"), "").await.0,
                200
            );
            assert_eq!(h.read(&route, key).await, serde_json::json!({"xai":normalized}));
            let saved = std::fs::read(&h.path).unwrap();
            assert_eq!(
                h.request("DELETE", &format!("{route}?{selector}=missing"), "").await.0,
                404
            );
            assert_eq!(std::fs::read(&h.path).unwrap(), saved);
            // An empty PATCH explicitly removes its selected channel.
            let mut remove = serde_json::Map::new();
            remove.insert((*selector).into(), serde_json::json!("xai"));
            remove.insert((*list_name).into(), serde_json::json!([]));
            h.json("PATCH", &route, Value::Object(remove), 200).await;
            assert!(h.read(&route, key).await.is_null());
            assert!(h.persisted(path).is_null());
        }
    }
}

#[tokio::test]
async fn contract_removed_routes_and_settings_refuse_every_write_without_persisting() {
    let h = ContractHarness::new(false).await;
    for route in [
        "/gemini-api-key",
        "/interactions-api-key",
        "/vertex-api-key",
        "/openai-compatibility",
        "/vertex/import",
        "/antigravity-auth-url",
        "/kimi-auth-url",
        "/kimi-ai-auth-url",
        "/devin-auth-url",
    ] {
        for method in ["GET", "PUT", "PATCH", "DELETE", "POST"] {
            // These aliases have the methods of their original public route.
            if route.ends_with("auth-url") && method != "GET" {
                continue;
            }
            if route == "/vertex/import" && method != "POST" {
                continue;
            }
            if (route.ends_with("api-key") || route == "/openai-compatibility") && method == "POST" {
                continue;
            }
            let saved = std::fs::read(&h.path).unwrap();
            let (status, value) = h.request(method, route, r#"{"items":[]}"#).await;
            assert_eq!(status, 410, "{method} {route}: {value}");
            assert_eq!(value["code"], "removed_provider");
            assert!(
                value
                    .as_object()
                    .unwrap()
                    .values()
                    .filter_map(Value::as_str)
                    .any(|text| text.contains(cpa_core::removed::GUIDANCE))
            );
            assert_eq!(std::fs::read(&h.path).unwrap(), saved);
        }
    }
    for (route, body) in [
        ("/ws-auth", serde_json::json!({"value":true})),
        ("/oauth-excluded-models", serde_json::json!({"gemini":["model"]})),
        (
            "/oauth-model-alias",
            serde_json::json!({"gemini":[{"name":"model","alias":"alias"}]}),
        ),
        (
            "/oauth-request-scoped-errors",
            serde_json::json!({"gemini":[{"status":429,"match":["limit"],"action":"stop"}]}),
        ),
    ] {
        let saved = std::fs::read(&h.path).unwrap();
        let value = h.json("PUT", route, body, 422).await;
        assert!(
            value
                .as_object()
                .unwrap()
                .values()
                .filter_map(Value::as_str)
                .any(|text| text.contains(cpa_core::removed::GUIDANCE))
        );
        assert_eq!(std::fs::read(&h.path).unwrap(), saved);
    }
    for (route, body) in [
        (
            "/oauth-excluded-models",
            serde_json::json!({"provider":"gemini","models":["model"]}),
        ),
        (
            "/oauth-model-alias",
            serde_json::json!({"channel":"gemini","aliases":[{"name":"model","alias":"alias"}]}),
        ),
        (
            "/oauth-request-scoped-errors",
            serde_json::json!({"channel":"gemini","rules":[{"status":429,"match":["limit"],"action":"stop"}]}),
        ),
    ] {
        let saved = std::fs::read(&h.path).unwrap();
        h.json("PATCH", route, body, 422).await;
        assert_eq!(std::fs::read(&h.path).unwrap(), saved);
    }
    let saved = std::fs::read(&h.path).unwrap();
    let upload = with_auth_dir(
        "config-version: 8\nmanagement:\n  secret-key: fake-secret\napi-keys:\n  openai-compatibility: [{name: old, base-url: http://127.0.0.1:9, keys: [{api-key: removed-test-key}]}]\n",
        &h.dir,
    );
    let (status, value) = h.request("PUT", "/config.yaml", &upload).await;
    assert_eq!(status, 422, "{value}");
    assert_eq!(std::fs::read(&h.path).unwrap(), saved);
}

#[tokio::test]
async fn contract_claude_normalization_cloak_inheritance_and_validation_preserve_operator_intent() {
    let h = ContractHarness::new(false).await;
    let initial = serde_json::json!([{
        "api-key":" key ", "base-url":" http://127.0.0.1:9 ", "prefix":" /team/ ",
        "headers":{" X-Test ":" value ", "Empty":" "}, "excluded-models":[" Model-A ","model-a", ""],
        "fingerprint-profile":"oauth-cli", "cloak":{"mode":" auto ","sensitive-words":[" private ", ""]}
    }]);
    h.json("PUT", "/claude-api-key", initial, 200).await;
    let value = h.read("/claude-api-key", "claude-api-key").await;
    let entry = &value[0];
    assert_eq!(entry["api-key"], "key");
    assert_eq!(entry["base-url"], "http://127.0.0.1:9");
    assert_eq!(entry["prefix"], "team");
    assert_eq!(entry["headers"], serde_json::json!({"X-Test":"value"}));
    assert_eq!(entry["excluded-models"], serde_json::json!(["model-a"]));
    assert_eq!(entry["fingerprint-profile"], "claude-code-cli");
    assert_eq!(entry["cloak"]["mode"], "auto");
    assert_eq!(entry["cloak"]["sensitive-words"], serde_json::json!(["private"]));
    h.json(
        "PUT",
        "/claude-api-key",
        serde_json::json!([{"api-key":"key","base-url":"http://127.0.0.1:9","prefix":"team"}]),
        200,
    )
    .await;
    assert_eq!(
        h.read("/claude-api-key", "claude-api-key").await[0]["cloak"]["mode"],
        "auto",
        "same identity inherits explicit cloak mode"
    );
    h.json(
        "PATCH",
        "/claude-api-key",
        serde_json::json!({"index":0,"value":{"api-key":"replacement"}}),
        200,
    )
    .await;
    assert!(
        h.read("/claude-api-key", "claude-api-key").await[0]["cloak"]["mode"]
            .as_str()
            .is_none_or(str::is_empty),
        "changed identity resets inherited cloak mode"
    );
    let saved = std::fs::read(&h.path).unwrap();
    for body in [
        serde_json::json!([{"api-key":"key","weight":1000001}]),
        serde_json::json!([{"api-key":"key","fingerprint-profile":"unknown"}]),
    ] {
        h.json("PUT", "/claude-api-key", body, 400).await;
        assert_eq!(std::fs::read(&h.path).unwrap(), saved);
    }
}
