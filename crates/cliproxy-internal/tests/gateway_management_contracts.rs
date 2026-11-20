//! Management API contracts: independently asserted config, credentials, auth and logs.
use std::net::SocketAddr;
use std::path::Path;

use cliproxy_internal::config::{Config, TrustedProxies, credentials};
use cliproxy_internal::credential::Source;
use serde_json::{Value, json};

#[test]
fn config_boolean_spellings_and_opaque_host_text_have_explicit_types() {
    for (spelling, expected) in [
        ("true", true),
        ("TRUE", true),
        ("yes", true),
        ("on", true),
        ("false", false),
        ("NO", false),
        ("off", false),
        ("null", false),
    ] {
        let cfg = Config::parse(&format!("observability: {{logs: {{debug: {spelling}}}}}\n")).unwrap();
        assert_eq!(
            cfg.document["observability"]["logs"]["debug"]
                .as_bool()
                .unwrap_or_default(),
            expected,
            "{spelling}"
        );
    }
    for text in ["1.50", "0x1F", "007", "true", "2001-12-14"] {
        let cfg = Config::parse(&format!("server: {{host: {text}}}\n")).unwrap();
        assert_eq!(cfg.host, text);
    }
    for spelling in ["maybe", "[]", "{}", "1"] {
        assert!(Config::parse(&format!("observability: {{logs: {{debug: {spelling}}}}}\n")).is_err());
    }
    for yaml in [
        "plugins: {}",
        "plugins: null",
        "plugins: {configs: {x: {enabled: yes}}}",
    ] {
        assert_eq!(
            Config::parse(yaml).unwrap_err().to_string(),
            cliproxy_internal::config::PLUGINS_REMOVED
        );
    }
}

#[test]
fn retry_numbers_preserve_supported_operator_spellings() {
    for (spelling, expected) in [("2", 2), ("2.9", 2), ("-1.5", -1), ("1e3", 1000), ("0x10", 16)] {
        let cfg = Config::parse(&format!("routing: {{retry: {{request-retry: {spelling}}}}}\n")).unwrap();
        assert_eq!(cfg.routing.retry.request_retry, expected);
    }
    for malformed in ["'5'", "[]", "{}", "18446744073709551616"] {
        assert!(Config::parse(&format!("routing: {{retry: {{request-retry: {malformed}}}}}\n")).is_err());
    }
}

#[test]
fn config_merges_resolve_explicit_values_before_inherited_values() {
    for (yaml, retry, interval) in [
        (
            "routing: {retry: {<<: [{request-retry: 4, max-retry-interval: 9}, {request-retry: 8}], max-retry-interval: 2}}",
            4,
            2,
        ),
        (
            "routing: {retry: {<<: {<<: {request-retry: 6}, max-retry-interval: 2}}}",
            6,
            2,
        ),
        ("request-retry: &n 5\nmax-retry-interval: *n", 5, 5),
    ] {
        let cfg = Config::parse(yaml).unwrap();
        assert_eq!(
            (cfg.routing.retry.request_retry, cfg.routing.retry.max_retry_interval),
            (retry, interval)
        );
        assert_eq!(
            Config::parse(&cliproxy_internal::config::yaml_string(&cfg.document).unwrap())
                .unwrap()
                .document,
            cfg.document
        );
    }
    let cfg = Config::parse(
        "claude-api-key:\n - &key {api-key: synthetic-first, priority: 2}\n - {<<: *key, api-key: synthetic-second}\n",
    )
    .unwrap();
    let creds = credentials::from_config(&cfg);
    assert_eq!(
        creds
            .iter()
            .map(|c| c.attributes["api_key"].as_str())
            .collect::<Vec<_>>(),
        ["synthetic-first", "synthetic-second"]
    );
    assert!(
        creds
            .iter()
            .all(|c| c.attributes.get("priority").map(String::as_str) == Some("2"))
    );
    for merge in ["5", "[5]"] {
        assert!(Config::parse(&format!("routing: {{retry: {{<<: {merge}, request-retry: 1}}}}\n")).is_err());
    }
}

#[test]
fn forwarded_client_identity_requires_a_trusted_socket_peer() {
    let trusted = TrustedProxies::new(&["127.0.0.1".into(), "10.0.0.0/8".into(), "::1".into()]);
    for (peer, accepts_forwarding) in [
        ("127.0.0.1:1", true),
        ("10.1.2.3:1", true),
        ("[::1]:1", true),
        ("198.51.100.7:1", false),
        ("[2001:db8::7]:1", false),
    ] {
        let peer: SocketAddr = peer.parse().unwrap();
        for (xff, real, client) in [
            ("198.51.100.9", None, "198.51.100.9"),
            ("198.51.100.9, 10.1.2.3", None, "198.51.100.9"),
            ("198.51.100.9, 203.0.113.2", None, "203.0.113.2"),
            ("198.51.100.9, ::1", None, "198.51.100.9"),
            ("bad, 198.51.100.9", None, "198.51.100.9"),
            ("bad", Some("203.0.113.5"), "203.0.113.5"),
        ] {
            let got = trusted.client_ip(Some(peer), |name| match name {
                "X-Forwarded-For" => Some(xff.as_bytes()),
                "X-Real-IP" => real.map(str::as_bytes),
                _ => None,
            });
            assert_eq!(
                got,
                if accepts_forwarding {
                    client.to_owned()
                } else {
                    peer.ip().to_string()
                }
            );
        }
        assert_eq!(trusted.client_ip(Some(peer), |_| None), peer.ip().to_string());
    }
}

#[test]
fn malformed_forwarding_bytes_fall_back_without_trusting_spoofed_addresses() {
    let trusted = TrustedProxies::new(&["127.0.0.1".into()]);
    let peer = "127.0.0.1:1".parse().unwrap();
    type ForwardingCase<'a> = (&'a [u8], Option<&'a [u8]>, &'a str);
    let cases: &[ForwardingCase<'_>] = &[
        (b"\xff\xfe, 198.51.100.9", None, "198.51.100.9"),
        ("\u{a0}198.51.100.9\u{2003}".as_bytes(), None, "198.51.100.9"),
        ("198.51.100.9 \u{85}".as_bytes(), None, "198.51.100.9"),
        (b"198.51.100.9, \xff", Some(b"203.0.113.5"), "203.0.113.5"),
        (b"198.51.100.9\xff", None, "127.0.0.1"),
        (b"\xff198.51.100.9", None, "127.0.0.1"),
    ];
    for &(xff, real, expected) in cases {
        assert_eq!(
            trusted.client_ip(Some(peer), |name| match name {
                "X-Forwarded-For" => Some(xff),
                "X-Real-IP" => real,
                _ => None,
            }),
            expected
        );
    }
}

#[test]
fn invalid_config_boundaries_report_actionable_fields() {
    // Preserve all historical validation stimuli, but no decoder's error prose.
    let inputs: Vec<String> = serde_json::from_str(include_str!("gateway/fixtures/manage_config_inputs.json")).unwrap();
    const ACCEPTED: &[&str] = &[
        "server: {trusted-proxies: [127.0.0.1, '::1', 10.0.0.0/8]}",
        "credentials: {in-flight: {snapshot-interval: 1.5s, stale-after: 4.5s}}",
        "credentials: {in-flight: {snapshot-interval: null, stale-after: 1h1m0.5s}}",
        "credentials: {in-flight: {snapshot-interval: .5us}}",
        "oauth: {providers: {codex: {live-media-relay: {enabled: true, public-ip: ' 2001:db8::1 '}}}}",
        "oauth: {providers: {codex: {live-media-relay: {enabled: true, max-sessions: 2, udp-port-min: 10000, udp-port-max: 10003}}}}",
        "oauth: {providers: {codex: {live-media-relay: {enabled: false, max-sessions: -1, ice-servers: [{urls: []}]}}}}",
        "oauth: {providers: {codex: {live-media-relay: {allow-private-remote-ips: true, disable-private-remote-ips: true}}}}",
        "models: {}",
        "models: null",
        "models: {catalog: ''}",
        "models: {catalog: null}",
        "models: {catalog: /abs/models.json}",
        "models: {codex-catalog: 'https://example.invalid/models.json'}",
        "models: {catalog: 'https://[::1]:443/x'}",
        "models: {catalog: 'https://example.invalid/x '}",
        "models: {catalog: '//host/share/m.json'}",
        "models: {catalog: 'http://[::1]:80/p'}",
        "models: {other: x}",
    ];
    for yaml in inputs {
        let parsed = Config::parse(&yaml);
        if ACCEPTED.contains(&yaml.as_str()) {
            parsed.unwrap_or_else(|e| panic!("accepted config {yaml}: {e:#}"));
        } else {
            let error = parsed.unwrap_err().to_string();
            assert!(!error.is_empty());
            let field = if yaml.contains("trusted-proxies") {
                "trusted-proxies"
            } else if yaml.contains("in-flight") {
                "in-flight"
            } else if yaml.contains("relay") {
                "live-media-relay"
            } else {
                "models"
            };
            assert!(error.contains(field), "{yaml}: error must identify {field}: {error}");
        }
    }
}

#[test]
fn oauth_only_settings_do_not_reach_api_key_credentials() {
    let cfg =
        Config::parse("oauth: {providers: {codex: {live-media-relay: {enabled: false, ice-servers: []}}}}\n").unwrap();
    assert_eq!(
        cfg.oauth_only,
        [
            "oauth.providers.codex.live-media-relay.enabled".to_owned(),
            "oauth.providers.codex.live-media-relay.ice-servers".to_owned()
        ]
        .into()
    );
    let view = cfg.for_api_key();
    assert!(view.oauth_only.is_empty());
    assert!(
        view.document["oauth"]["providers"]["codex"]["live-media-relay"]
            .get("ice-servers")
            .is_none()
    );
    let shared = Config::parse("upstream: {claude: {model-level-cooling: true}}\n").unwrap();
    assert!(shared.oauth_only.is_empty());
    assert_eq!(shared.for_api_key().document, shared.document);
    assert!(
        Config::parse("oauth: {providers: {codex: {header-defaults: {user-agent: x}}}}\n")
            .unwrap_err()
            .to_string()
            .contains("header-defaults.user-agent")
    );
}

#[test]
fn grouped_credentials_inherit_defaults_and_keep_explicit_overrides() {
    let cfg = Config::parse("api-keys:\n claude:\n  - base-url: https://claude.example.invalid\n    priority: 5\n    prefix: team\n    disable-cooling: true\n    headers: {X-Group: g}\n    excluded-models: [group-model]\n    keys:\n      - api-key: synthetic-first\n      - {api-key: synthetic-second, priority: 9, disable-cooling: false, weight: 7, excluded-models: [key-model]}\n").unwrap();
    let creds = credentials::from_config(&cfg);
    assert_eq!(creds.len(), 2);
    for c in &creds {
        assert_eq!(c.provider, "claude");
        assert_eq!(
            c.attributes.get("base_url").map(String::as_str),
            Some("https://claude.example.invalid")
        );
        assert_eq!(c.attributes.get("prefix").map(String::as_str), Some("team"));
        assert_eq!(c.attributes.get("header:X-Group").map(String::as_str), Some("g"));
    }
    assert_eq!(creds[0].attributes.get("priority").map(String::as_str), Some("5"));
    assert_eq!(creds[1].attributes.get("priority").map(String::as_str), Some("9"));
    assert_eq!(creds[1].attributes.get("weight").map(String::as_str), Some("7"));
    assert_ne!(creds[0].id, creds[1].id);
    assert_eq!(
        credentials::from_config(&cfg).iter().map(|c| &c.id).collect::<Vec<_>>(),
        creds.iter().map(|c| &c.id).collect::<Vec<_>>()
    );
}

#[test]
fn saved_credentials_preserve_unknown_metadata_and_skip_removed_providers() {
    let dir = std::env::temp_dir().join(format!("cpa-credential-policy-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (file, body) in [
        (
            "claude.json",
            r#"{"type":"Claude","email":"operator@example.invalid","disabled":true,"unknown":{"keep":true},"excluded_models":[" Haiku "]}"#,
        ),
        ("codex.json", r#"{"type":"codex","plan_type":"pro"}"#),
        ("broken.json", r#"{"type":"#),
        ("missing.json", r#"{"email":"operator@example.invalid"}"#),
        ("removed.json", r#"{"type":"gemini"}"#),
    ] {
        std::fs::write(dir.join(file), body).unwrap();
    }
    let cfg = Config {
        auth_dir: dir.clone(),
        ..Config::default()
    };
    let creds = credentials::from_auth_dir(&cfg);
    assert_eq!(
        creds
            .iter()
            .map(|c| c.provider.as_str())
            .collect::<std::collections::BTreeSet<_>>(),
        ["claude", "codex"].into()
    );
    let claude = creds.iter().find(|c| c.provider == "claude").unwrap();
    assert!(claude.disabled);
    assert_eq!(claude.label, "operator@example.invalid");
    assert_eq!(claude.metadata["unknown"], json!({"keep":true}));
    assert_eq!(
        claude.attributes.get("excluded_models").map(String::as_str),
        Some("haiku")
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("removed.json")).unwrap(),
        r#"{"type":"gemini"}"#
    );
    std::fs::remove_dir_all(dir).unwrap();
}

mod access {
    use super::*;
    use cliproxy_internal::gateway::management::{self, Management, Options};
    use cliproxy_internal::providers::Executors;
    use cliproxy_internal::providers::claude::ClaudeExecutor;
    use std::sync::Arc;

    /// Serves the real management router; `X-Test-Peer` stands in for the socket peer.
    pub(super) async fn serve(state: Arc<Management>) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = management::router(state).layer(axum::middleware::from_fn(
            |mut req: axum::extract::Request, next: axum::middleware::Next| async move {
                let peer: SocketAddr = req.headers()["x-test-peer"].to_str().unwrap().parse().unwrap();
                req.extensions_mut().insert(axum::extract::ConnectInfo(peer));
                next.run(req).await
            },
        ));
        (
            base,
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
        )
    }

    pub(super) fn state(dir: &Path, scenario: &Value) -> Arc<Management> {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("config.yaml");
        let mut yaml = format!(
            "config-version: 8\nmanagement:\n  allow-remote: {}\n",
            scenario["allow_remote"]
        );
        if let Some(secret) = scenario["secret"].as_str() {
            yaml += &format!("  secret-key: '{}'\n", bcrypt::hash(secret, 4).unwrap());
        }
        if let Some(t) = scenario["trusted"].as_array() {
            yaml += &format!("server:\n  trusted-proxies: {}\n", Value::Array(t.clone()));
        }
        std::fs::write(&path, yaml).unwrap();
        let mut cfg = Config::load(&path).unwrap();
        cfg.auth_dir = dir.join("auth");
        std::fs::create_dir_all(&cfg.auth_dir).unwrap();
        let rt = Arc::new(cliproxy_internal::gateway::testing::runtime(
            cfg,
            vec![],
            Executors {
                claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
                codex: Default::default(),
                meta: Default::default(),
                xai: Default::default(),
            },
        ));
        let options = Options {
            local_password: scenario["local_password"].as_str().unwrap_or_default().into(),
            management_password: Some(scenario["env"].as_str().unwrap_or_default().into()),
            ..Options::default()
        };
        Management::with_options(rt, path, options)
    }

    #[tokio::test]
    async fn management_authentication_enforces_credentials_remote_policy_and_ip_bans() {
        struct Step {
            peer: &'static str,
            headers: Vec<(&'static str, &'static str)>,
            status: u16,
        }
        let step = |peer, headers, status| Step { peer, headers, status };
        let valid = || vec![("Authorization", "Bearer synthetic-secret")];
        let wrong = || vec![("Authorization", "Bearer synthetic-wrong")];
        let local = "127.0.0.1:40000";
        let remote = "203.0.113.7:1";
        let seed = |allow_remote| json!({"secret":"synthetic-secret","allow_remote":allow_remote});
        let mut cases: Vec<(&str, Value, Vec<Step>)> = vec![
            (
                "header_precedence",
                seed(false),
                vec![
                    step(local, valid(), 200),
                    step(local, vec![("X-Management-Key", "synthetic-secret")], 200),
                    step(local, vec![("Authorization", "synthetic-secret")], 200),
                    step(local, vec![("Authorization", "bEaReR synthetic-secret")], 200),
                    step(local, vec![("Authorization", "Bearer  synthetic-secret")], 401),
                    step(
                        local,
                        vec![("Authorization", "Basic abc"), ("X-Management-Key", "synthetic-secret")],
                        401,
                    ),
                    step(
                        local,
                        vec![
                            ("Authorization", "Bearer synthetic-secret"),
                            ("Authorization", "Bearer other"),
                        ],
                        200,
                    ),
                    step(local, vec![], 401),
                ],
            ),
            (
                "remote_policy",
                seed(false),
                vec![
                    step(remote, valid(), 403),
                    step("127.0.0.2:1", valid(), 403),
                    step("[::1]:1", valid(), 200),
                    step("[::ffff:127.0.0.1]:1", valid(), 200),
                    step("[0:0:0:0:0:0:0:1]:1", valid(), 200),
                    step(remote, vec![], 403),
                ],
            ),
            (
                "remote_policy_precedes_bad_key",
                seed(false),
                vec![step(remote, wrong(), 403), step(remote, wrong(), 403)],
            ),
            (
                "environment_secret",
                json!({"allow_remote":false,"env":"synthetic-env"}),
                vec![
                    step(remote, vec![("Authorization", "Bearer synthetic-env")], 200),
                    step(remote, wrong(), 401),
                    step(local, vec![("X-Management-Key", "synthetic-env")], 200),
                ],
            ),
            (
                "both_management_secrets",
                json!({"allow_remote":false,"env":"synthetic-env","secret":"synthetic-secret"}),
                vec![
                    step(remote, valid(), 200),
                    step(remote, vec![("Authorization", "Bearer synthetic-env")], 200),
                ],
            ),
            (
                "local_password",
                json!({"allow_remote":true,"secret":"synthetic-secret","local_password":"synthetic-local"}),
                vec![
                    step(local, vec![("Authorization", "Bearer synthetic-local")], 200),
                    step(remote, vec![("Authorization", "Bearer synthetic-local")], 401),
                    step("[::1]:3", vec![("X-Management-Key", "synthetic-local")], 200),
                ],
            ),
            (
                "local_password_needs_management_secret",
                json!({"allow_remote":false,"local_password":"synthetic-local"}),
                vec![
                    step(local, vec![("Authorization", "Bearer synthetic-local")], 403),
                    step(local, vec![], 403),
                ],
            ),
            (
                "trusted_forwarding",
                json!({"allow_remote":false,"secret":"synthetic-secret","trusted":["127.0.0.1","::1"]}),
                vec![
                    step(
                        local,
                        vec![
                            ("Authorization", "Bearer synthetic-secret"),
                            ("X-Forwarded-For", "203.0.113.9"),
                        ],
                        403,
                    ),
                    step(
                        local,
                        vec![
                            ("Authorization", "Bearer synthetic-secret"),
                            ("X-Forwarded-For", "127.0.0.1"),
                        ],
                        200,
                    ),
                    step(
                        local,
                        vec![
                            ("Authorization", "Bearer synthetic-secret"),
                            ("X-Forwarded-For", "203.0.113.9, 127.0.0.1"),
                        ],
                        403,
                    ),
                    step(
                        local,
                        vec![("Authorization", "Bearer synthetic-secret"), ("X-Real-IP", "::1")],
                        200,
                    ),
                    step(
                        local,
                        vec![
                            ("Authorization", "Bearer synthetic-secret"),
                            ("X-Forwarded-For", "garbage"),
                            ("X-Real-IP", "203.0.113.4"),
                        ],
                        403,
                    ),
                    step(
                        local,
                        vec![
                            ("Authorization", "Bearer synthetic-secret"),
                            ("X-Forwarded-For", "::ffff:127.0.0.1"),
                        ],
                        403,
                    ),
                    step(
                        remote,
                        vec![
                            ("Authorization", "Bearer synthetic-secret"),
                            ("X-Forwarded-For", "127.0.0.1"),
                        ],
                        403,
                    ),
                ],
            ),
            (
                "untrusted_forwarding",
                seed(false),
                vec![
                    step(
                        remote,
                        vec![
                            ("Authorization", "Bearer synthetic-secret"),
                            ("X-Forwarded-For", "127.0.0.1"),
                        ],
                        403,
                    ),
                    step(
                        local,
                        vec![
                            ("Authorization", "Bearer synthetic-secret"),
                            ("X-Forwarded-For", "203.0.113.9"),
                        ],
                        200,
                    ),
                ],
            ),
            (
                "mapped_literal_trusts_only_the_configured_host",
                json!({"allow_remote":false,"secret":"synthetic-secret","trusted":["::ffff:10.1.2.3"]}),
                vec![
                    step(
                        "10.1.2.3:1",
                        vec![
                            ("Authorization", "Bearer synthetic-secret"),
                            ("X-Forwarded-For", "127.0.0.1"),
                        ],
                        200,
                    ),
                    step(
                        "[::ffff:10.1.2.3]:1",
                        vec![
                            ("Authorization", "Bearer synthetic-secret"),
                            ("X-Forwarded-For", "127.0.0.1"),
                        ],
                        200,
                    ),
                    step(
                        "10.1.2.2:1",
                        vec![
                            ("Authorization", "Bearer synthetic-secret"),
                            ("X-Forwarded-For", "127.0.0.1"),
                        ],
                        403,
                    ),
                    step(
                        "[::2]:1",
                        vec![
                            ("Authorization", "Bearer synthetic-secret"),
                            ("X-Forwarded-For", "127.0.0.1"),
                        ],
                        403,
                    ),
                ],
            ),
            (
                "junk_forwarding",
                json!({"allow_remote":false,"secret":"synthetic-secret","trusted":["127.0.0.1"]}),
                vec![
                    step(
                        local,
                        vec![
                            ("Authorization", "Bearer synthetic-secret"),
                            ("X-Forwarded-For", "été, 198.51.100.9"),
                        ],
                        403,
                    ),
                    step(
                        local,
                        vec![
                            ("Authorization", "Bearer synthetic-secret"),
                            ("X-Forwarded-For", "été, 127.0.0.1"),
                        ],
                        200,
                    ),
                ],
            ),
        ];
        let mut banned_local = (0..5).map(|_| step(local, wrong(), 401)).collect::<Vec<_>>();
        banned_local[2] = step(local, vec![], 401);
        banned_local.push(step(local, valid(), 403));
        banned_local.push(step("[::1]:2", valid(), 200));
        cases.push(("local_failures_are_counted_per_ip", seed(false), banned_local));
        let mut reset = Vec::new();
        for _ in 0..2 {
            reset.extend((0..4).map(|_| step(local, wrong(), 401)));
            reset.push(step(local, valid(), 200));
        }
        cases.push(("success_resets_failure_count", seed(false), reset));
        let mut banned_remote = vec![step(remote, valid(), 200)];
        banned_remote.extend((0..5).map(|_| step(remote, wrong(), 401)));
        banned_remote.push(step(remote, valid(), 403));
        banned_remote.push(step(local, valid(), 200));
        cases.push(("remote_failures_are_counted_per_ip", seed(true), banned_remote));
        let forwarded = |key, address| vec![("Authorization", key), ("X-Forwarded-For", address)];
        let mut forwarded_ban = (0..5)
            .map(|_| step(local, forwarded("Bearer wrong", "203.0.113.20"), 401))
            .collect::<Vec<_>>();
        forwarded_ban.push(step(local, forwarded("Bearer synthetic-secret", "203.0.113.20"), 403));
        forwarded_ban.push(step(local, forwarded("Bearer synthetic-secret", "203.0.113.21"), 200));
        forwarded_ban.push(step(local, valid(), 200));
        cases.push((
            "trusted_forwarded_ip_is_the_ban_key",
            json!({"allow_remote":true,"secret":"synthetic-secret","trusted":["127.0.0.0/8"]}),
            forwarded_ban,
        ));
        let client = wreq::Client::new();
        for (name, scenario, steps) in cases {
            let dir = std::env::temp_dir().join(format!("cpa-access-policy-{name}-{}", std::process::id()));
            let (base, server) = serve(state(&dir, &scenario)).await;
            for (i, step) in steps.into_iter().enumerate() {
                let mut request = client
                    .get(format!("{base}/v8/management/config"))
                    .header("X-Test-Peer", step.peer);
                for (key, value) in step.headers {
                    request = request.header(key, wreq::header::HeaderValue::from_bytes(value.as_bytes()).unwrap());
                }
                let response = request.send().await.unwrap();
                let status = response.status().as_u16();
                assert!(response.headers().contains_key("x-cpa-version"));
                let body = response.text().await.unwrap();
                assert_eq!(status, step.status, "{name}[{i}]: {body}");
                let value: Value = serde_json::from_str(&body).unwrap();
                assert!(value.is_object(), "{name}[{i}]: management response schema");
                if status != 200 {
                    assert!(
                        !value["error"].as_str().unwrap().is_empty(),
                        "{name}[{i}]: safe actionable error"
                    );
                    assert!(
                        !body.contains("synthetic-secret")
                            && !body.contains("synthetic-env")
                            && !body.contains("synthetic-local")
                    );
                }
            }
            server.abort();
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}

mod routes {
    use super::*;
    use cliproxy_internal::gateway::watching;

    #[tokio::test]
    async fn management_routes_preflight_and_errors_follow_http_contracts() {
        let dir = std::env::temp_dir().join(format!("cpa-management-routes-{}", std::process::id()));
        let state = super::access::state(&dir, &json!({"secret":"synthetic-secret","allow_remote":false}));
        let (base, server) = super::access::serve(state).await;
        let client = wreq::Client::builder()
            .redirect(wreq::redirect::Policy::none())
            .build()
            .unwrap();
        for (method, path, key, peer, status, version) in [
            ("GET", "/v8/management/unknown", None, "127.0.0.1:1", 404, false),
            (
                "GET",
                "/v8/management/unknown",
                Some("synthetic-secret"),
                "127.0.0.1:1",
                404,
                false,
            ),
            ("POST", "/v8/management/config", None, "127.0.0.1:1", 404, false),
            (
                "HEAD",
                "/v8/management/config",
                Some("synthetic-secret"),
                "127.0.0.1:1",
                404,
                false,
            ),
            ("OPTIONS", "/v8/management/config", None, "203.0.113.1:1", 204, false),
            ("OPTIONS", "/anything/at/all", None, "127.0.0.1:1", 204, false),
            ("GET", "/v8/management/config", None, "127.0.0.1:1", 401, true),
            ("GET", "/v8/management/config", Some("wrong"), "127.0.0.1:1", 401, true),
            (
                "GET",
                "/v8/management",
                Some("synthetic-secret"),
                "127.0.0.1:1",
                404,
                false,
            ),
            (
                "GET",
                "/v8/management/",
                Some("synthetic-secret"),
                "127.0.0.1:1",
                404,
                false,
            ),
            (
                "GET",
                "/v0/management/unknown",
                Some("synthetic-secret"),
                "127.0.0.1:1",
                404,
                true,
            ),
            (
                "GET",
                "/v8/management/credentials/status",
                Some("synthetic-secret"),
                "127.0.0.1:1",
                404,
                false,
            ),
            (
                "PATCH",
                "/v8/management/credentials/status",
                Some("synthetic-secret"),
                "203.0.113.1:1",
                403,
                true,
            ),
            (
                "GET",
                "/v8/management/config/config-version",
                Some("synthetic-secret"),
                "127.0.0.1:1",
                200,
                true,
            ),
            (
                "GET",
                "/v8/management/config/server/missing",
                Some("synthetic-secret"),
                "127.0.0.1:1",
                404,
                true,
            ),
            (
                "GET",
                "/v8/management/config/",
                Some("synthetic-secret"),
                "127.0.0.1:1",
                200,
                true,
            ),
        ] {
            let mut request = client
                .request(method.parse().unwrap(), format!("{base}{path}"))
                .header("X-Test-Peer", peer);
            if let Some(key) = key {
                request = request.bearer_auth(key);
            }
            let response = request.send().await.unwrap();
            assert_eq!(response.status().as_u16(), status, "{method} {path}");
            assert_eq!(response.headers().contains_key("x-cpa-version"), version, "{path}");
            assert_eq!(response.headers()["access-control-allow-origin"], "*");
            let methods = response.headers()["access-control-allow-methods"].to_str().unwrap();
            for supported in ["GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"] {
                assert!(methods.split(',').any(|v| v.trim() == supported));
            }
            let allowed = response.headers()["access-control-allow-headers"].to_str().unwrap();
            assert!(
                allowed
                    .split(',')
                    .any(|v| v.trim().eq_ignore_ascii_case("Authorization"))
            );
            let exposed = response.headers()["access-control-expose-headers"].to_str().unwrap();
            for required in ["x-cpa-version", "retry-after", "location"] {
                assert!(exposed.split(',').any(|v| v.trim().eq_ignore_ascii_case(required)));
            }
            if version {
                assert_eq!(response.headers()["x-cpa-support-plugin"], "0");
                assert!(response.headers().contains_key("x-cpa-commit"));
                assert!(response.headers().contains_key("x-cpa-build-date"));
            }
            if status == 200 {
                assert_eq!(response.headers()["cache-control"], "no-store");
            }
            let body = response.text().await.unwrap();
            if path.ends_with("/config-version") && status == 200 {
                assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), 8);
            } else if path == "/v0/management/unknown" {
                assert!(body.is_empty(), "unknown legacy route has no error payload");
            } else if version && method != "HEAD" {
                let value: Value = serde_json::from_str(&body).unwrap();
                if status >= 400 {
                    assert!(!value["error"].as_str().unwrap().is_empty());
                } else {
                    assert!(value.is_object());
                }
            } else {
                assert!(body.is_empty(), "{method} {path}: unexpected error payload");
            }
        }
        server.abort();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn management_availability_changes_on_reload_with_environment_override() {
        let hash = bcrypt::hash("synthetic-secret", 4).unwrap();
        let client = wreq::Client::new();
        for (name, seed, key, initial_status, after_status) in [
            ("none", json!({"allow_remote":false}), "synthetic-secret", 404, 404),
            (
                "local",
                json!({"allow_remote":false,"local_password":"synthetic-local"}),
                "synthetic-local",
                403,
                404,
            ),
            (
                "environment",
                json!({"allow_remote":false,"env":"synthetic-env"}),
                "synthetic-env",
                200,
                200,
            ),
        ] {
            let dir = std::env::temp_dir().join(format!("cpa-management-reload-{name}-{}", std::process::id()));
            let state = super::access::state(&dir, &seed);
            let (base, server) = super::access::serve(state.clone()).await;
            for (round, expected) in [(0, initial_status), (1, after_status)] {
                if round == 1 {
                    let yaml = format!(
                        "management: {{allow-remote: false}}\noauth: {{auth-dir: {}}}\n",
                        dir.join("auth").display()
                    );
                    std::fs::write(dir.join("config.yaml"), yaml).unwrap();
                    watching::reload(&state).unwrap();
                }
                let response = client
                    .get(format!("{base}/v8/management/config"))
                    .header("X-Test-Peer", "127.0.0.1:1")
                    .bearer_auth(key)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status().as_u16(), expected, "{name}: reload {round}");
            }
            if name == "none" {
                let yaml = format!(
                    "management: {{secret-key: '{hash}'}}\noauth: {{auth-dir: {}}}\n",
                    dir.join("auth").display()
                );
                std::fs::write(dir.join("config.yaml"), yaml).unwrap();
                watching::reload(&state).unwrap();
                let response = client
                    .get(format!("{base}/v8/management/config"))
                    .header("X-Test-Peer", "127.0.0.1:1")
                    .bearer_auth("synthetic-secret")
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status().as_u16(), 200);
                std::fs::write(
                    dir.join("config.yaml"),
                    format!(
                        "management: {{allow-remote: false}}\noauth: {{auth-dir: {}}}\n",
                        dir.join("auth").display()
                    ),
                )
                .unwrap();
                watching::reload(&state).unwrap();
                let response = client
                    .get(format!("{base}/v8/management/config"))
                    .header("X-Test-Peer", "127.0.0.1:1")
                    .bearer_auth("synthetic-secret")
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status().as_u16(), 404);
            }
            server.abort();
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}

mod config_contract {
    use super::*;
    use cliproxy_internal::gateway::management::{Management, Options};
    use cliproxy_internal::providers::{Executors, claude::ClaudeExecutor};
    use std::sync::Arc;

    pub(super) struct Server {
        pub(super) dir: std::path::PathBuf,
        pub(super) path: std::path::PathBuf,
        pub(super) base: String,
        task: tokio::task::JoinHandle<()>,
        pub(super) client: wreq::Client,
        pub(super) runtime: Arc<cliproxy_internal::runtime::Runtime>,
    }

    impl Server {
        async fn start(name: &str, source: &str) -> Self {
            Self::with_files(name, source, &[]).await
        }

        pub(super) async fn with_files(name: &str, source: &str, files: &[(&str, &str)]) -> Self {
            let dir = std::env::temp_dir().join(format!("cpa-config-contract-{name}-{}", std::process::id()));
            std::fs::create_dir_all(dir.join("auth")).unwrap();
            for (name, text) in files {
                std::fs::write(dir.join("auth").join(name), text).unwrap();
            }
            std::fs::create_dir_all(dir.join("logs")).unwrap();
            let path = dir.join("config.yaml");
            let hash = bcrypt::hash("synthetic-secret", 4).unwrap();
            std::fs::write(&path, source.replace("$HASH", &hash)).unwrap();
            let mut cfg = Config::load(&path).unwrap();
            cfg.auth_dir = dir.join("auth");
            let scan = credentials::scan_auth_dir(&cfg);
            let mut loaded = scan.credentials;
            loaded.extend(credentials::from_config(&cfg));
            let runtime = Arc::new(cliproxy_internal::gateway::testing::runtime(
                cfg,
                loaded,
                Executors {
                    claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
                    codex: Default::default(),
                    meta: Default::default(),
                    xai: Default::default(),
                },
            ));
            runtime.store().note_loaded(&scan.read);
            let state = Management::with_options(
                runtime.clone(),
                path.clone(),
                Options {
                    management_password: Some(String::new()),
                    auth_dir: Some(dir.join("auth")),
                    log_dir: Some(dir.join("logs")),
                    log_zone: chrono::FixedOffset::east_opt(0),
                    ..Options::default()
                },
            );
            let (base, task) = super::access::serve(state).await;
            Self {
                dir,
                path,
                base,
                task,
                client: wreq::Client::new(),
                runtime,
            }
        }

        pub(super) async fn request(&self, method: &str, route: &str, body: &str, key: &str) -> (u16, String) {
            let response = self
                .client
                .request(method.parse().unwrap(), format!("{}/v8/management{route}", self.base))
                .header("X-Test-Peer", "127.0.0.1:1")
                .bearer_auth(key)
                .body(body.to_owned())
                .send()
                .await
                .unwrap();
            (response.status().as_u16(), response.text().await.unwrap())
        }

        pub(super) async fn ok(&self, method: &str, route: &str, body: &str) -> Value {
            let (status, answer) = self.request(method, route, body, "synthetic-secret").await;
            assert_eq!(status, 200, "{method} {route}: {answer}");
            serde_json::from_str(&answer).unwrap_or(Value::String(answer))
        }

        pub(super) fn config(&self) -> Config {
            Config::load(&self.path).unwrap()
        }
        pub(super) fn text(&self) -> String {
            std::fs::read_to_string(&self.path).unwrap()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    pub(super) const SEED: &str = "config-version: 8\nmanagement:\n  secret-key: '$HASH'\nserver:\n  port: 8317 # keep listener comment\nrouting:\n  strategy: round-robin\n  retry:\n    request-retry: 2\n";

    #[tokio::test]
    async fn rejected_config_writes_are_transactional_and_identify_the_boundary() {
        let server = Server::start("refusals", SEED).await;
        for (method, route, body, status, field) in [
            ("PUT", "/config/server/port", "\"bad\"", 422, "server.port"),
            ("PUT", "/config/access", "null", 422, "access"),
            ("PUT", "/config/oauth/providers/codex/unknown", "true", 400, ""),
            (
                "PUT",
                "/config/credentials/concurrency/lifecycle-config-revision",
                "5",
                400,
                "",
            ),
            ("PUT", "/config/plugins/auth-revision", "1", 400, ""),
            ("PUT", "/config/access/api-keys/0", "\"x\"", 422, "access.api-keys"),
            ("PUT", "/config/routing/strategy/x", "\"x\"", 400, ""),
            ("PUT", "/config", "[]", 400, ""),
            ("PUT", "/config", "{bad", 400, ""),
            ("PUT", "/config", "", 400, ""),
            ("PUT", "/config.yaml", "# only a comment", 400, ""),
            ("PUT", "/config", "{\"port\":1}", 400, ""),
            ("PUT", "/config/config-version", "7", 422, "config-version"),
            (
                "PUT",
                "/config/api-keys/claude",
                "[{\"keys\":[{\"api-key\":\"synthetic-upstream\",\"weight\":1000001}]}]",
                422,
                "",
            ),
            (
                "PATCH",
                "/config/routing",
                "{\"retry\":{\"request-retry\":\"three\"}}",
                422,
                "routing.retry.request-retry",
            ),
            (
                "PUT",
                "/config/oauth/excluded-models",
                "{\"claude\":\"opus-*\"}",
                422,
                "oauth.excluded-models.claude",
            ),
            (
                "PUT",
                "/config/oauth/model-alias",
                "{\"claude\":[{\"name\":[\"x\"],\"alias\":\"y\"}]}",
                422,
                "oauth.model-alias.claude",
            ),
            (
                "PUT",
                "/config/oauth/settings",
                "{\"claude\":\"x\"}",
                422,
                "oauth.settings.claude",
            ),
            ("PUT", "/config/models/catalog", "\"relative.json\"", 422, ""),
            ("PUT", "/config/models/catalog", "\"file:///tmp/models.json\"", 422, ""),
            ("PUT", "/config/models/catalog", "\"http://x[::1]:80/p\"", 422, ""),
            ("PUT", "/config/models/unknown", "\"x\"", 400, ""),
            (
                "PUT",
                "/config/server/github-token",
                "{\"a\":1}",
                422,
                "server.github-token",
            ),
            ("PUT", "/config/multimedia/disable-image-generation", "\"bad\"", 422, ""),
            ("PATCH", "/config", "{\"<<\":5}", 422, ""),
            (
                "PUT",
                "/config.yaml",
                "config-version: 8\nmanagement: {secret-key: synthetic-secret}\nrouting: {<<: 5}\n",
                400,
                "",
            ),
            (
                "PUT",
                "/config.yaml",
                "config-version: 8\nmanagement: {secret-key: synthetic-secret}\nrequests: {proxy-url: !!binary aHR0cDovL3o=x}\n",
                400,
                "",
            ),
        ] {
            let before = server.text();
            let effective = server.config();
            let (got, answer) = server.request(method, route, body, "synthetic-secret").await;
            assert_eq!(got, status, "{method} {route}: {answer}");
            let error: Value = serde_json::from_str(&answer).unwrap();
            assert!(error["error"].is_string(), "{answer}");
            if !field.is_empty() {
                assert!(answer.contains(field), "{route}: {answer}");
            }
            assert!(!answer.contains("synthetic-upstream"), "credential leaked in error");
            assert_eq!(server.text(), before, "{route}: refused write changed disk");
            assert_eq!(server.config(), effective, "{route}: refused write changed config");
            let live = server.ok("GET", "/config/server/port", "").await;
            assert_eq!(live, 8317, "{route}: refused write changed live config");
        }
    }

    #[tokio::test]
    async fn config_edits_round_trip_and_preserve_unrelated_settings() {
        let server = Server::start("edits", SEED).await;
        server.ok("PUT", "/config/api-keys/claude", r#"[{"name":"team","base-url":"https://claude.example.invalid","priority":3,"keys":[{"api-key":"synthetic-upstream","weight":0}]}]"#).await;
        server.ok("PATCH", "/config/oauth", r#"{"excluded-models":{"claude":["Opus-*"]},"model-alias":{"claude":[{"name":"claude-sonnet-4-6","alias":"sonnet"}]}}"#).await;
        server
            .ok(
                "PUT",
                "/config/access/api-keys",
                r#"["synthetic-client-1","synthetic-client-2"]"#,
            )
            .await;
        server
            .ok(
                "PUT",
                "/config/requests/payload/default",
                r#"[{"models":[{"name":"*","protocol":"openai"}],"params":{"temperature":0.5}}]"#,
            )
            .await;
        server
            .ok("PATCH", "/config/routing/retry", r#"{"max-retry-interval":5}"#)
            .await;
        let cfg = server.config();
        assert_eq!(cfg.port, 8317);
        assert_eq!(cfg.routing.retry.request_retry, 2);
        assert_eq!(cfg.routing.retry.max_retry_interval, 5);
        assert_eq!(cfg.api_keys, ["synthetic-client-1", "synthetic-client-2"]);
        let credentials = credentials::from_config(&cfg);
        assert_eq!(credentials.len(), 1);
        assert_eq!(credentials[0].provider, "claude");
        assert_eq!(
            credentials[0].attributes.get("api_key").map(String::as_str),
            Some("synthetic-upstream")
        );
        assert_eq!(
            cfg.document["requests"]["payload"]["default"][0]["params"]["temperature"],
            0.5
        );
        assert_eq!(cfg.document["oauth"]["model-alias"]["claude"][0]["alias"], "sonnet");
        assert!(server.text().contains("# keep listener comment"));
        server
            .ok("PATCH", "/config", r#"{"routing":{"retry":{"request-retry":null}}}"#)
            .await;
        assert_eq!(server.config().routing.retry.request_retry, 0);
        server
            .ok("DELETE", "/config/routing/retry/max-retry-interval", "")
            .await;
        server.ok("DELETE", "/config/routing/retry/request-retry", "").await;
        assert_eq!(server.config().routing.retry.max_retry_interval, 0);
        server.ok("DELETE", "/config/routing/strategy", "").await;
        assert_eq!(
            server
                .request("DELETE", "/config/routing/strategy", "", "synthetic-secret")
                .await
                .0,
            404
        );
        assert!(
            serde_json::to_value(server.config().document)
                .unwrap()
                .pointer("/routing/retry")
                .is_none_or(Value::is_null)
        );
        assert_eq!(server.ok("GET", "/config/server/port", "").await, 8317);
    }

    #[tokio::test]
    async fn config_nulls_restore_typed_defaults_and_scalar_updates_remain_typed() {
        let server = Server::start("defaults", SEED).await;
        let defaults = Config::parse("").unwrap();
        for (path, value) in [
            ("server/host", json!("synthetic-host")),
            ("server/port", json!(9000)),
            ("routing/retry/request-retry", json!(5)),
            ("routing/session-affinity-ttl", json!(5)),
            ("observability/logs/error-logs-max-files", json!(12)),
            ("observability/usage/redis-usage-queue-retention-seconds", json!(300)),
            ("observability/pprof/addr", json!("127.0.0.1:9001")),
            ("credentials/in-flight/snapshot-interval", json!("3s")),
            ("credentials/concurrency/max-limit", json!(100)),
            ("multimedia/disable-image-generation", json!("off")),
        ] {
            let route = format!("/config/{path}");
            server.ok("PUT", &route, &value.to_string()).await;
            server.ok("PUT", &route, "null").await;
            let cfg = server.config();
            if path == "multimedia/disable-image-generation" {
                assert_eq!(cfg.document["multimedia"]["disable-image-generation"], false);
            } else {
                assert_eq!(
                    serde_json::to_value(&cfg.document)
                        .unwrap()
                        .pointer(&format!("/{path}"))
                        .unwrap_or(&Value::Null),
                    serde_json::to_value(&defaults.document)
                        .unwrap()
                        .pointer(&format!("/{path}"))
                        .unwrap_or(&Value::Null),
                    "{path}"
                );
            }
            if path == "server/port" {
                assert_eq!(cfg.port, defaults.port);
            }
            if path == "routing/retry/request-retry" {
                assert_eq!(cfg.routing.retry.request_retry, defaults.routing.retry.request_retry);
            }
        }
        for (path, spelling, expected) in [
            ("routing/session-affinity-subagents", "off", false),
            ("observability/logs/request-log", "YES", true),
            ("upstream/xai/inject-x-search", "on", true),
        ] {
            server
                .ok("PUT", &format!("/config/{path}"), &json!(spelling).to_string())
                .await;
            assert_eq!(
                serde_json::to_value(server.config().document)
                    .unwrap()
                    .pointer(&format!("/{path}")),
                Some(&json!(expected))
            );
        }
        server
            .ok(
                "PUT",
                "/config/server/host",
                &json!("a<b>&c\u{2028}d\u{2029}").to_string(),
            )
            .await;
        assert_eq!(
            server.ok("GET", "/config/server/host", "").await,
            "a<b>&c\u{2028}d\u{2029}"
        );
        server.ok("PUT", "/config/server/github-token", "5").await;
        assert_eq!(server.config().document["server"]["github-token"], "5");
    }

    #[tokio::test]
    async fn config_uploads_migrate_legacy_and_expand_merges_and_binary_scalars() {
        let legacy = "# owner configuration\nport: 8317 # listener\nremote-management:\n  secret-key: '$HASH'\napi-keys: [synthetic-client]\nrequest-retry: 2\nmystery-section: {nested: [1, 2]}\n";
        let server = Server::start("upload", legacy).await;
        server.ok("PUT", "/config/routing/strategy", "\"fill-first\"").await;
        assert_eq!(server.config().api_keys, ["synthetic-client"]);
        assert_eq!(server.config().routing.retry.request_retry, 2);
        assert_eq!(server.config().routing.strategy, "fill-first");
        assert!(server.text().contains("config-version: 8"));
        assert!(server.text().contains("# mystery-section"));
        assert!(server.text().contains("nested"));
        server.ok("PATCH", "/config", r#"{"<<":{"server":{"port":3}}}"#).await;
        assert_eq!(
            server.config().port,
            8317,
            "explicit value takes precedence over inherited merge"
        );
        let upload = format!(
            "config-version: 8\nmanagement: &m {{secret-key: synthetic-secret}}\noauth: {{auth-dir: {}}}\nrouting:\n  retry: &r {{request-retry: 3}}\n  <<: {{strategy: fill-first}}\nrequests:\n  proxy-url: &p !!binary aHR0cDovL3Byb3h5LmV4YW1wbGUuaW52YWxpZDo4MA==\naccess:\n  api-keys: [!!binary c3ludGhldGljLWNsaWVudA==]\n",
            server.dir.join("auth").display()
        );
        server.ok("PUT", "/config.yaml", &upload).await;
        let cfg = server.config();
        assert_eq!(cfg.routing.strategy, "fill-first");
        assert_eq!(cfg.routing.retry.request_retry, 3);
        assert_eq!(cfg.document["requests"]["proxy-url"], "http://proxy.example.invalid:80");
        assert_eq!(cfg.api_keys, ["synthetic-client"]);
        assert!(bcrypt::verify("synthetic-secret", &cfg.management.secret_key).unwrap());
        server
            .ok("PATCH", "/config/routing/retry", r#"{"max-retry-interval":5}"#)
            .await;
        assert_eq!(server.config().routing.retry.request_retry, 3);
        assert_eq!(server.config().routing.retry.max_retry_interval, 5);
        server
            .ok(
                "PUT",
                "/config/models",
                r#"{"catalog":"/abs/models.json","codex-catalog":"https://catalog.example.invalid/codex.json"}"#,
            )
            .await;
        assert_eq!(
            server.config().catalog_sources(),
            ["/abs/models.json", "https://catalog.example.invalid/codex.json"]
        );
        server.ok("DELETE", "/config/models/codex-catalog", "").await;
        assert_eq!(server.config().catalog_sources(), ["/abs/models.json", ""]);
        server.ok("DELETE", "/config/models", "").await;
        assert_eq!(
            server.request("GET", "/config/models", "", "synthetic-secret").await.0,
            404
        );
        server
            .ok(
                "PUT",
                "/config",
                r#"{"server":{"port":4},"management":{"secret-key":"synthetic-secret"}}"#,
            )
            .await;
        assert_eq!(server.config().port, 4);
        assert!(
            server.config().api_keys.is_empty(),
            "root PUT replaces omitted settings"
        );
        assert_eq!(
            server
                .request("PATCH", "/config.yaml", "{}", "synthetic-secret")
                .await
                .0,
            404
        );
    }

    #[tokio::test]
    async fn management_secrets_are_redacted_preserved_by_identity_and_rotated_as_hashes() {
        let source = format!(
            "{SEED}oauth:\n  providers:\n    codex:\n      live-media-relay:\n        ice-servers:\n          - {{urls: ['turn:a.example.invalid'], username: synthetic-user-a, credential: synthetic-credential-a}}\n          - {{urls: ['turn:b.example.invalid'], username: synthetic-user-b, credential: synthetic-credential-b}}\n"
        );
        let server = Server::start("secrets", &source).await;
        let view = server.ok("GET", "/config", "").await.to_string();
        for secret in [
            "synthetic-secret",
            "synthetic-credential-a",
            "synthetic-credential-b",
            "synthetic-user-a",
            "synthetic-user-b",
        ] {
            assert!(!view.contains(secret), "view leaked {secret}");
        }
        server.ok("PUT", "/config/oauth/providers/codex/live-media-relay/ice-servers", r#"[{"urls":["turn:b.example.invalid"]},{"urls":["turn:c.example.invalid"]},{"urls":["turn:a.example.invalid"],"username":""}]"#).await;
        let cfg = server.config();
        let document = serde_json::to_value(&cfg.document).unwrap();
        let ice = document["oauth"]["providers"]["codex"]["live-media-relay"]["ice-servers"]
            .as_array()
            .unwrap();
        assert_eq!(ice[0]["credential"], "synthetic-credential-b");
        assert_eq!(ice[0]["username"], "synthetic-user-b");
        assert!(ice[1].get("credential").is_none());
        assert_eq!(ice[2]["credential"], "synthetic-credential-a");
        assert_eq!(ice[2]["username"], "");
        server
            .ok("PUT", "/config/management/secret-key", "\"synthetic-rotated\"")
            .await;
        let hash = server.config().management.secret_key;
        assert!(bcrypt::verify("synthetic-rotated", &hash).unwrap());
        assert!(!bcrypt::verify("synthetic-secret", &hash).unwrap());
        assert!(!server.text().contains("synthetic-rotated"));
        assert_eq!(server.request("GET", "/config", "", "synthetic-secret").await.0, 401);
        assert_eq!(server.request("GET", "/config", "", "synthetic-rotated").await.0, 200);
    }
}

mod credential_contract {
    use super::config_contract::{SEED, Server};
    use super::*;

    const CLAUDE: &str = "{\"type\":\"claude\",\"email\":\"operator@example.invalid\",\"access_token\":\"synthetic-token\",\"unknown\":{\"keep\":[1,2]},\"disabled\":false}";
    const CODEX: &str = "{\"type\":\"codex\",\"email\":\"codex@example.invalid\",\"access_token\":\"synthetic-codex\",\"refresh_token\":\"synthetic-refresh\",\"unknown\":{\"keep\":true}}";

    fn auth_snapshot(server: &Server) -> Vec<(String, Vec<u8>)> {
        let mut files: Vec<_> = std::fs::read_dir(server.dir.join("auth"))
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    std::fs::read(entry.path()).unwrap(),
                )
            })
            .collect();
        files.sort();
        files
    }

    fn file(server: &Server, name: &str) -> Value {
        serde_json::from_slice(&std::fs::read(server.dir.join("auth").join(name)).unwrap()).unwrap()
    }

    fn index(server: &Server, name: &str) -> String {
        let credential = server
            .runtime
            .store()
            .snapshot()
            .into_iter()
            .find(|credential| matches!(&credential.source, Source::File(path) if path.file_name().unwrap() == name))
            .unwrap();
        credentials::auth_index(&credential)
    }

    #[tokio::test]
    async fn credential_inventory_filters_paginates_edits_and_deletes_persistently() {
        let server = Server::with_files(
            "credential-inventory",
            SEED,
            &[("a.json", CLAUDE), ("b.json", CODEX), ("ignored.txt", "notes")],
        )
        .await;
        let listing = server.ok("GET", "/credentials", "").await;
        let entries = listing["files"].as_array().unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["a.json", "b.json"]
        );
        assert!(listing["observed_at"].is_string());
        assert!(!listing.to_string().contains("synthetic-token"));
        let codex_index = index(&server, "b.json");
        let selected = server
            .ok("GET", &format!("/credentials?auth_index={codex_index}"), "")
            .await;
        assert_eq!(selected["files"][0]["name"], "b.json");
        assert_eq!(
            server
                .ok("GET", &format!("/credentials?name=a.json&auth_index={codex_index}"), "")
                .await["files"],
            json!([])
        );
        let page = server.ok("GET", "/credentials?page=2&page_size=1", "").await;
        assert_eq!(page["total"], 2);
        assert_eq!(page["files"][0]["name"], "b.json");
        assert_eq!(page["has_more"], false);
        server
            .ok("PATCH", "/credentials/status", r#"{"name":"a.json","disabled":true}"#)
            .await;
        assert_eq!(file(&server, "a.json")["disabled"], true);
        server.ok("PATCH", "/credentials/fields", r#"{"name":"b.json","priority":5,"note":"operator note","headers":{"X-Test":"one"},"request_retry":3,"weight":2,"meta.inner":"value"}"#).await;
        let saved = file(&server, "b.json");
        assert_eq!(saved["priority"], 5);
        assert_eq!(saved["weight"], 2);
        assert_eq!(saved["note"], "operator note");
        assert_eq!(saved["headers"]["X-Test"], "one");
        assert_eq!(saved["request_retry"], 3);
        assert_eq!(saved["meta"]["inner"], "value");
        assert_eq!(saved["unknown"], json!({"keep":true}));
        assert_eq!(saved["access_token"], "synthetic-codex");
        assert_eq!(saved["refresh_token"], "synthetic-refresh");
        let live = server.ok("GET", "/credentials?name=b.json", "").await;
        assert_eq!(live["files"][0]["priority"], 5);
        assert_eq!(live["files"][0]["weight"], 2);
        assert_eq!(live["files"][0]["siwc_status"], "off");
        let definitions = server.ok("GET", "/routing/model-definitions/claude", "").await;
        assert_eq!(definitions["channel"], "claude");
        assert!(!definitions["models"].as_array().unwrap().is_empty());
        let reset = server
            .ok(
                "POST",
                "/routing/cooldown/reset",
                &json!({"auth_index":index(&server,"a.json")}).to_string(),
            )
            .await;
        assert!(reset["models"].is_array());
        server.ok("DELETE", "/credentials?name=a.json", "").await;
        assert!(!server.dir.join("auth/a.json").exists());
        assert_eq!(
            server.ok("GET", "/credentials?name=a.json", "").await["files"],
            json!([])
        );
        server.ok("DELETE", "/credentials?all=true", "").await;
        assert_eq!(server.ok("GET", "/credentials", "").await["files"], json!([]));
        assert!(server.dir.join("auth/ignored.txt").exists());
    }

    #[tokio::test]
    async fn config_credentials_change_status_through_config_without_creating_auth_files() {
        let source = format!(
            "{SEED}api-keys:\n  claude:\n    - name: operator-group\n      base-url: https://claude.example.invalid\n      keys:\n        - api-key: synthetic-config-key\n          prefix: team\n"
        );
        let server = Server::with_files("config-credential-status", &source, &[]).await;
        let credential = server
            .runtime
            .store()
            .snapshot()
            .into_iter()
            .find(|c| matches!(c.source, Source::Config { .. }))
            .unwrap();
        let id = credential.id.clone();
        assert_eq!(server.ok("GET", "/credentials", "").await["files"], json!([]));
        server
            .ok(
                "PATCH",
                "/credentials/status",
                &json!({"name":id,"disabled":true}).to_string(),
            )
            .await;
        assert_eq!(
            credentials::api_key_entries(&server.config(), "claude")[0].excluded_models,
            ["*"]
        );
        assert_eq!(
            credentials::api_key_entries(&server.config(), "claude")[0].api_key,
            "synthetic-config-key"
        );
        let config = server.text();
        assert_eq!(
            server
                .request(
                    "PATCH",
                    "/credentials/fields",
                    &json!({"name":id,"note":"unsupported edit"}).to_string(),
                    "synthetic-secret"
                )
                .await
                .0,
            404
        );
        assert_eq!(server.text(), config);
        server
            .ok(
                "PATCH",
                "/credentials/status",
                &json!({"name":id,"disabled":false}).to_string(),
            )
            .await;
        assert!(
            credentials::api_key_entries(&server.config(), "claude")[0]
                .excluded_models
                .is_empty()
        );
        assert!(auth_snapshot(&server).is_empty());
    }

    #[tokio::test]
    async fn credential_refusals_and_dashboard_probes_have_no_side_effects() {
        let server = Server::with_files("credential-refusals", SEED, &[("a.json", CLAUDE)]).await;
        for (method, route, body, status) in [
            ("GET", "/credentials?page=0", "", 400),
            ("GET", "/credentials?page_size=x", "", 400),
            ("GET", "/credentials/download?name=../outside.json", "", 400),
            ("GET", "/credentials/download?name=x.txt", "", 400),
            ("GET", "/credentials/download?name=missing.json", "", 404),
            ("GET", "/credentials/download", "", 400),
            ("PATCH", "/credentials/status", r#"{"name":"a.json"}"#, 400),
            ("PATCH", "/credentials/status", r#"{}"#, 400),
            (
                "PATCH",
                "/credentials/status",
                r#"{"name":"missing.json","disabled":true}"#,
                404,
            ),
            ("PATCH", "/credentials/fields", r#"{"name":"a.json","weight":"x"}"#, 400),
            ("PATCH", "/credentials/fields", r#"{"name":"a.json"}"#, 400),
            ("PATCH", "/credentials/fields", r#"{}"#, 400),
            (
                "PATCH",
                "/credentials/fields",
                r#"{"name":"missing.json","priority":1}"#,
                404,
            ),
            ("POST", "/credentials?name=bad.json", "{", 400),
            ("POST", "/credentials?name=x.txt", "{}", 400),
            ("POST", "/credentials?name=../evil.json", "{}", 400),
            ("POST", "/credentials?name=removed.json", r#"{"type":"gemini"}"#, 400),
            ("POST", "/credentials", "{}", 400),
            ("DELETE", "/credentials", "{}", 400),
            ("DELETE", "/credentials", r#"["../outside.json"]"#, 400),
            ("POST", "/credentials/refresh", "{}", 400),
            ("POST", "/credentials/refresh", r#"{"name":"missing.json"}"#, 404),
            ("POST", "/routing/cooldown/reset", "{}", 400),
            ("POST", "/routing/cooldown/reset", r#"{"auth_index":"missing"}"#, 404),
            ("POST", "/requests/api-call", "{}", 400),
            ("POST", "/oauth/import", "{}", 400),
            ("GET", "/oauth/auth-url", "", 400),
            ("GET", "/observability/usage/queue?count=0", "", 400),
        ] {
            let before = auth_snapshot(&server);
            let config_before = server.text();
            let revisions: Vec<_> = server
                .runtime
                .store()
                .snapshot()
                .iter()
                .map(|c| (c.id.clone(), c.revision))
                .collect();
            let (got, answer) = server.request(method, route, body, "synthetic-secret").await;
            assert_eq!(got, status, "{method} {route}: {answer}");
            let error: Value = serde_json::from_str(&answer).unwrap();
            assert!(error["error"].is_string());
            assert!(!answer.contains("synthetic-token"));
            assert_eq!(auth_snapshot(&server), before, "{route}: disk changed");
            assert_eq!(server.text(), config_before);
            assert_eq!(
                server
                    .runtime
                    .store()
                    .snapshot()
                    .iter()
                    .map(|c| (c.id.clone(), c.revision))
                    .collect::<Vec<_>>(),
                revisions
            );
        }
    }

    #[tokio::test]
    async fn credential_uploads_preserve_bytes_and_metadata_while_edits_change_requested_fields() {
        let server = Server::with_files("credential-upload", SEED, &[]).await;
        let raw = "{\n  \"type\": \"claude\",\n  \"email\": \"upload@example.invalid\",\n  \"disabled\": false,\n  \"unknown\": {\"nested\": [1, \"<&>\"]}\n}\n";
        server.ok("POST", "/credentials?name=upload.json", raw).await;
        assert_eq!(
            std::fs::read_to_string(server.dir.join("auth/upload.json")).unwrap(),
            raw
        );
        let download = server
            .client
            .get(format!(
                "{}/v8/management/credentials/download?name=upload.json",
                server.base
            ))
            .header("X-Test-Peer", "127.0.0.1:1")
            .bearer_auth("synthetic-secret")
            .send()
            .await
            .unwrap();
        assert_eq!(download.status().as_u16(), 200);
        assert!(
            download.headers()["content-disposition"]
                .to_str()
                .unwrap()
                .contains("upload.json")
        );
        assert_eq!(download.text().await.unwrap(), raw);
        let before = file(&server, "upload.json");
        server
            .ok(
                "PATCH",
                "/credentials/status",
                r#"{"name":"upload.json","disabled":false}"#,
            )
            .await;
        assert_eq!(file(&server, "upload.json"), before);
        server
            .ok(
                "PATCH",
                "/credentials/fields",
                r#"{"name":"upload.json","note":"updated","meta.path":"value","obj":{"b":2,"a":[1,2]},"decimal":1.5}"#,
            )
            .await;
        let saved = file(&server, "upload.json");
        assert_eq!(saved["unknown"], before["unknown"]);
        assert_eq!(saved["note"], "updated");
        assert_eq!(saved["meta"]["path"], "value");
        assert_eq!(saved["obj"], json!({"b":2,"a":[1,2]}));
        assert_eq!(saved["decimal"], 1.5);
        server.ok("POST", "/credentials?name=codex.json", CODEX).await;
        server
            .ok(
                "PATCH",
                "/credentials/fields",
                r#"{"name":"codex.json","priority":7,"note":"updated"}"#,
            )
            .await;
        let saved = file(&server, "codex.json");
        assert_eq!(saved["access_token"], "synthetic-codex");
        assert_eq!(saved["priority"], 7);
        assert_eq!(saved["refresh_token"], "synthetic-refresh");
        assert_eq!(saved["unknown"], json!({"keep":true}));
        let body = "--contract\r\nContent-Disposition: form-data; name=\"file\"; filename=\"multipart.json\"\r\nContent-Type: application/json\r\n\r\n{\"type\":\"claude\",\"note\":\"multipart\"}\r\n--contract\r\nContent-Disposition: form-data; name=\"file\"; filename=\"bad.txt\"\r\n\r\nnotes\r\n--contract--\r\n";
        let response = server
            .client
            .post(format!("{}/v8/management/credentials", server.base))
            .header("X-Test-Peer", "127.0.0.1:1")
            .bearer_auth("synthetic-secret")
            .header("Content-Type", "multipart/form-data; boundary=contract")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 207);
        assert_eq!(file(&server, "multipart.json")["note"], "multipart");
        assert!(!server.dir.join("auth/bad.txt").exists());
        let before = auth_snapshot(&server);
        let malformed = "--contract\r\nContent-Disposition: form-data; name=\"file\"; filename=\"cut.json\"\r\n\r\n{}";
        let response = server
            .client
            .post(format!("{}/v8/management/credentials", server.base))
            .header("X-Test-Peer", "127.0.0.1:1")
            .bearer_auth("synthetic-secret")
            .header("Content-Type", "multipart/form-data; boundary=contract")
            .body(malformed)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 400);
        assert_eq!(auth_snapshot(&server), before);
        let (status, response) = server
            .request(
                "DELETE",
                "/credentials",
                r#"{"names":["upload.json","missing.json"]}"#,
                "synthetic-secret",
            )
            .await;
        assert_eq!(status, 207);
        let response: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["deleted"], 1);
        assert_eq!(response["failed"][0]["name"], "missing.json");
        assert!(!server.dir.join("auth/upload.json").exists());
    }

    #[tokio::test]
    async fn api_call_substitutes_only_the_selected_credential_and_relays_local_upstream() {
        let quoted = r#"{"type":"claude","access_token":"token\"with\\quotes"}"#;
        let server = Server::with_files("api-call", SEED, &[("a.json", CLAUDE), ("quote.json", quoted)]).await;
        let (echo, task) = super::echo::serve().await;
        let response = server.ok("POST", "/requests/api-call", &json!({"auth_index":index(&server,"a.json"),"method":"POST","url":format!("{echo}/echo?q=1"),"header":{"Authorization":"Bearer $TOKEN$","Content-Type":"application/json","X-Custom":"kept"},"data":"{\"token\":\"$TOKEN$\"}"}).to_string()).await;
        assert_eq!(response["status_code"], 200);
        let seen: Value = serde_json::from_str(response["body"].as_str().unwrap()).unwrap();
        assert_eq!(seen["method"], "POST");
        assert_eq!(seen["query"], "q=1");
        assert_eq!(seen["headers"]["Authorization"], json!(["Bearer synthetic-token"]));
        assert_eq!(seen["headers"]["X-Custom"], json!(["kept"]));
        assert_eq!(
            serde_json::from_str::<Value>(seen["body"].as_str().unwrap()).unwrap(),
            json!({"token":"synthetic-token"})
        );
        let quoted_response=server.ok("POST","/requests/api-call",&json!({"auth_index":index(&server,"quote.json"),"method":"POST","url":format!("{echo}/echo"),"data":"{\"token\":\"$TOKEN$\"}"}).to_string()).await;
        let quoted_seen: Value = serde_json::from_str(quoted_response["body"].as_str().unwrap()).unwrap();
        let substituted: Value = serde_json::from_str(quoted_seen["body"].as_str().unwrap()).unwrap();
        assert_eq!(
            substituted["token"],
            serde_json::from_str::<Value>(quoted).unwrap()["access_token"]
        );
        for (path, code, body) in [("/status", 418, "teapot�"), ("/chunked", 200, "parttwo")] {
            let response = server
                .ok(
                    "POST",
                    "/requests/api-call",
                    &json!({"method":"GET","url":format!("{echo}{path}")}).to_string(),
                )
                .await;
            assert_eq!(response["status_code"], code);
            assert_eq!(response["body"], body);
        }
        let redirected = server
            .ok(
                "POST",
                "/requests/api-call",
                &json!({"method":"POST","url":format!("{echo}/redirect307"),"data":"kept"}).to_string(),
            )
            .await;
        let seen: Value = serde_json::from_str(redirected["body"].as_str().unwrap()).unwrap();
        assert_eq!(seen["method"], "POST");
        assert_eq!(seen["body"], "kept");
        for body in [
            json!({"method":"GET","url":"/relative"}),
            json!({"method":"GET","url":"mailto:operator@example.invalid"}),
            json!({"method":"GET","url":format!("{echo}/echo"),"header":{"Authorization":"Bearer $TOKEN$"}}),
            json!({"auth_index":"missing","method":"GET","url":format!("{echo}/echo"),"data":"$TOKEN$"}),
        ] {
            assert_eq!(
                server
                    .request("POST", "/requests/api-call", &body.to_string(), "synthetic-secret")
                    .await
                    .0,
                400
            );
        }
        assert_eq!(
            server
                .request(
                    "POST",
                    "/requests/api-call",
                    &json!({"method":"GET","url":"http://127.0.0.1:1/unreachable"}).to_string(),
                    "synthetic-secret"
                )
                .await
                .0,
            502
        );
        assert!(server.ok("GET", "/observability/usage/api-keys", "").await.is_object());
        assert!(server.ok("GET", "/observability/usage/queue", "").await.is_array());
        assert_eq!(
            server
                .request("GET", "/observability/usage/queue?count=-2", "", "synthetic-secret")
                .await
                .0,
            400
        );
        task.abort();
    }

    #[tokio::test]
    async fn oauth_sessions_validate_callback_identity_and_support_denial_and_cancellation() {
        let server = Server::with_files("oauth-session", SEED, &[]).await;
        for (method, route, body, status) in [
            ("GET", "/oauth/auth-url", "", 400),
            ("GET", "/oauth/auth-url?provider=nope", "", 404),
            ("POST", "/oauth/import?provider=nope", "", 404),
            ("GET", "/oauth/status?state=a/b", "", 400),
            ("DELETE", "/oauth/session", "", 400),
            ("DELETE", "/oauth/session?state=a..b", "", 400),
            ("POST", "/oauth/callback", "{}", 400),
            ("POST", "/oauth/callback", r#"{"state":"x y"}"#, 400),
            ("POST", "/oauth/callback", r#"{"state":"unknown","code":"unused"}"#, 404),
        ] {
            assert_eq!(
                server.request(method, route, body, "synthetic-secret").await.0,
                status,
                "{route}"
            );
        }
        for provider in ["claude", "codex"] {
            let answer = server
                .ok("GET", &format!("/oauth/auth-url?provider={provider}"), "")
                .await;
            let state = answer["state"].as_str().unwrap();
            assert!(!state.is_empty());
            let url = url::Url::parse(answer["url"].as_str().unwrap()).unwrap();
            let pairs: std::collections::HashMap<_, _> = url
                .query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            assert_eq!(pairs["state"], state);
            assert!(!pairs["code_challenge"].is_empty());
            assert_eq!(pairs["code_challenge_method"], "S256");
            if provider == "claude" {
                for scope in ["org:create_api_key", "user:profile", "user:inference"] {
                    assert!(pairs["scope"].split_whitespace().any(|actual| actual == scope));
                }
            }
            let route = format!("/oauth/status?state={state}");
            assert_eq!(server.ok("GET", &route, "").await["status"], "wait");
            if provider == "claude" {
                assert_eq!(
                    server
                        .request(
                            "POST",
                            "/oauth/callback",
                            &json!({"state":state,"provider":"codex","code":"unused"}).to_string(),
                            "synthetic-secret"
                        )
                        .await
                        .0,
                    400
                );
                server
                    .ok(
                        "POST",
                        "/oauth/callback",
                        &json!({"state":state,"error":"access_denied"}).to_string(),
                    )
                    .await;
                let status = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    loop {
                        let status = server.ok("GET", &route, "").await;
                        if status["status"] == "error" {
                            break status;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("denial callback finishes without exchanging tokens");
                assert!(!status["error"].as_str().unwrap().trim().is_empty());
                assert_eq!(
                    server
                        .request(
                            "POST",
                            "/oauth/callback",
                            &json!({"state":state,"code":"unused"}).to_string(),
                            "synthetic-secret"
                        )
                        .await
                        .0,
                    409
                );
            }
            assert_eq!(
                server.ok("DELETE", &format!("/oauth/session?state={state}"), "").await["cancelled"],
                provider == "codex"
            );
            assert_eq!(server.ok("GET", &route, "").await["status"], "error");
            assert_eq!(
                server
                    .request(
                        "POST",
                        "/oauth/callback",
                        &json!({"state":state,"code":"unused"}).to_string(),
                        "synthetic-secret"
                    )
                    .await
                    .0,
                if provider == "claude" { 409 } else { 404 }
            );
        }
        assert!(auth_snapshot(&server).is_empty());
    }

    #[tokio::test]
    async fn application_logs_support_tail_cursor_download_and_clear_without_exposing_other_files() {
        use std::io::Write;
        let source = format!("{SEED}observability:\n  logs:\n    logging-to-file: true\n");
        let server = Server::with_files("logs", &source, &[]).await;
        let logs = server.dir.join("logs");
        std::fs::write(logs.join("main.log.1"), "[2024-01-01 00:00:00] old\n").unwrap();
        std::fs::write(
            logs.join("main.log"),
            "[2024-01-01 00:01:00] first\ncontinuation\n[2024-01-01 00:02:00] second\n",
        )
        .unwrap();
        std::fs::write(logs.join("error-test.log"), "protected error\n").unwrap();
        std::fs::write(logs.join("request-0198ffff-abcd1234.log"), "protected request\n").unwrap();
        std::fs::write(logs.join("private.txt"), "private\n").unwrap();
        let answer = server.ok("GET", "/observability/logs", "").await;
        assert_eq!(
            answer["lines"],
            json!([
                "[2024-01-01 00:00:00] old",
                "[2024-01-01 00:01:00] first",
                "continuation",
                "[2024-01-01 00:02:00] second"
            ])
        );
        let tail = server.ok("GET", "/observability/logs?limit=2", "").await;
        assert_eq!(tail["lines"], json!(["continuation", "[2024-01-01 00:02:00] second"]));
        let cursor = tail["next-cursor"].as_str().unwrap();
        assert!(!cursor.is_empty());
        let mut log = std::fs::OpenOptions::new()
            .append(true)
            .open(logs.join("main.log"))
            .unwrap();
        writeln!(log, "[2024-01-01 00:03:00] appended").unwrap();
        let next = server
            .ok("GET", &format!("/observability/logs?cursor={cursor}"), "")
            .await;
        assert_eq!(next["lines"], json!(["[2024-01-01 00:03:00] appended"]));
        assert_eq!(
            server.ok("GET", "/observability/logs?after=1704067320", "").await["lines"],
            json!(["[2024-01-01 00:03:00] appended"])
        );
        let errors = server.ok("GET", "/observability/logs/errors", "").await;
        assert_eq!(errors["files"][0]["name"], "error-test.log");
        assert_eq!(errors["files"][0]["size"], 16);
        for (route, body) in [
            ("/observability/logs/errors/error-test.log", "protected error\n"),
            ("/observability/logs/requests/0198ffff-abcd1234", "protected request\n"),
        ] {
            let (status, got) = server.request("GET", route, "", "synthetic-secret").await;
            assert_eq!(status, 200);
            assert_eq!(got, body);
        }
        for (route, status) in [
            ("/observability/logs?limit=0", 400),
            ("/observability/logs?limit=abc", 400),
            ("/observability/logs/errors/private.txt", 404),
            ("/observability/logs/requests/a%5Cb", 400),
            ("/observability/logs/requests/missing", 404),
        ] {
            assert_eq!(server.request("GET", route, "", "synthetic-secret").await.0, status);
        }
        let reset = server.ok("GET", "/observability/logs?cursor=not-base64!", "").await;
        assert_eq!(reset["cursor-reset"], true);
        server.ok("DELETE", "/observability/logs", "").await;
        assert_eq!(std::fs::read_to_string(logs.join("main.log")).unwrap(), "");
        assert!(!logs.join("main.log.1").exists());
        assert!(logs.join("private.txt").exists());
        server
            .ok("PUT", "/config/observability/logs/logging-to-file", "false")
            .await;
        assert_eq!(
            server
                .request("GET", "/observability/logs", "", "synthetic-secret")
                .await
                .0,
            400
        );
        assert_eq!(
            server
                .request("DELETE", "/observability/logs", "", "synthetic-secret")
                .await
                .0,
            400
        );
    }
}

/// A local upstream that reports the request it receives and exercises HTTP relay behaviour.
mod echo {
    use axum::body::Bytes;
    use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
    use axum::response::{IntoResponse, Response};
    use serde_json::{Map, Value, json};
    use std::collections::BTreeMap;

    pub(super) async fn serve() -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let own = addr.clone();
        let app = axum::Router::new().fallback(move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
            let own = own.clone();
            async move { handle(&own, method, uri, headers, body) }
        });
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), handle)
    }

    fn handle(own: &str, method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
        match uri.path() {
            "/redirect" => return (StatusCode::FOUND, [(header::LOCATION, "/echo?from=redirect")]).into_response(),
            "/redirect307" => {
                return (StatusCode::TEMPORARY_REDIRECT, [(header::LOCATION, "/echo?from=307")]).into_response();
            }
            "/chunked" => {
                let parts = futures_util::stream::iter([
                    Ok::<_, std::io::Error>(Bytes::from_static(b"part")),
                    Ok(Bytes::from_static(b"two")),
                ]);
                return (
                    [(header::CONTENT_TYPE, "text/plain"), (header::TRAILER, "X-Foo")],
                    axum::body::Body::from_stream(parts),
                )
                    .into_response();
            }
            "/status" => {
                let mut res = (StatusCode::IM_A_TEAPOT, &b"teapot\xff"[..]).into_response();
                let h = res.headers_mut();
                h.insert(header::CONTENT_TYPE, "text/plain".parse().unwrap());
                h.append("x-multi", "a".parse().unwrap());
                h.append("x-multi", "b".parse().unwrap());
                return res;
            }
            _ => {}
        }
        let mut seen = BTreeMap::new();
        for name in [
            "Authorization",
            "X-Custom",
            "Content-Type",
            "User-Agent",
            "Content-Length",
            "Accept-Encoding",
            "Referer",
        ] {
            let values: Vec<Value> = headers
                .get_all(name)
                .iter()
                .map(|v| Value::from(String::from_utf8_lossy(v.as_bytes()).into_owned()))
                .collect();
            if !values.is_empty() {
                seen.insert(name, Value::Array(values));
            }
        }
        let host = headers
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or_default();
        let host = if host == own { "<listener>" } else { host };
        let length: i64 = headers
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok()?.parse().ok())
            .unwrap_or(0);
        // The upstream chooses its own serialization; api-call must relay its bytes.
        let mut out = Map::new();
        out.insert("body".into(), String::from_utf8_lossy(&body).into_owned().into());
        out.insert("content_length".into(), length.into());
        out.insert("headers".into(), json!(seen));
        out.insert("host".into(), host.into());
        out.insert("method".into(), method.as_str().into());
        out.insert("path".into(), uri.path().into());
        out.insert("query".into(), uri.query().unwrap_or_default().into());
        let text = format!("{}\n", Value::Object(out));
        ([(header::CONTENT_TYPE, "application/json")], text).into_response()
    }
}
