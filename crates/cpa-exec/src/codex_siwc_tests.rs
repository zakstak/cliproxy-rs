//! Expected strings were printed by Go 1.26 running the v8.0.35 encoding steps
//! (`url.Values.Encode` for `GenerateSIWCAuthURL`; `json.Unmarshal` into
//! `map[string]json.RawMessage` and `json.Marshal` for `BindCodexSIWC`).

use super::*;
use crate::codex_testkit::{Mock, json};

#[test]
fn authorize_url_matches_go() {
    let url = authorize_url(
        " st-1 ",
        "no-1",
        " E61BBE28-07EF-466D-8E5D-A344F94AB305 ",
        "",
        "ch_-1",
        "My Agent/1.0 (x)",
    )
    .unwrap();
    assert_eq!(
        url,
        "https://auth.openai.com/api/accounts/authorize?agent_name_hint=My+Agent%2F1.0+%28x%29&client_id=dynamic_agent_client&code_challenge=ch_-1&code_challenge_method=S256&ext_agent_host_id=urn%3Auuid%3Ae61bbe28-07ef-466d-8e5d-a344f94ab305&nonce=no-1&redirect_uri=http%3A%2F%2F127.0.0.1%3A1455%2Fauth%2Fcallback&resource=https%3A%2F%2Fapi.openai.com%2Fv1&response_type=code&scope=openid+profile+email+offline_access+resource.invoke+chatgpt.tokens.use.direct&state=st-1"
    );
    for (state, nonce, device) in [
        ("", "n", "e61bbe28-07ef-466d-8e5d-a344f94ab305"),
        ("s", " ", "e61bbe28-07ef-466d-8e5d-a344f94ab305"),
        ("s", "n", "e61bbe28-07ef-466d-8e5d-a344f94ab30"),
        ("s", "n", "e61bbe28x07ef-466d-8e5d-a344f94ab305"),
    ] {
        assert!(
            authorize_url(state, nonce, device, "", "c", "a").is_err(),
            "{state} {nonce} {device}"
        );
    }
}

#[test]
fn identifier_reads_scalars_as_go_strings() {
    let id = |yaml: &str| identifier(&Config::parse(yaml).unwrap());
    let v8 = |v: &str| format!("oauth:\n  providers:\n    codex:\n      signin-with-chatgpt-identifier: {v}\n");
    assert_eq!(id(&v8("'  Agent X  '")), "Agent X");
    assert_eq!(id(&v8("123")), "123");
    assert_eq!(id(&v8("true")), "true");
    assert_eq!(id(&v8("'  '")), "CLIProxyAPI");
    assert_eq!(id(&v8("null")), "CLIProxyAPI");
    assert_eq!(id("port: 1\n"), "CLIProxyAPI");
    assert_eq!(id("codex:\n  signin-with-chatgpt-identifier: Legacy\n"), "Legacy");
}

#[test]
fn bind_file_keeps_disk_values_and_encodes_like_go() {
    let disk = "{\n  \"type\": \"codex\",\n  \"access_token\": \"outer\", \"refresh_token\": \"r\",\n  \"big\": 123456789012345678901234567890,\n  \"html\": \"<a&b>\u{2028}\",\n  \"nested\": { \"k\" : [1, 2.50, \"x \\\" y\"] },\n  \"siwc\": {\"device_id\": \"old\"},\n  \"dup\": 1, \"dup\": 2,\n  \"use_siwc\": false\n}";
    let tokens = Tokens {
        access_token: "a<b".into(),
        refresh_token: "r2".into(),
        id_token: "id".into(),
        client_id: "oaiapp_x".into(),
        scope: "s chatgpt.tokens.use.direct".into(),
        scopes: vec!["s".into(), "chatgpt.tokens.use.direct".into()],
        expired: "2035-01-01T00:00:00Z".into(),
        expires_at: String::new(),
        last_refresh: "2026-01-01T00:00:00Z".into(),
        device_id: "e61bbe28-07ef-466d-8e5d-a344f94ab305".into(),
    };
    let out = bind_file(disk.as_bytes(), &tokens).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        r#"{"access_token":"outer","big":123456789012345678901234567890,"dup":2,"html":"\u003ca\u0026b\u003e\u2028","nested":{"k":[1,2.50,"x \" y"]},"refresh_token":"r","siwc":{"access_token":"a\u003cb","client_id":"oaiapp_x","device_id":"e61bbe28-07ef-466d-8e5d-a344f94ab305","expired":"2035-01-01T00:00:00Z","id_token":"id","last_refresh":"2026-01-01T00:00:00Z","refresh_token":"r2","scope":"s chatgpt.tokens.use.direct","scopes":["s","chatgpt.tokens.use.direct"]},"type":"codex","use_siwc":true}"#
    );
    assert!(bind_file(b"[1]", &tokens).is_none());
}

async fn oauth(mock: &Mock) -> CodexOAuth {
    CodexOAuth::new(wreq::Client::new()).with_siwc_token_url(&format!("{}/api/accounts/oauth/token", mock.url))
}

#[tokio::test]
async fn exchange_sends_the_issued_client_and_validates_the_grant() {
    let mock = Mock::start().await;
    let path = "/api/accounts/oauth/token";
    let ok = r#"{"access_token":"at","refresh_token":"rt","id_token":"it","scope":" openid chatgpt.tokens.use.direct ","expires_in":3600}"#;
    mock.script(path, vec![json(200, ok)]);
    let tokens = oauth(&mock)
        .await
        .exchange_siwc(" issued-code ", " oaiapp_issued ", "", " dev ", "verifier")
        .await
        .unwrap();
    let sent = mock.take();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].header("content-type"), "application/x-www-form-urlencoded");
    assert_eq!(sent[0].header("accept"), "application/json");
    assert_eq!(
        String::from_utf8_lossy(&sent[0].body),
        "client_id=oaiapp_issued&code=issued-code&code_verifier=verifier&grant_type=authorization_code&redirect_uri=http%3A%2F%2F127.0.0.1%3A1455%2Fauth%2Fcallback&resource=https%3A%2F%2Fapi.openai.com%2Fv1"
    );
    assert_eq!(
        (
            tokens.client_id.as_str(),
            tokens.device_id.as_str(),
            tokens.scope.as_str()
        ),
        ("oaiapp_issued", "dev", "openid chatgpt.tokens.use.direct")
    );
    assert_eq!(tokens.scopes, ["openid", "chatgpt.tokens.use.direct"]);
    // expires_in less Go's three-minute margin.
    let expired = chrono::DateTime::parse_from_rfc3339(&tokens.expired).unwrap();
    let last = chrono::DateTime::parse_from_rfc3339(&tokens.last_refresh).unwrap();
    assert_eq!((expired - last).num_seconds(), 3600 - 180);

    let rejected = [
        (500, r#"{"error":"secret-body"}"#),
        (
            200,
            r#"{"refresh_token":"rt","id_token":"it","scope":"chatgpt.tokens.use.direct","expires_in":60}"#,
        ),
        (
            200,
            r#"{"access_token":"at","id_token":"it","scope":"chatgpt.tokens.use.direct","expires_in":60}"#,
        ),
        (
            200,
            r#"{"access_token":"at","refresh_token":"rt","scope":"chatgpt.tokens.use.direct","expires_in":60}"#,
        ),
        (
            200,
            r#"{"access_token":"at","refresh_token":"rt","id_token":"it","scope":"openid","expires_in":60}"#,
        ),
        (
            200,
            r#"{"access_token":"at","refresh_token":"rt","id_token":"it","scope":"chatgpt.tokens.use.direct","expires_in":0}"#,
        ),
        (200, "not json"),
    ];
    mock.script(path, rejected.iter().map(|(s, b)| json(*s, b)).collect());
    for (status, body) in rejected {
        let e = oauth(&mock)
            .await
            .exchange_siwc("c", "client", "", "d", "v")
            .await
            .unwrap_err();
        assert!(!String::from_utf8_lossy(&e.body).contains("secret"), "{status} {body}");
    }
    // A short grant keeps its whole lifetime rather than going negative.
    mock.script(
        path,
        vec![json(
            200,
            r#"{"access_token":"at","refresh_token":"rt","id_token":"it","scope":"chatgpt.tokens.use.direct","expires_in":60}"#,
        )],
    );
    let short = oauth(&mock)
        .await
        .exchange_siwc("c", "client", "", "", "v")
        .await
        .unwrap();
    let expired = chrono::DateTime::parse_from_rfc3339(&short.expired).unwrap();
    let last = chrono::DateTime::parse_from_rfc3339(&short.last_refresh).unwrap();
    assert_eq!((expired - last).num_seconds(), 60);
    // Missing callback values never reach the endpoint.
    mock.take();
    assert!(oauth(&mock).await.exchange_siwc("c", " ", "", "", "v").await.is_err());
    assert!(
        oauth(&mock)
            .await
            .exchange_siwc(" ", "client", "", "", "v")
            .await
            .is_err()
    );
    assert!(mock.take().is_empty());
}

/// Go `CodexSIWCNeedsRefresh` at two fixed clocks for every expiry shape Go reads:
/// `time.Parse`'s grammar, `normaliseUnix`, relative expiries with Go's wrapping
/// `Duration` product, nested token maps, the JWT `exp` and Go's zero time; see
/// tests/reference/codex_siwc_expiry.
#[test]
fn refresh_due_matches_go_expiry_parsing() {
    let go: Value = serde_json::from_str(include_str!("../tests/fixtures/codex_siwc_expiry_go.json")).unwrap();
    let clocks: Vec<chrono::DateTime<chrono::Utc>> = go["clocks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().parse().unwrap())
        .collect();
    let mut wrong = vec![];
    for case in go["cases"].as_array().unwrap() {
        let siwc: Value = serde_json::from_str(case["siwc"].as_str().unwrap()).unwrap();
        let credential = cpa_core::credential::Credential::from_file(
            std::path::Path::new("/fake"),
            std::path::Path::new("/fake/c.json"),
            serde_json::json!({"type": "codex", "siwc": siwc})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap();
        let ours: Vec<Value> = clocks
            .iter()
            .map(|at| refresh_due(&credential, *at).unwrap().into())
            .collect();
        let mut want = case["due"].clone();
        if case["name"] == "out-of-range float" && !cfg!(target_arch = "x86_64") {
            // Recorded on amd64 (`math.MinInt64`, the zero time); Go on arm64 saturates to
            // a far-future time.
            want = serde_json::json!([false, false]);
        }
        if case["name"] == "float at the unit threshold" {
            // DIFFERENCES-FROM-GO.md: serde_json rounds 1000000000000.9999 up to 1e12 + 1,
            // read as milliseconds (2001); Go's correctly rounded float64 truncates to 1e12
            // seconds.
            want = serde_json::json!([true, true]);
        }
        if ours != want.as_array().unwrap()[..] {
            wrong.push(format!("{}: {ours:?}, Go {want}", case["name"]));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}
