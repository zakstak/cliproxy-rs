//! Expected values come from Go (tests/reference/claude/main.go `login` section) or
//! from Go's source rules for the legacy migration.

use super::*;
use axum::response::IntoResponse;
use serde_json::json;
use std::sync::Mutex;

fn golden() -> Value {
    serde_json::from_str(include_str!("claude/testdata/go_executor.json")).unwrap()
}

#[tokio::test]
async fn callback_server_answers_like_go() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, mut rx) = mpsc::channel(1);
    let server = tokio::spawn(serve(listener, tx));
    let client = wreq::Client::builder()
        .redirect(wreq::redirect::Policy::none())
        .no_proxy()
        .build()
        .unwrap();
    for case in golden()["login"]["server"].as_array().unwrap() {
        let target = case["target"].as_str().unwrap();
        let method = wreq::Method::from_bytes(case["method"].as_str().unwrap().as_bytes()).unwrap();
        let response = client
            .request(method, format!("http://{addr}{target}"))
            .send()
            .await
            .unwrap();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .map(|v| v.to_str().unwrap().to_owned())
                .unwrap_or_default()
        };
        assert_eq!(
            response.status().as_u16() as u64,
            case["status"].as_u64().unwrap(),
            "{target}"
        );
        assert_eq!(
            header("content-type"),
            case["content_type"].as_str().unwrap(),
            "{target}"
        );
        assert_eq!(
            header("location"),
            case["location"].as_str().unwrap_or_default(),
            "{target}"
        );
        assert_eq!(
            header("x-content-type-options"),
            case["nosniff"].as_str().unwrap_or_default(),
            "{target}"
        );
        let body = response.text().await.unwrap();
        assert_eq!(body, case["body"].as_str().unwrap(), "{target}");
    }
    // Go keeps only the first result (a buffered channel of one).
    assert_eq!(
        rx.recv().await.unwrap(),
        Callback {
            code: "abc#frag".into(),
            state: "xyz".into(),
            ..Default::default()
        }
    );
    assert!(rx.try_recv().is_err());
    server.abort();
}

#[test]
fn success_page_escapes_the_platform_url() {
    let page = success_html(true, "https://x.invalid/\"><script>");
    assert!(page.contains("https://x.invalid/&#34;&gt;&lt;script&gt;"));
    assert!(!page.contains("<script>\""));
}

#[test]
fn pasted_callbacks_parse_like_go() {
    for case in golden()["login"]["callback_parse"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        let s = |k: &str| case[k].as_str().unwrap_or_default().to_owned();
        match parse_callback_input(input) {
            Ok(None) => assert_eq!(case["nil"], json!(true), "{input}"),
            Ok(Some(c)) => assert_eq!(
                c,
                Callback {
                    code: s("code"),
                    state: s("state"),
                    error: s("error"),
                    description: s("desc"),
                    client_id: String::new(),
                },
                "{input}"
            ),
            // url.Parse error text is Go's own; only its presence is ported.
            Err(message) if s("err").starts_with("parse ") => assert!(message.starts_with("parse "), "{input}"),
            Err(message) => assert_eq!(message, s("err"), "{input}"),
        }
    }
}

#[test]
fn file_names_match_go() {
    for case in golden()["login"]["file_names"].as_array().unwrap() {
        let s = |k: &str| case[k].as_str().unwrap();
        assert_eq!(
            credential_file_name(s("email"), s("organization"), s("account")),
            s("out")
        );
    }
}

/// Token, profile and roles endpoints for the exchange.
async fn oauth_mock() -> OAuth {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = axum::Router::new().fallback(|request: axum::extract::Request| async move {
        match request.uri().path() {
            "/token" => axum::Json(json!({"access_token":"sk-ant-oat-login-fake","refresh_token":"fake-r",
                "expires_in":3600,"account":{"uuid":"acct-1","email_address":"a@example.invalid"},
                "organization":{"uuid":"org-1","name":"Org"}}))
            .into_response(),
            "/profile" => axum::Json(json!({"account":{"uuid":"acct-1","email":"a@example.invalid"},
                "organization":{"uuid":"org-1","name":"Org"}}))
            .into_response(),
            _ => axum::Json(json!({})).into_response(),
        }
    });
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    OAuth::with_endpoints(
        wreq::Client::builder().no_proxy().build().unwrap(),
        &format!("{base}/token"),
        &format!("{base}/profile"),
        &format!("{base}/roles"),
    )
}

/// The callback listener on a port the kernel picked, bound once and handed to the
/// login: probing a free port and binding it again races parallel tests.
async fn callback() -> (tokio::net::TcpListener, u16) {
    let listener = bind(0).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cpa-claude-login-{}", random_hex(8).unwrap()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn state_of(url: &str) -> String {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned()
}

/// An interaction whose "browser" calls the callback with `state(url)`.
fn browser(port: u16, state: fn(&str) -> String) -> Interaction {
    Interaction {
        manual_delay: Duration::from_secs(3600),
        prompt: None,
        show_url: Box::new(move |url| {
            let target = format!("http://127.0.0.1:{port}/callback?code=fake-code&state={}", state(url));
            tokio::spawn(async move {
                let client = wreq::Client::builder().no_proxy().build().unwrap();
                let _ = client.get(target).send().await;
            });
        }),
    }
}

#[tokio::test]
async fn browser_login_writes_go_file_and_migrates_the_legacy_one() {
    let dir = temp_dir();
    // Email-only legacy file of the same organization: merged, then removed. Another
    // email's legacy file stays.
    let legacy = dir.join("claude-a@example.invalid.json");
    std::fs::write(
        &legacy,
        r#"{"type":"claude","email":"a@example.invalid","organization_uuid":"org-1","proxy_url":"socks5://keep:1","access_token":"old","note":"<kept>"}"#,
    )
    .unwrap();
    let other = dir.join("claude-b@example.invalid.json");
    std::fs::write(
        &other,
        r#"{"type":"claude","email":"b@example.invalid","organization_uuid":"org-1"}"#,
    )
    .unwrap();
    let mock = oauth_mock().await;
    let (listener, port) = callback().await;
    let path = login_with(&dir, listener, mock, browser(port, state_of)).await.unwrap();
    assert_eq!(
        path,
        dir.join(credential_file_name("a@example.invalid", "org-1", "acct-1"))
    );
    let written = std::fs::read(&path).unwrap();
    let value: Value = serde_json::from_slice(&written).unwrap();
    assert_eq!(
        value["access_token"], "sk-ant-oat-login-fake",
        "token keys never come from the legacy file"
    );
    assert_eq!(value["proxy_url"], "socks5://keep:1");
    assert_eq!(value["organization_uuid"], "org-1");
    // encoding/json: sorted keys, HTML-escaped strings, trailing newline.
    let text = String::from_utf8(written).unwrap();
    assert!(text.ends_with("}\n"));
    assert!(text.contains(r#""note":"\u003ckept\u003e""#));
    let keys: Vec<&String> = value.as_object().unwrap().keys().collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
    assert!(!legacy.exists());
    assert!(other.exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn legacy_matching_follows_go_identity_rules() {
    let dir = temp_dir();
    let target: BTreeMap<String, Value> = [
        ("email", "a@example.invalid"),
        ("organization_uuid", "org-1"),
        ("account_uuid", "acct-1"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), json!(v)))
    .collect();
    let name = credential_file_name("a@example.invalid", "org-1", "acct-1");
    // An organization target ignores an email-only file without that organization...
    let email_only = dir.join("claude-a@example.invalid.json");
    std::fs::write(&email_only, r#"{"type":"claude","account_uuid":"acct-1"}"#).unwrap();
    assert_eq!(legacy_credential(&dir, &name, &target), None);
    // ...but takes the account-hashed predecessor of the same account.
    let predecessor = dir.join(credential_file_name("a@example.invalid", "", "acct-1"));
    std::fs::write(&predecessor, r#"{"type":"claude","account_uuid":"ACCT-1"}"#).unwrap();
    assert_eq!(legacy_credential(&dir, &name, &target), Some(predecessor.clone()));
    std::fs::remove_file(&predecessor).unwrap();
    // An account-only target takes the email-only file of the same account.
    let account_target: BTreeMap<String, Value> = target
        .iter()
        .filter(|(k, _)| *k != "organization_uuid")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let account_name = credential_file_name("a@example.invalid", "", "acct-1");
    assert_eq!(
        legacy_credential(&dir, &account_name, &account_target),
        Some(email_only)
    );
    // Non-Claude files never match.
    std::fs::write(
        dir.join("claude-a@example.invalid.json"),
        r#"{"type":"codex","account_uuid":"acct-1"}"#,
    )
    .unwrap();
    assert_eq!(legacy_credential(&dir, &account_name, &account_target), None);
    // File names compare with Go's EqualFold, which folds non-ASCII letters too.
    let greek_target: BTreeMap<String, Value> = [("email", "δ@example.invalid"), ("account_uuid", "acct-1")]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), json!(v)))
        .collect();
    let greek_file = dir.join("claude-Δ@example.invalid.json");
    std::fs::write(&greek_file, r#"{"type":"claude","account_uuid":"acct-1"}"#).unwrap();
    let greek_name = credential_file_name("δ@example.invalid", "", "acct-1");
    assert_eq!(legacy_credential(&dir, &greek_name, &greek_target), Some(greek_file));
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_wrong_state_fails_the_login_without_writing() {
    let dir = temp_dir();
    let mock = oauth_mock().await;
    let (listener, port) = callback().await;
    let error = login_with(&dir, listener, mock, browser(port, |_| "forged".into()))
        .await
        .unwrap_err();
    assert_eq!(
        error.body,
        "invalid_state: OAuth state parameter is invalid (caused by: state mismatch)"
    );
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn pasted_callback_completes_the_login() {
    let dir = temp_dir();
    let url: Arc<Mutex<String>> = Arc::default();
    let seen = url.clone();
    let interaction = Interaction {
        manual_delay: Duration::from_millis(20),
        prompt: Some(Box::new(move || {
            Box::pin(async move {
                let state = state_of(&seen.lock().unwrap());
                Ok(format!("localhost:54545/callback?code=fake-code&state={state}\n"))
            })
        })),
        show_url: Box::new(move |u| *url.lock().unwrap() = u.to_owned()),
    };
    let mock = oauth_mock().await;
    let (listener, _) = callback().await;
    let path = login_with(&dir, listener, mock, interaction).await.unwrap();
    assert!(path.exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn an_empty_paste_keeps_waiting_for_the_browser() {
    let dir = temp_dir();
    let mock = oauth_mock().await;
    let (listener, port) = callback().await;
    let mut interaction = browser(port, state_of);
    let show = interaction.show_url;
    interaction.manual_delay = Duration::from_millis(1);
    interaction.prompt = Some(Box::new(|| Box::pin(async { Ok("\n".to_owned()) })));
    interaction.show_url = Box::new(move |url| {
        // The browser arrives after the empty paste.
        let url = url.to_owned();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            show(&url);
        });
    });
    assert!(login_with(&dir, listener, mock, interaction).await.is_ok());
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_busy_port_is_go_port_in_use() {
    // The realistic holder: another Go or cliproxy login's dual-stack listener.
    let holder = listen_any(0).unwrap();
    let port = holder.local_addr().unwrap().port();
    let error = bind(port).await.unwrap_err();
    assert!(String::from_utf8_lossy(&error.body).starts_with(PORT_IN_USE));
    // A holder on one family: the dual-stack socket conflicts with it.
    for host in ["::", "127.0.0.1"] {
        let holder = tokio::net::TcpListener::bind((host, 0)).await.unwrap();
        let port = holder.local_addr().unwrap().port();
        let result = bind(port).await;
        assert!(
            String::from_utf8_lossy(&result.unwrap_err().body).starts_with(PORT_IN_USE),
            "{host}"
        );
    }
}

/// #76 (C8): the authorization scopes (Claude Code 2.1.322's `fHo()` without `user:plugins`,
/// as Go ea0b4712 sends them), written out here instead of read from the constant under test.
const CLAUDE_CODE_AUTHORIZE_SCOPE: &str =
    "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

/// Asserts a login state is 256 random bits as unpadded base64url (43 characters).
fn assert_256_bit_base64url(state: &str) {
    use base64::Engine as _;
    assert_eq!(state.len(), 43, "{state}");
    assert!(
        state
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "{state}"
    );
    assert_eq!(
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(state)
            .unwrap()
            .len(),
        32
    );
}

/// A fake token endpoint that records each request body.
async fn recording_oauth_mock() -> (OAuth, Arc<Mutex<Vec<Value>>>) {
    let bodies: Arc<Mutex<Vec<Value>>> = Arc::default();
    let seen = bodies.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = axum::Router::new().fallback(move |request: axum::extract::Request| {
        let seen = seen.clone();
        async move {
            let path = request.uri().path().to_owned();
            let body = axum::body::to_bytes(request.into_body(), 1 << 20).await.unwrap();
            match path.as_str() {
                "/token" => {
                    seen.lock().unwrap().push(serde_json::from_slice(&body).unwrap());
                    axum::Json(json!({"access_token":"sk-ant-oat-login-fake","refresh_token":"fake-r",
                        "expires_in":3600,"account":{"uuid":"acct-1","email_address":"a@example.invalid"},
                        "organization":{"uuid":"org-1","name":"Org"}}))
                    .into_response()
                }
                "/profile" => axum::Json(json!({"account":{"uuid":"acct-1","email":"a@example.invalid"},
                    "organization":{"uuid":"org-1","name":"Org"}}))
                .into_response(),
                _ => axum::Json(json!({})).into_response(),
            }
        }
    });
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let oauth = OAuth::with_endpoints(
        wreq::Client::builder().no_proxy().build().unwrap(),
        &format!("{base}/token"),
        &format!("{base}/profile"),
        &format!("{base}/roles"),
    );
    (oauth, bodies)
}

/// #76 (C8): a browser login authorizes with the Console scope and a 256-bit base64url
/// state, exchanges the code with exactly Claude Code's form and that state (no scope),
/// and logs none of the state, verifier, code or tokens.
#[tokio::test]
async fn login_uses_console_scope_256_bit_state_and_logs_no_secret() {
    let capture = crate::device_fixture::LogCapture::default();
    let _guard = capture.install();
    let dir = temp_dir();
    let (mock, bodies) = recording_oauth_mock().await;
    let (listener, port) = callback().await;
    let shown: Arc<Mutex<String>> = Arc::default();
    let mut interaction = browser(port, state_of);
    let show = interaction.show_url;
    let record = shown.clone();
    interaction.show_url = Box::new(move |url| {
        *record.lock().unwrap() = url.to_owned();
        show(url);
    });
    login_with(&dir, listener, mock, interaction).await.unwrap();
    let url = url::Url::parse(&shown.lock().unwrap()).unwrap();
    let query: BTreeMap<String, String> = url.query_pairs().into_owned().collect();
    assert_eq!(query["scope"], CLAUDE_CODE_AUTHORIZE_SCOPE);
    let state = &query["state"];
    assert_256_bit_base64url(state);
    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies.len(), 1);
    let exchange = bodies[0].as_object().unwrap();
    let mut keys: Vec<&str> = exchange.keys().map(String::as_str).collect();
    keys.sort_unstable();
    let mut want = vec![
        "client_id",
        "code",
        "code_verifier",
        "grant_type",
        "redirect_uri",
        "state",
    ];
    want.sort_unstable();
    assert_eq!(keys, want, "no scope on the code exchange");
    assert_eq!(exchange["grant_type"], "authorization_code");
    assert_eq!(exchange["code"], "fake-code");
    assert_eq!(exchange["state"], state.as_str());
    assert_eq!(exchange["redirect_uri"], "http://localhost:54545/callback");
    assert_eq!(exchange["client_id"], "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
    let verifier = exchange["code_verifier"].as_str().unwrap().to_owned();
    let logs = capture.0.lock().unwrap().concat();
    for secret in [
        state.as_str(),
        &verifier,
        "fake-code",
        "sk-ant-oat-login-fake",
        "fake-r",
    ] {
        assert!(!logs.contains(secret), "{secret} reached the log");
    }
    std::fs::remove_dir_all(dir).unwrap();
}

/// #76 (C8): the callback state is compared byte for byte, so a state that differs
/// only in letter case is a mismatch and nothing is written.
#[tokio::test]
async fn a_state_differing_only_in_case_fails_the_login() {
    let dir = temp_dir();
    let mock = oauth_mock().await;
    let (listener, port) = callback().await;
    let swapped = |url: &str| {
        let state = state_of(url);
        let swapped: String = state
            .chars()
            .map(|c| {
                if c.is_ascii_lowercase() {
                    c.to_ascii_uppercase()
                } else {
                    c.to_ascii_lowercase()
                }
            })
            .collect();
        if swapped == state { format!("{state}A") } else { swapped }
    };
    let error = login_with(&dir, listener, mock, browser(port, swapped))
        .await
        .unwrap_err();
    assert_eq!(
        error.body,
        "invalid_state: OAuth state parameter is invalid (caused by: state mismatch)"
    );
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    std::fs::remove_dir_all(dir).unwrap();
}

/// #76 (C8): a management login hands out the same 256-bit base64url state it puts in its
/// authorization URL, asks for the Console scope, and exchanges the code with that state
/// and no scope.
#[tokio::test]
async fn managed_login_hands_out_a_256_bit_state_and_the_console_scope() {
    let login = ManagedLogin::start().unwrap();
    assert_256_bit_base64url(&login.state);
    let url = url::Url::parse(&login.url).unwrap();
    let query: BTreeMap<String, String> = url.query_pairs().into_owned().collect();
    assert_eq!(query["state"], login.state);
    assert_eq!(query["scope"], CLAUDE_CODE_AUTHORIZE_SCOPE);
    assert_ne!(ManagedLogin::start().unwrap().state, login.state);
    let (mock, bodies) = recording_oauth_mock().await;
    login.exchange_with(&mock, "fake-code#ignored").await.unwrap();
    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["state"], login.state.as_str());
    assert_eq!(bodies[0]["code"], "fake-code");
    assert!(bodies[0].get("scope").is_none(), "no scope on the code exchange");
}
