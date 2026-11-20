//! OAuth boundary tests use hand-authored claims and local token endpoints.
//! Grant forms follow OAuth 2.0 (RFC 6749 sections 4.1.3 and 6), PKCE follows
//! RFC 7636, and permanent refresh refusals follow the pinned Codex contract.
//! Credential names and fields are the existing saved-login compatibility contract.

use super::*;
use crate::providers::codex_testkit::{Captured, Mock, json};
use std::path::Path;

fn claim_token(claims: Value) -> String {
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    format!(
        "{}.{}.signature",
        engine.encode(r#"{"alg":"none"}"#),
        engine.encode(serde_json::to_vec(&claims).unwrap())
    )
}

fn id_token() -> String {
    claim_token(json!({
        "email":"login@example.invalid",
        "https://api.openai.com/auth": {
            "chatgpt_account_id":"account-contract",
            "chatgpt_plan_type":"Team"
        }
    }))
}

fn token_body() -> String {
    json!({"access_token":"at-FAKE","refresh_token":"rt-FAKE-2","id_token":id_token(),"token_type":"Bearer","expires_in":3600}).to_string()
}

fn assert_grant_request(request: &Captured, fields: &[(&str, &str)]) {
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/oauth/token");
    let form: BTreeMap<String, String> = url::form_urlencoded::parse(&request.body).into_owned().collect();
    let expected: BTreeMap<String, String> = fields.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    assert_eq!(form, expected);
    assert_eq!(request.header("content-type"), "application/x-www-form-urlencoded");
    assert_eq!(request.header("accept"), "application/json");
    assert_eq!(
        request.header("user-agent"),
        crate::providers::codex_identity::USER_AGENT
    );
    assert!(
        request.header("authorization").is_empty(),
        "no client or provider bearer belongs on a grant form"
    );
}

fn assert_tokens(tokens: &Tokens) {
    assert_eq!(tokens.access_token, "at-FAKE");
    assert_eq!(tokens.refresh_token, "rt-FAKE-2");
    assert_eq!(tokens.id_token, id_token());
    assert_eq!(tokens.account_id, "account-contract");
    assert_eq!(tokens.email, "login@example.invalid");
    assert_eq!(tokens.plan_type, "Team");
    assert!(DateTime::parse_from_rfc3339(&tokens.expired).is_ok());
}

async fn oauth(mock: &Mock) -> CodexOAuth {
    CodexOAuth::with_endpoints(
        wreq::Client::new(),
        &format!("{}/oauth/token", mock.url),
        &format!("{}/api/accounts/deviceauth/usercode", mock.url),
        &format!("{}/api/accounts/deviceauth/token", mock.url),
    )
    .with_retry_delay(Duration::from_millis(1))
}

#[test]
fn typed_claims_and_saved_names_preserve_account_identity() {
    let claims = parse_jwt(&id_token()).unwrap();
    assert_eq!(claims.email, "login@example.invalid");
    assert_eq!(claims.account_id, "account-contract");
    assert_eq!(claims.plan_type(), "Team");
    for payload in [
        json!({}),
        json!({"email":null,"https://api.openai.com/auth":null}),
        json!({"https://api.openai.com/auth":{"chatgpt_plan_type":"  "}}),
    ] {
        assert_eq!(parse_jwt(&claim_token(payload)).unwrap().plan_type(), "free");
    }
    // Untrusted token metadata is decoded at the boundary, never coerced to the
    // wrong Rust type. These are parser safety cases, not provider acceptance claims.
    for payload in [
        json!({"email":false}),
        json!({"aud":"invalid-audience-type"}),
        json!({"auth_time":1.5}),
        json!({"https://api.openai.com/auth":{"chatgpt_subscription_last_checked":"yesterday"}}),
    ] {
        assert!(parse_jwt(&claim_token(payload.clone())).is_none(), "{payload}");
    }
    for malformed in ["", "not-a-jwt", "a.b", "a.%%%.signature"] {
        assert!(parse_jwt(malformed).is_none());
    }
    assert!(
        parse_jwt(&claim_token(
            json!({"https://api.openai.com/auth":{"chatgpt_subscription_last_checked":"2026-01-01T00:00:00Z"}})
        ))
        .is_some()
    );
    for (email, plan, hash, name) in [
        (
            " person@example.invalid ",
            "Team Plus",
            "account",
            "codex-account-person@example.invalid-team-plus.json",
        ),
        (
            "person@example.invalid",
            "",
            "account",
            "codex-account-person@example.invalid.json",
        ),
        (
            "person@example.invalid",
            "Team_ENTERPRISE",
            "",
            "codex-person@example.invalid-team-enterprise.json",
        ),
        ("person@example.invalid", "--", "", "codex-person@example.invalid.json"),
    ] {
        assert_eq!(credential_file_name(email, plan, hash), name);
    }
}

#[test]
fn authorize_request_binds_pkce_state_and_callback_without_query_order_requirements() {
    let url = Url::parse(&authorize_url("state +/=", "challenge-FAKE", 1456)).unwrap();
    assert_eq!(url.origin().ascii_serialization(), "https://auth.openai.com");
    assert_eq!(url.path(), "/oauth/authorize");
    let params: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    for (key, expected) in [
        ("client_id", CLIENT_ID),
        ("state", "state +/="),
        ("code_challenge", "challenge-FAKE"),
        ("code_challenge_method", "S256"),
        ("response_type", "code"),
        ("redirect_uri", "http://localhost:1456/auth/callback"),
        ("scope", "openid email profile offline_access"),
        ("prompt", "login"),
        ("codex_cli_simplified_flow", "true"),
        ("id_token_add_organizations", "true"),
    ] {
        assert_eq!(params.get(key).map(String::as_str), Some(expected), "{key}");
    }
}

#[test]
fn login_record_contains_tokens_account_and_timestamps_as_json_values() {
    let tokens = Tokens {
        id_token: id_token(),
        access_token: "at-<FAKE>&".into(),
        refresh_token: "rt-FAKE-2".into(),
        account_id: "account-contract".into(),
        email: "login@example.invalid".into(),
        plan_type: "Team".into(),
        expired: "2026-10-12T12:00:00Z".into(),
    };
    let (name, record) = login_record(&tokens, "2026-10-02T12:00:00Z").unwrap();
    // SHA-256's first four account bytes are a saved-file identity convention.
    assert_eq!(name, "codex-4c9636ca-login@example.invalid-team.json");
    let saved: Value = serde_json::from_str(&encode_go_json(&Value::Object(record))).unwrap();
    assert_eq!(
        saved,
        json!({
            "type":"codex","id_token":id_token(),"access_token":"at-<FAKE>&","refresh_token":"rt-FAKE-2",
            "account_id":"account-contract","email":"login@example.invalid","plan_type":"Team",
            "expired":"2026-10-12T12:00:00Z","last_refresh":"2026-10-02T12:00:00Z","disabled":false
        })
    );
}

#[tokio::test]
async fn existing_file_keeps_user_fields_but_never_old_tokens() {
    let dir = std::env::temp_dir().join(format!("codex-login-{}", random_state().unwrap()));
    let tokens = Tokens {
        id_token: String::new(),
        access_token: "at-new".into(),
        refresh_token: "rt-new".into(),
        account_id: String::new(),
        email: "user@example.invalid".into(),
        plan_type: String::new(),
        expired: "2026-10-12T12:00:00Z".into(),
    };
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("codex-user@example.invalid-free.json");
    std::fs::write(
        &path,
        r#"{"access_token":"at-old","prefix":"team","disabled":true,"note":"mine"}"#,
    )
    .unwrap();
    let written = write_login(&dir, &tokens).unwrap();
    assert_eq!(written, path);
    let saved: Map<String, Value> = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["access_token"], "at-new");
    assert_eq!(saved["prefix"], "team");
    assert_eq!(saved["note"], "mine");
    assert_eq!(
        saved["disabled"], true,
        "re-login keeps an explicitly disabled account disabled"
    );
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn exchange_and_refresh_send_typed_oauth_grants_and_parse_token_identity() {
    let mock = Mock::start().await;
    let oauth = oauth(&mock).await;
    mock.script("/oauth/token", vec![json(200, &token_body())]);
    let tokens = oauth
        .exchange("code +/=", &redirect_uri(DEFAULT_CALLBACK_PORT), "verifier +/=")
        .await
        .unwrap();
    assert_grant_request(
        &mock.take()[0],
        &[
            ("client_id", CLIENT_ID),
            ("code", "code +/="),
            ("code_verifier", "verifier +/="),
            ("grant_type", "authorization_code"),
            ("redirect_uri", "http://localhost:1455/auth/callback"),
        ],
    );
    assert_tokens(&tokens);
    mock.script("/oauth/token", vec![json(200, &token_body())]);
    let tokens = oauth.refresh("refresh +/=").await.unwrap();
    assert_grant_request(
        &mock.take()[0],
        &[
            ("client_id", CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", "refresh +/="),
            ("scope", "openid profile email"),
        ],
    );
    assert_tokens(&tokens);
}

#[tokio::test]
async fn transient_refresh_retries_but_a_permanently_refused_grant_is_sent_once() {
    // CONTRACT.md: request_chatgpt_token_refresh at the first-party pin treats
    // invalid_grant and refresh_token_reused as permanent, not retryable failures.
    let mock = Mock::start().await;
    for (code, replies, attempts, ok) in [
        (
            "transient",
            vec![json(500, r#"{"error":"server_error"}"#), json(200, &token_body())],
            2,
            true,
        ),
        (
            "refresh_token_reused",
            vec![
                json(400, r#"{"error":"refresh_token_reused"}"#),
                json(200, &token_body()),
            ],
            1,
            false,
        ),
        (
            "invalid_grant",
            vec![json(400, r#"{"error":"invalid_grant"}"#); 3],
            1,
            false,
        ),
    ] {
        let oauth = oauth(&mock).await;
        mock.script("/oauth/token", replies);
        let result = oauth.refresh(code).await;
        let requests = mock.take();
        assert_eq!(requests.len(), attempts, "{code}");
        for request in &requests {
            assert_grant_request(
                request,
                &[
                    ("client_id", CLIENT_ID),
                    ("grant_type", "refresh_token"),
                    ("refresh_token", code),
                    ("scope", "openid profile email"),
                ],
            );
        }
        if ok {
            assert_tokens(&result.unwrap());
        } else {
            let error = result.unwrap_err();
            assert_eq!(error.status, 401);
            assert_eq!(error.scope, FailureScope::Credential);
            assert!(!error.body.is_empty());
            assert!(!String::from_utf8_lossy(&error.body).contains("rt-"));
        }
    }
}

#[tokio::test]
async fn malformed_token_response_is_a_safe_upstream_failure() {
    let mock = Mock::start().await;
    let oauth = oauth(&mock).await;
    mock.script(
        "/oauth/token",
        vec![json(
            200,
            r#"{"access_token":"at-SECRET-FAKE","expires_in":"rt-SECRET-FAKE"}"#,
        )],
    );
    let error = oauth
        .exchange("code-FAKE", &redirect_uri(DEFAULT_CALLBACK_PORT), "verifier-FAKE")
        .await
        .unwrap_err();
    let message = String::from_utf8_lossy(&error.body);
    assert!(!message.is_empty());
    assert!(!message.contains("at-SECRET-FAKE") && !message.contains("rt-SECRET-FAKE"));
    assert_eq!(error.status, 502);
}

#[tokio::test]
async fn concurrent_refreshes_of_one_token_share_one_exchange() {
    let mock = Mock::start().await;
    let oauth = oauth(&mock).await;
    mock.script("/oauth/token", vec![json(200, &token_body())]);
    let results = futures_util::future::join_all((0..8).map(|_| oauth.refresh("rt-shared"))).await;
    assert!(results.iter().all(|r| r.as_ref().unwrap().refresh_token == "rt-FAKE-2"));
    assert_eq!(mock.take().len(), 1);
}

#[tokio::test]
async fn device_flow_polls_pending_then_exchanges_the_authorization_code() {
    let mock = Mock::start().await;
    let oauth = oauth(&mock).await;
    mock.script(
        "/api/accounts/deviceauth/usercode",
        vec![json(
            200,
            r#"{"device_auth_id":"dev-FAKE","usercode":"ABCD-EFGH","interval":"1"}"#,
        )],
    );
    mock.script(
        "/api/accounts/deviceauth/token",
        vec![
            json(403, "{}"),
            json(
                200,
                r#"{"authorization_code":"devcode-FAKE","code_verifier":"devverifier-FAKE","code_challenge":"devchallenge"}"#,
            ),
        ],
    );
    mock.script("/oauth/token", vec![json(200, &token_body())]);
    let code = oauth.device_code().await.unwrap();
    assert_eq!(code.interval, Duration::from_secs(1));
    let tokens = oauth.device_poll(&code, Duration::from_secs(5)).await.unwrap();
    assert_tokens(&tokens);
    let captured = mock.take();
    assert_eq!(
        captured.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
        [
            "/api/accounts/deviceauth/usercode",
            "/api/accounts/deviceauth/token",
            "/api/accounts/deviceauth/token",
            "/oauth/token"
        ]
    );
    for request in &captured[..3] {
        assert_eq!(request.method, "POST");
        assert_eq!(request.header("content-type"), "application/json");
        assert_eq!(
            request.header("user-agent"),
            crate::providers::codex_identity::USER_AGENT
        );
        assert!(request.header("authorization").is_empty());
    }
    assert_eq!(
        serde_json::from_slice::<Value>(&captured[0].body).unwrap(),
        json!({"client_id":CLIENT_ID})
    );
    for request in &captured[1..3] {
        assert_eq!(
            serde_json::from_slice::<Value>(&request.body).unwrap(),
            json!({"device_auth_id":"dev-FAKE","user_code":"ABCD-EFGH"})
        );
    }
    assert_grant_request(
        &captured[3],
        &[
            ("client_id", CLIENT_ID),
            ("code", "devcode-FAKE"),
            ("code_verifier", "devverifier-FAKE"),
            ("grant_type", "authorization_code"),
            ("redirect_uri", "https://auth.openai.com/deviceauth/callback"),
        ],
    );
    let (name, record) = login_record(&tokens, "t").unwrap();
    assert_eq!(name, "codex-4c9636ca-login@example.invalid-team.json");
    assert_eq!(record["plan_type"], "Team");
}

#[tokio::test]
async fn refresh_patch_replaces_tokens_and_retains_unrelated_saved_fields() {
    let mock = Mock::start().await;
    let oauth = oauth(&mock).await;
    mock.script("/oauth/token", vec![json(200, &token_body())]);
    let mut metadata: Map<String, Value> = serde_json::from_value(json!({
        "type": "codex", "refresh_token": "rt-FAKE-1", "access_token": "old",
        "email": "old@example.invalid", "custom": "kept"
    }))
    .unwrap();
    oauth.refresh_patch("rt-FAKE-1").await.unwrap().apply(&mut metadata);
    for key in ["expired", "last_refresh"] {
        let value = metadata[key].as_str().unwrap();
        assert!(DateTime::parse_from_rfc3339(value).is_ok(), "{key}={value}");
        metadata.insert(key.into(), "<time>".into());
    }
    assert_eq!(
        Value::Object(metadata),
        json!({
            "type":"codex","refresh_token":"rt-FAKE-2","access_token":"at-FAKE","id_token":id_token(),
            "account_id":"account-contract","email":"login@example.invalid","plan_type":"Team", "custom":"kept",
            "expired":"<time>","last_refresh":"<time>"
        })
    );
}

fn credential(meta: Value) -> Credential {
    Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/codex.json"),
        meta.as_object().unwrap().clone(),
    )
    .unwrap()
}

fn jwt_with_exp(exp: i64) -> String {
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    format!(
        "{}.{}.sig",
        engine.encode(r#"{"alg":"none"}"#),
        engine.encode(format!(r#"{{"exp":{exp}}}"#))
    )
}

#[test]
fn refresh_due_uses_jwt_exp_before_metadata_and_a_96h_lead() {
    let now = Utc::now();
    let soon = (now + chrono::TimeDelta::hours(95)).timestamp();
    let later = (now + chrono::TimeDelta::hours(97)).timestamp();
    let far = (now + chrono::TimeDelta::days(9)).to_rfc3339();
    // JWT exp wins over a distant metadata expiry.
    let c = credential(json!({"type":"codex","refresh_token":"r","access_token":jwt_with_exp(soon),"expired":far}));
    assert!(refresh_due(&c, now));
    let c = credential(json!({"type":"codex","refresh_token":"r","access_token":jwt_with_exp(later)}));
    assert!(!refresh_due(&c, now));
    let c = credential(json!({"type":"codex","refresh_token":"r","access_token":"opaque","expired":far}));
    assert!(!refresh_due(&c, now));
    let c = credential(json!({"type":"codex","access_token":jwt_with_exp(soon)}));
    assert!(!refresh_due(&c, now), "no refresh token, nothing to do");
    let c = credential(
        json!({"type":"codex","refresh_token":"r","access_token":"opaque","last_refresh":(now - chrono::TimeDelta::hours(97)).to_rfc3339()}),
    );
    assert!(refresh_due(&c, now));
    let c =
        credential(json!({"type":"codex","refresh_token":"r","access_token":"opaque","last_refresh":now.to_rfc3339()}));
    assert!(!refresh_due(&c, now));
    let c = credential(json!({"type":"codex","refresh_token":"r","access_token":"opaque"}));
    assert!(refresh_due(&c, now), "no expiry and no last refresh");
    // Millisecond and numeric-string expiries normalise like Go's parseTimeValue.
    let c = credential(
        json!({"type":"codex","refresh_token":"r","access_token":"opaque","expired":(now.timestamp()+3600)*1000}),
    );
    assert!(refresh_due(&c, now));
    assert!(access_usable(&c, now));
    let c = credential(json!({"type":"codex","access_token":"opaque","expired":(now.timestamp()-5).to_string()}));
    assert!(!access_usable(&c, now));
}

#[test]
fn callback_routes_report_success_missing_state_denial_and_wrong_methods() {
    // Issued client identity is normalized separately from the OAuth code and state.
    let (response, result) =
        route_callback(b"GET /auth/callback?code=c1&state=s1&client_id=%20cid%20 HTTP/1.1\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 302 Found") && response.contains("Location: /success"));
    assert_eq!(
        result,
        Some(Callback {
            code: "c1".into(),
            state: "s1".into(),
            error: None,
            client_id: "cid".into(),
        })
    );
    let (response, result) = route_callback(b"GET /auth/callback?error=access_denied HTTP/1.1\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 400") && response.contains("OAuth error: access_denied"));
    assert_eq!(result.unwrap().error.as_deref(), Some("access_denied"));
    assert_eq!(
        route_callback(b"GET /auth/callback?code=c HTTP/1.1\r\n\r\n")
            .1
            .unwrap()
            .error
            .as_deref(),
        Some("no_state")
    );
    assert!(
        route_callback(b"POST /auth/callback?code=c&state=s HTTP/1.1\r\n\r\n")
            .0
            .starts_with("HTTP/1.1 405")
    );
    assert!(
        route_callback(b"GET /success HTTP/1.1\r\n\r\n")
            .0
            .contains("Authentication")
    );
    assert!(
        route_callback(b"GET /other HTTP/1.1\r\n\r\n")
            .0
            .starts_with("HTTP/1.1 404")
    );
}

#[test]
fn pasted_callbacks_decode_urls_queries_fragments_and_errors() {
    for (input, code, state, error) in [
        (
            "http://localhost:1455/auth/callback?code=one%2Btwo&state=nonce",
            "one+two",
            "nonce",
            None,
        ),
        ("?code=one%23nonce", "one", "nonce", None),
        (
            "localhost:1455/auth/callback#code=fragment&state=nonce",
            "fragment",
            "nonce",
            None,
        ),
        ("?error=access_denied&state=nonce", "", "nonce", Some("access_denied")),
        ("?error_description=refused", "", "", Some("refused")),
    ] {
        let parsed = parse_manual_callback(input).unwrap();
        assert_eq!(parsed.code, code);
        assert_eq!(parsed.state, state);
        assert_eq!(parsed.error.as_deref(), error);
    }
    for invalid in ["nonsense", "?state=only", "  ", "code=one#nonce"] {
        assert!(parse_manual_callback(invalid).is_err(), "{invalid}");
    }
}

#[tokio::test]
async fn relogin_keeps_an_explicit_disabled_boolean_and_normalizes_other_types() {
    let dir = std::env::temp_dir().join(format!("codex-login-{}", random_state().unwrap()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("codex-u@example.invalid-free.json");
    let tokens = Tokens {
        id_token: String::new(),
        access_token: "new-access".into(),
        refresh_token: "new-refresh".into(),
        account_id: String::new(),
        email: "u@example.invalid".into(),
        plan_type: String::new(),
        expired: "2026-10-12T12:00:00Z".into(),
    };
    // Saved accounts retain an explicit operator refusal. Other JSON types cannot
    // masquerade as that boolean; a new login writes the typed enabled default.
    for (disabled, expected) in [
        (json!(true), true),
        (json!(false), false),
        (json!("true"), false),
        (json!(7), false),
        (Value::Null, false),
    ] {
        std::fs::write(
            &path,
            json!({"disabled":disabled,"note":"retained","access_token":"old"}).to_string(),
        )
        .unwrap();
        write_login(&dir, &tokens).unwrap();
        let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["disabled"], expected);
        assert_eq!(saved["note"], "retained");
        assert_eq!(saved["access_token"], "new-access");
        assert_eq!(saved["refresh_token"], "new-refresh");
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn retained_results_never_block_a_new_refresh() {
    let mock = Mock::start().await;
    let oauth = oauth(&mock).await;
    mock.script(
        "/oauth/token",
        vec![json(200, &token_body()); MAX_RETAINED_REFRESHES + 1],
    );
    for i in 0..MAX_RETAINED_REFRESHES {
        oauth.refresh(&format!("rt-{i}")).await.unwrap();
    }
    oauth
        .refresh("rt-extra")
        .await
        .expect("a 65th distinct token still reaches OAuth");
    assert_eq!(mock.take().len(), MAX_RETAINED_REFRESHES + 1);
    assert!(oauth.refreshes.outer.lock().unwrap().len() <= MAX_RETAINED_REFRESHES);
    // The newest result is still deduplicated.
    oauth.refresh("rt-extra").await.unwrap();
    assert!(mock.take().is_empty());
}

#[test]
fn abandoned_prompt_does_not_block_runtime_shutdown() {
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            // A read that never returns, like stdin held open.
            let blocked = prompt_line("", || {
                std::thread::park();
                Ok(String::new())
            });
            tokio::select! {
                _ = blocked => unreachable!(),
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        });
        drop(runtime);
        done_tx.send(()).unwrap();
    });
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("runtime shut down without waiting for the prompt");
}
