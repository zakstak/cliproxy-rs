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

// ---- #92: the SIWC token answer as OpenAI's token reference documents it (fields,
// `expires_in: 3600` in its example, a replacement refresh token on every success),
// read on a fixed clock. Fetched 2026-11-12.

/// A fixed local clock: 2026-11-12T13:33:20Z.
fn clock() -> chrono::DateTime<Local> {
    use chrono::TimeZone as _;
    Local.timestamp_opt(1_794_490_400, 0).unwrap()
}

fn at(seconds: i64) -> chrono::DateTime<chrono::Utc> {
    clock().with_timezone(&chrono::Utc) + TimeDelta::seconds(seconds)
}

const DIRECT: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";

/// An answer with `expires_in` and `scope` spelled raw (`None` leaves the member out).
fn answer(expires_in: Option<&str>, scope: Option<&str>, extra: &str) -> String {
    let mut members = vec![
        r#""access_token":"siwc-at-2""#.to_owned(),
        r#""refresh_token":"siwc-rt-2""#.to_owned(),
        r#""id_token":"siwc-id-2""#.to_owned(),
        r#""token_type":"Bearer""#.to_owned(),
    ];
    if let Some(e) = expires_in {
        members.push(format!(r#""expires_in":{e}"#));
    }
    if let Some(s) = scope {
        members.push(format!(r#""scope":{s}"#));
    }
    if !extra.is_empty() {
        members.push(extra.to_owned());
    }
    format!("{{{}}}", members.join(","))
}

fn read(body: &str, exchange: bool) -> Result<Tokens, String> {
    token_response(body.as_bytes(), " oaiapp_fake ", " dev-fake ", exchange, clock())
        .map_err(|e| String::from_utf8_lossy(&e.body).into_owned())
}

/// A Codex credential whose SIWC partition is what a refresh with `tokens` writes.
fn siwc_credential(tokens: &Tokens) -> cpa_core::credential::Credential {
    cpa_core::credential::Credential::from_file(
        std::path::Path::new("/fake"),
        std::path::Path::new("/fake/c.json"),
        serde_json::json!({"type": "codex", "use_siwc": true, "siwc": tokens.to_map()})
            .as_object()
            .unwrap()
            .clone(),
    )
    .unwrap()
}

/// Seconds from the fixed clock to the last second whose year, in the local offset,
/// still has four digits.
fn last_storable() -> i64 {
    use chrono::TimeZone as _;
    Local
        .with_ymd_and_hms(9999, 12, 31, 23, 59, 59)
        .single()
        .unwrap()
        .timestamp()
        - clock().timestamp()
}

/// The documented answer reads into a token set at the fixed clock: the real expiry
/// `expires_in` after it, the scheduling expiry three minutes earlier (Go's margin), and
/// the replacement refresh token. `token_type` and `earliest_refresh_at` change nothing,
/// and the latter is never persisted, whatever its spelling.
#[test]
fn the_documented_siwc_token_answer_reads_at_a_fixed_clock() {
    let scope = format!(r#""{DIRECT}""#);
    let tokens = read(&answer(Some("3600"), Some(&scope), ""), false).unwrap();
    assert_eq!(
        (
            tokens.access_token.as_str(),
            tokens.refresh_token.as_str(),
            tokens.id_token.as_str(),
            tokens.client_id.as_str(),
            tokens.device_id.as_str(),
            tokens.scope.as_str(),
        ),
        ("siwc-at-2", "siwc-rt-2", "siwc-id-2", "oaiapp_fake", "dev-fake", DIRECT)
    );
    let instant = |s: &str| {
        chrono::DateTime::parse_from_rfc3339(s)
            .unwrap()
            .with_timezone(&chrono::Utc)
    };
    assert_eq!(instant(&tokens.last_refresh), at(0));
    assert_eq!(instant(&tokens.expires_at), at(3600));
    assert_eq!(instant(&tokens.expired), at(3600 - 180));
    for earliest in [
        "1794494000",
        r#""2026-11-12T14:33:20Z""#,
        r#""in an hour""#,
        "null",
        r#"{"seconds":3600}"#,
    ] {
        let extra = format!(r#""earliest_refresh_at":{earliest}"#);
        let with = read(&answer(Some("3600"), Some(&scope), &extra), false).unwrap();
        assert_eq!(with, tokens, "earliest_refresh_at {earliest}");
        assert!(!with.to_map().contains_key("earliest_refresh_at"));
    }
}

/// `expires_in` is read as a positive JSON integer, the parse inherited from the typed
/// reader; the provider's example is `3600`, and no source forbids another spelling of
/// the same number, so none is claimed and none is coerced. On a refresh any other value
/// leaves the expiry unknown and keeps the token set: the stored refresh token was
/// spent. The code exchange keeps refusing a value that is not a positive integer. An
/// expiry whose year needs five digits cannot be stored (RFC 3339) and is unknown on
/// both, never a refusal and never "expires now".
#[test]
fn siwc_expires_in_reads_the_documented_integer() {
    let scope = format!(r#""{DIRECT}""#);
    let longest = last_storable().to_string();
    let past = (last_storable() + 1).to_string();
    for known in ["3600", "1", "180", "181", longest.as_str()] {
        for exchange in [false, true] {
            let t = read(&answer(Some(known), Some(&scope), ""), exchange).unwrap();
            assert!(!t.expired.is_empty() && !t.expires_at.is_empty(), "{known}");
        }
    }
    let short = read(&answer(Some("1"), Some(&scope), ""), false).unwrap();
    assert_eq!(
        short.expired, short.expires_at,
        "a one-second grant keeps its whole lifetime"
    );
    let longest = read(&answer(Some(&longest), Some(&scope), ""), false).unwrap();
    assert!(
        longest.expires_at.starts_with("9999-12-31T23:59:59"),
        "{}",
        longest.expires_at
    );
    let c = siwc_credential(&longest);
    assert_eq!(refresh_due(&c, at(0)), Some(false));
    assert_eq!(refresh_due(&c, at(366 * 24 * 3600)), Some(false));
    let not_integers: [Option<&str>; 11] = [
        Some("0"),
        Some("-1"),
        Some("3600.0"),
        Some("3.6e3"),
        Some("3600.5"),
        Some(r#""3600""#),
        Some("null"),
        Some("true"),
        Some("1e20"),
        Some("18446744073709551616"),
        None,
    ];
    for expires_in in not_integers {
        let t = read(&answer(expires_in, Some(&scope), ""), false).unwrap();
        assert_eq!(
            (t.refresh_token.as_str(), t.expired.as_str(), t.expires_at.as_str()),
            ("siwc-rt-2", "", ""),
            "{expires_in:?}"
        );
        let map = t.to_map();
        assert_eq!(
            (&map["expired"], &map["expires_at"]),
            (&Value::Null, &Value::Null),
            "{expires_in:?}"
        );
        assert_eq!(
            read(&answer(expires_in, Some(&scope), ""), true).unwrap_err(),
            "OpenAI OAuth token response has invalid expires_in",
            "exchange {expires_in:?}"
        );
    }
    for unstorable in [past.as_str(), "9223372036854775807", "18446744073709551615"] {
        for exchange in [false, true] {
            let t = read(&answer(Some(unstorable), Some(&scope), ""), exchange).unwrap();
            assert_eq!(
                (t.expired.as_str(), t.expires_at.as_str()),
                ("", ""),
                "{unstorable} {exchange}"
            );
        }
    }
}

/// The token set is the access token and the replacement refresh token: without both,
/// nothing is used. A refresh never discards a usable token set over its other fields:
/// an `id_token` that is not a string is left out, an absent or unreadable `scope` keeps
/// the stored grant, and a reported scope is stored as reported. The code exchange keeps
/// its strict reading.
#[test]
fn siwc_token_answer_fields_are_read_at_the_boundary() {
    let direct = format!(r#""{DIRECT}""#);
    let ok = |s: &str| Ok(s.to_owned());
    for (case, text, exchange, want) in [
        (
            "no replacement refresh token",
            r#"{"access_token":"a","expires_in":3600}"#.to_owned(),
            false,
            Err("OpenAI OAuth token response has invalid refresh_token"),
        ),
        (
            "blank replacement",
            r#"{"access_token":"a","refresh_token":"  ","expires_in":3600}"#.to_owned(),
            false,
            Err("OpenAI OAuth token response has invalid refresh_token"),
        ),
        (
            "null replacement",
            r#"{"access_token":"a","refresh_token":null,"expires_in":3600}"#.to_owned(),
            false,
            Err("OpenAI OAuth token response has invalid refresh_token"),
        ),
        (
            "no access token",
            r#"{"refresh_token":"r","expires_in":3600}"#.to_owned(),
            false,
            Err("OpenAI OAuth token response has invalid access_token"),
        ),
        (
            "numeric access token",
            r#"{"access_token":5,"refresh_token":"r","expires_in":3600}"#.to_owned(),
            false,
            Err("OpenAI OAuth token response has invalid access_token"),
        ),
        (
            "refresh: null ID token",
            r#"{"access_token":"a","refresh_token":"r","id_token":null,"expires_in":3600}"#.to_owned(),
            false,
            ok(""),
        ),
        (
            "refresh: no ID token or scope",
            r#"{"access_token":"a","refresh_token":"r","expires_in":3600}"#.to_owned(),
            false,
            ok(""),
        ),
        (
            "refresh: blank scope",
            r#"{"access_token":"a","refresh_token":"r","expires_in":3600,"scope":" "}"#.to_owned(),
            false,
            ok(""),
        ),
        (
            "refresh: null scope",
            r#"{"access_token":"a","refresh_token":"r","expires_in":3600,"scope":null}"#.to_owned(),
            false,
            ok(""),
        ),
        (
            "refresh: scope without direct use",
            r#"{"access_token":"a","refresh_token":"r","expires_in":3600,"scope":"openid"}"#.to_owned(),
            false,
            ok("openid"),
        ),
        (
            "exchange: null ID token",
            format!(r#"{{"access_token":"a","refresh_token":"r","id_token":null,"expires_in":3600,"scope":{direct}}}"#),
            true,
            Err("OpenAI OAuth token response has invalid id_token"),
        ),
        (
            "exchange: no scope",
            r#"{"access_token":"a","refresh_token":"r","id_token":"i","expires_in":3600}"#.to_owned(),
            true,
            Err("OpenAI OAuth token response has invalid scope"),
        ),
        (
            "exchange: no ID token",
            format!(r#"{{"access_token":"a","refresh_token":"r","expires_in":3600,"scope":{direct}}}"#),
            true,
            Err("OpenAI OAuth token response did not contain an ID token"),
        ),
        (
            "exchange: scope without direct use",
            r#"{"access_token":"a","refresh_token":"r","id_token":"i","expires_in":3600,"scope":"openid"}"#.to_owned(),
            true,
            Err("OpenAI OAuth grant did not include chatgpt.tokens.use.direct"),
        ),
        (
            "an array",
            "[]".to_owned(),
            false,
            Err("failed to parse SIWC token response"),
        ),
        (
            "null",
            "null".to_owned(),
            false,
            Err("failed to parse SIWC token response"),
        ),
        (
            "not JSON",
            "not json".to_owned(),
            false,
            Err("failed to parse SIWC token response"),
        ),
    ] {
        let got = read(&text, exchange).map(|t| t.scope);
        assert_eq!(got, want.map_err(str::to_owned), "{case}");
    }
    // An absent scope keeps the partition's stored grant when the refresh is merged.
    let tokens = read(r#"{"access_token":"a2","refresh_token":"r2","expires_in":3600}"#, false).unwrap();
    let mut stored = siwc_credential(&read(&answer(Some("3600"), Some(&direct), ""), false).unwrap());
    stored.metadata["siwc"]["scope"] = DIRECT.into();
    let patch = refresh_patch(&stored, &tokens);
    let siwc = &patch.set["siwc"];
    assert_eq!(
        (&siwc["access_token"], &siwc["refresh_token"]),
        (&"a2".into(), &"r2".into())
    );
    assert_eq!(
        (&siwc["scope"], &siwc["scopes"]),
        (&stored.metadata["siwc"]["scope"], &stored.metadata["siwc"]["scopes"])
    );
}

/// The renewal schedule of a refreshed SIWC partition on a controlled clock: due five
/// minutes before the margin-deducted expiry (57 minutes after the grant, so at 52
/// minutes), usable until that expiry, and reported `valid` until the real one. An
/// `earliest_refresh_at` in the answer moves none of it. With an unknown expiry the
/// new access token's JWT `exp` schedules it, else `last_refresh` five minutes on.
/// Without a refresh token or an issued client the partition is never due.
#[test]
fn a_refreshed_siwc_partition_follows_a_controlled_clock() {
    let extra = r#""earliest_refresh_at":1794497600"#;
    let tokens = read(&answer(Some("3600"), Some(&format!(r#""{DIRECT}""#)), extra), false).unwrap();
    let c = siwc_credential(&tokens);
    let due = |t| refresh_due(&c, at(t));
    assert_eq!(
        (due(0), due(52 * 60 - 1), due(52 * 60)),
        (Some(false), Some(false), Some(true))
    );
    assert!(access_usable(&c, at(57 * 60 - 1)));
    assert!(!access_usable(&c, at(57 * 60)));
    assert_eq!(status(&c, false, at(3599)), "valid");
    assert_eq!(status(&c, false, at(3600)), "invalid");
    assert_eq!(status(&c, true, at(0)), "invalid", "rejected");
    // An unknown expiry: `last_refresh` and the five-minute lead.
    let unknown = siwc_credential(&read(&answer(Some(r#""3600""#), None, ""), false).unwrap());
    let due = |t| refresh_due(&unknown, at(t));
    assert_eq!((due(0), due(299), due(300)), (Some(false), Some(false), Some(true)));
    assert!(
        access_usable(&unknown, at(299)),
        "an unknown expiry does not block requests"
    );
    // The JWT `exp` claim (integer Unix seconds, token reference) schedules it instead.
    use base64::Engine as _;
    let claims =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{}}}"#, at(3600).timestamp()));
    let jwt_answer =
        format!(r#"{{"access_token":"e30.{claims}.c2ln","refresh_token":"siwc-rt-jwt","expires_in":"3600"}}"#);
    let jwt = siwc_credential(&read(&jwt_answer, false).unwrap());
    let due = |t| refresh_due(&jwt, at(t));
    assert_eq!((due(55 * 60 - 1), due(55 * 60)), (Some(false), Some(true)));
    // Missing renewal credentials: never due, at any time.
    for (case, remove) in [("refresh token", "refresh_token"), ("issued client", "client_id")] {
        let mut missing = c.clone();
        missing.metadata["siwc"].as_object_mut().unwrap().remove(remove);
        for t in [0, 3600, 30 * 24 * 3600, 365 * 24 * 3600] {
            assert_eq!(refresh_due(&missing, at(t)), None, "{case} at {t}");
        }
    }
}

/// A refreshed grant that reports scopes without `chatgpt.tokens.use.direct` keeps the
/// sign-in (OpenAI "errors and recovery": retain it, mark ChatGPT plan usage disabled):
/// the replacement tokens stay and renew on their usual schedule, the status reads
/// `invalid`, and requests are refused before they are built with a 403, never a 401,
/// so nothing starts a refresh or a sign-in loop. An unknown grant (no stored scope, or
/// a refresh that reported none) is the bound one and stays usable. The outer tokens of
/// a credential with `use_siwc` off are untouched.
#[test]
fn a_grant_without_direct_use_disables_plan_usage() {
    let denied = siwc_credential(&read(&answer(Some("3600"), Some(r#""openid offline_access""#), ""), false).unwrap());
    assert!(plan_use_denied(&denied));
    assert_eq!(
        denied.metadata["siwc"]["refresh_token"], "siwc-rt-2",
        "the replacement stays"
    );
    let error = require_token(&denied).unwrap_err();
    assert_eq!((error.status, error.scope), (403, FailureScope::Credential));
    assert!(!String::from_utf8_lossy(&error.body).contains("401"));
    assert_eq!(status(&denied, false, at(0)), "invalid");
    assert!(
        access_usable(&denied, at(0)),
        "the token itself is valid; only its plan use is refused"
    );
    assert_eq!(
        (refresh_due(&denied, at(0)), refresh_due(&denied, at(52 * 60))),
        (Some(false), Some(true)),
        "it renews on its usual schedule, not at once"
    );
    // The scopes list alone decides when no scope string is stored.
    let mut listed = denied.clone();
    listed.metadata["siwc"].as_object_mut().unwrap().remove("scope");
    assert!(plan_use_denied(&listed));
    // Unknown grants stay usable.
    for (case, siwc) in [
        (
            "no stored scope",
            serde_json::json!({"access_token": "a", "refresh_token": "r", "client_id": "c"}),
        ),
        ("blank scope", serde_json::json!({"access_token": "a", "scope": " "})),
        ("empty scopes", serde_json::json!({"access_token": "a", "scopes": []})),
        (
            "scopes not strings",
            serde_json::json!({"access_token": "a", "scopes": [1, 2]}),
        ),
        (
            "direct use granted",
            serde_json::json!({"access_token": "a", "scope": DIRECT}),
        ),
    ] {
        let mut c = denied.clone();
        c.metadata["siwc"] = siwc;
        assert!(!plan_use_denied(&c), "{case}");
        assert!(require_token(&c).is_ok(), "{case}");
    }
    // A refresh that reports no readable scope keeps a stored grant with direct use.
    let granted = siwc_credential(&read(&answer(Some("3600"), Some(&format!(r#""{DIRECT}""#)), ""), false).unwrap());
    let unreported = read(r#"{"access_token":"a2","refresh_token":"r2","expires_in":3600}"#, false).unwrap();
    let mut merged = granted.clone();
    merged.metadata["siwc"] = refresh_patch(&granted, &unreported).set["siwc"].clone();
    assert!(!plan_use_denied(&merged) && require_token(&merged).is_ok());
    // With `use_siwc` off the requests use the outer tokens: nothing is refused.
    let mut native = denied.clone();
    native.metadata["use_siwc"] = false.into();
    assert!(require_token(&native).is_ok());
    assert_eq!(status(&native, false, at(0)), "off");
}
