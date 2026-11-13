//! #92: Codex renewal correctness. Token endpoints are loopback mocks; every token is
//! synthetic.

use super::*;
use crate::codex_testkit::{Mock, json};
use std::path::Path;
use std::sync::Arc;

const OUTER: &str = "/oauth/token";
const SIWC: &str = "/api/accounts/oauth/token";

fn refresher(mock: &Mock) -> Refresher {
    let oauth = CodexOAuth::with_endpoints(wreq::Client::new(), &format!("{}{OUTER}", mock.url), "", "")
        .with_siwc_token_url(&format!("{}{SIWC}", mock.url))
        .with_retry_delay(std::time::Duration::from_millis(1));
    Refresher::new(oauth)
}

fn minutes(m: i64) -> String {
    (Utc::now() + chrono::TimeDelta::minutes(m)).to_rfc3339()
}

/// A Codex file whose tokens are unique to `id` (refresh results are single-flighted and
/// retained per token): outer tokens valid for `outer` minutes, SIWC ones for `siwc`.
fn doc(id: &str, use_siwc: bool, outer: i64, siwc: i64) -> Map<String, Value> {
    serde_json::json!({
        "type": "codex", "use_siwc": use_siwc,
        "access_token": format!("outer-at-{id}"), "refresh_token": format!("outer-rt-{id}"),
        "expired": minutes(outer),
        "siwc": {"access_token": format!("siwc-at-{id}"), "refresh_token": format!("siwc-rt-{id}"),
                 "client_id": "oaiapp_fake", "device_id": "dev-fake", "expired": minutes(siwc)},
    })
    .as_object()
    .unwrap()
    .clone()
}

fn credential(metadata: &Map<String, Value>) -> Credential {
    Credential::from_file(Path::new("/fake"), Path::new("/fake/c.json"), metadata.clone()).unwrap()
}

fn outer_ok() -> crate::codex_testkit::Reply {
    json(
        200,
        r#"{"access_token":"outer-at-new","refresh_token":"outer-rt-new","id_token":"","expires_in":864000}"#,
    )
}

fn sent_to(mock: &Mock) -> Vec<String> {
    mock.take().into_iter().map(|c| c.path).collect()
}

/// The classifications follow their sources exactly: first-party codex-rs for the outer
/// ChatGPT OAuth refresh, OpenAI's SIWC error table for the SIWC one.
#[test]
fn refusals_follow_first_party_and_official_classification() {
    for (status, code, terminal) in [
        (401, "", true),
        (401, "server_error", true),
        (400, "invalid_grant", true),
        (400, "INVALID_GRANT", true),
        (403, "invalid_grant", false),
        (500, "refresh_token_reused", true),
        (400, "Refresh_Token_Expired", true),
        (400, "refresh_token_invalidated", true),
        (400, "invalid_request", false),
        (500, "", false),
        (429, "", false),
    ] {
        assert_eq!(
            codex_oauth::outer_terminal(status, code),
            terminal,
            "outer {status} {code}"
        );
    }
    for (code, terminal) in [
        ("invalid_grant", true),
        ("invalid_refresh_token", true),
        ("token_expired", true),
        ("refresh_token_expired", true),
        ("refresh_token_invalidated", true),
        ("refresh_token_reused", true),
        ("invalid_client", false),
        ("server_error", false),
        ("Invalid_Grant", false),
        ("", false),
    ] {
        assert_eq!(codex_oauth::siwc_terminal(code), terminal, "siwc {code}");
    }
}

/// A SIWC refresh token refused with each documented code is sent once, not three
/// times, and the refusal is cached: the same token fails at once, with no request.
/// A transient failure is retried three times and not cached as a refusal.
#[tokio::test]
async fn a_refused_siwc_refresh_token_is_sent_once() {
    let mock = Mock::start().await;
    let oauth = CodexOAuth::with_endpoints(wreq::Client::new(), "", "", "")
        .with_siwc_token_url(&format!("{}{SIWC}", mock.url))
        .with_retry_delay(std::time::Duration::from_millis(1));
    for code in [
        "invalid_grant",
        "invalid_refresh_token",
        "token_expired",
        "refresh_token_expired",
        "refresh_token_invalidated",
        "refresh_token_reused",
    ] {
        let token = format!("siwc-rt-{code}");
        mock.script(SIWC, vec![json(400, &format!(r#"{{"error":"{code}"}}"#))]);
        let refused = oauth.refresh_siwc(&token, "oaiapp_fake", "dev").await.unwrap_err();
        assert!(refused.terminal, "{code}");
        assert_eq!(sent_to(&mock).len(), 1, "{code}: sent once");
        let again = oauth.refresh_siwc(&token, "oaiapp_fake", "dev").await.unwrap_err();
        assert!(again.terminal, "{code}");
        assert!(mock.take().is_empty(), "{code}: a refused token fails at once");
    }
    mock.script(SIWC, vec![json(500, "{}"); 3]);
    let transient = oauth
        .refresh_siwc("siwc-rt-busy", "oaiapp_fake", "dev")
        .await
        .unwrap_err();
    assert!(!transient.terminal);
    assert_eq!(sent_to(&mock).len(), 3, "a transient failure is retried");
}

/// OpenAI's token reference: each successful SIWC refresh returns a replacement refresh
/// token. A 200 without one is refused (and not mistaken for a dead token): nothing of
/// the answer is applied, and the stored refresh token stays for a later attempt. The
/// endpoint answered 200, so it may have spent that token: the same refresh does not
/// send it again (it used to, three times), and the next one waits for its backoff.
#[tokio::test]
async fn a_siwc_refresh_without_a_replacement_token_is_refused() {
    let mock = Mock::start().await;
    let r = refresher(&mock);
    let metadata = doc("norotation", true, 60 * 24 * 10, 2);
    mock.script(
        SIWC,
        vec![
            json(
                200,
                r#"{"access_token":"siwc-at-unrotated","scope":"openid chatgpt.tokens.use.direct","expires_in":3600}"#
            );
            1
        ],
    );
    let (result, refused) = r
        .refresh_locked(&credential(&metadata), &metadata, RefreshMode::Due, &Refused::default())
        .await;
    assert_eq!(sent_to(&mock), [SIWC], "sent once, not again in the same refresh");
    assert!(refused.is_empty(), "not a refused refresh token");
    // The SIWC token is still valid for two minutes, so the due refresh tolerates it.
    let patch = result.unwrap();
    assert!(patch.is_empty(), "nothing of the unrotated answer is applied");
    // Once the access token has expired, the failure is reported.
    let expired = doc("norotation2", true, 60 * 24 * 10, -1);
    mock.script(
        SIWC,
        vec![
            json(
                200,
                r#"{"access_token":"siwc-at-unrotated","scope":"openid chatgpt.tokens.use.direct","expires_in":3600}"#
            );
            1
        ],
    );
    let (result, _) = r
        .refresh_locked(&credential(&expired), &expired, RefreshMode::Due, &Refused::default())
        .await;
    let error = result.unwrap_err();
    assert_eq!(
        String::from_utf8_lossy(&error.body),
        "SIWC token refresh answered without a usable token set; the refresh token was not sent again: OpenAI OAuth token response has invalid refresh_token"
    );
    assert_eq!(sent_to(&mock), [SIWC], "sent once");
}

/// A management refresh of a credential whose SIWC refresh token is refused for good:
/// the outer partition still refreshes, the SIWC one is reported refused for that token
/// alone. Refreshing again with that record sends nothing for SIWC, whatever the mode,
/// while the outer token keeps refreshing.
#[tokio::test]
async fn partitions_are_refused_independently() {
    let mock = Mock::start().await;
    let r = refresher(&mock);
    let metadata = doc("split", true, 60, 60);
    let c = credential(&metadata);
    mock.script(OUTER, vec![outer_ok()]);
    mock.script(SIWC, vec![json(400, r#"{"error":"invalid_grant"}"#)]);
    let (result, refused) = r
        .refresh_locked(&c, &metadata, RefreshMode::Management, &Refused::default())
        .await;
    assert_eq!(sent_to(&mock), [OUTER, SIWC]);
    assert!(!result.unwrap().is_empty(), "the outer refresh is kept");
    assert_eq!(refused.outer, None);
    assert!(refused.siwc_refused(&c) && refused.active_refused(&c));
    for mode in [RefreshMode::Due, RefreshMode::Rejected, RefreshMode::Management] {
        mock.script(OUTER, vec![outer_ok()]);
        let fresh = doc(&format!("split-{mode:?}"), true, 60, -1);
        let mut fresh_c = credential(&fresh);
        // The same refused SIWC refresh token, another (unrefused) outer one.
        fresh_c.metadata["siwc"] = c.metadata["siwc"].clone();
        let mut disk = fresh.clone();
        disk["siwc"] = metadata["siwc"].clone();
        let (_, still) = r.refresh_locked(&fresh_c, &disk, mode, &refused).await;
        let sent = sent_to(&mock);
        assert!(
            !sent.contains(&SIWC.to_owned()),
            "{mode:?}: the refused token was sent: {sent:?}"
        );
        assert!(still.siwc_refused(&fresh_c), "{mode:?}: the refusal is kept");
    }
}

/// Another process signed in again: the file holds a new SIWC refresh token. The refresh
/// adopts it from disk, and the refusal of the old token no longer applies to it.
#[tokio::test]
async fn a_new_token_on_disk_is_not_refused() {
    let mock = Mock::start().await;
    let r = refresher(&mock);
    let old = doc("relogin-old", true, 60 * 24 * 10, -1);
    let c = credential(&old);
    let refused = Refused {
        outer: None,
        siwc: token_hash("siwc-rt-relogin-old"),
    };
    assert!(refused.siwc_refused(&c));
    let disk = doc("relogin-new", true, 60 * 24 * 10, -1);
    mock.script(
        SIWC,
        vec![json(
            200,
            r#"{"access_token":"siwc-at-renewed","refresh_token":"siwc-rt-renewed","scope":"openid chatgpt.tokens.use.direct","expires_in":3600}"#,
        )],
    );
    let (result, still) = r.refresh_locked(&c, &disk, RefreshMode::Due, &refused).await;
    let sent = mock.take();
    assert_eq!(sent.len(), 1);
    assert!(
        String::from_utf8_lossy(&sent[0].body).contains("refresh_token=siwc-rt-relogin-new"),
        "the new token on disk is refreshed"
    );
    assert!(still.is_empty(), "the old refusal does not apply to the new token");
    assert!(!result.unwrap().is_empty());
}

/// The record keys on the refresh token itself: a changed token lifts it for that
/// partition only, and the mask removes only refused refresh tokens.
#[test]
fn a_refusal_follows_its_partition_token() {
    let metadata = doc("keyed", true, 60, 60);
    let c = credential(&metadata);
    let refused = Refused {
        outer: token_hash("outer-rt-keyed"),
        siwc: token_hash("siwc-rt-keyed"),
    };
    assert_eq!(refused.current(&c), refused);
    let masked = refused.mask(&c);
    assert!(codex_oauth::refresh_token(&masked).is_empty());
    assert!(crate::codex_siwc::refresh_inputs(&masked).0.is_empty());
    assert_eq!(masked.str("access_token"), Some("outer-at-keyed"), "access tokens stay");
    assert_eq!(crate::codex_siwc::access_token(&masked), "siwc-at-keyed");
    let mut relogged = c.clone();
    relogged.metadata["siwc"]["refresh_token"] = "siwc-rt-new".into();
    let left = refused.current(&relogged);
    assert_eq!((left.outer.is_some(), left.siwc), (true, None));
    assert!(!left.active_refused(&relogged), "SIWC is in use and renewable again");
    let mut native = relogged.clone();
    native.metadata["use_siwc"] = false.into();
    assert!(
        left.active_refused(&native),
        "the outer partition is in use and refused"
    );
}

/// Sol review of 8596a77d: the classification reads the error code as first-party
/// codex-rs does (`TokenErrorDetail::parse`): a string `error`, else `error.code`, else a
/// top-level `code`. A top-level refusal code is sent once for both token sets; an
/// `error` string wins over a top-level code, and a code that is terminal only with HTTP
/// 400 stays transient with another status.
#[tokio::test]
async fn top_level_refusal_codes_classify_like_first_party() {
    assert_eq!(codex_oauth::grant_code(br#"{"code":"invalid_grant"}"#), "invalid_grant");
    assert_eq!(
        codex_oauth::grant_code(br#"{"error":"server_error","code":"invalid_grant"}"#),
        "server_error"
    );
    assert_eq!(
        codex_oauth::grant_code(br#"{"error":{"code":"refresh_token_reused"},"code":"x"}"#),
        "refresh_token_reused"
    );
    assert_eq!(
        codex_oauth::grant_code(br#"{"error":" ","code":"token_expired"}"#),
        "token_expired"
    );
    assert_eq!(codex_oauth::grant_code(b"not json"), "");
    let mock = Mock::start().await;
    let oauth = CodexOAuth::with_endpoints(wreq::Client::new(), &format!("{}{OUTER}", mock.url), "", "")
        .with_siwc_token_url(&format!("{}{SIWC}", mock.url))
        .with_retry_delay(std::time::Duration::from_millis(1));
    for (i, (status, body, terminal)) in [
        (400, r#"{"code":"invalid_grant"}"#, true),
        (403, r#"{"code":"refresh_token_expired"}"#, true),
        (400, r#"{"error":"server_error","code":"invalid_grant"}"#, false),
        (500, r#"{"code":"invalid_grant"}"#, false),
        (400, r#"{"code":"server_error"}"#, false),
    ]
    .into_iter()
    .enumerate()
    {
        mock.script(OUTER, vec![json(status, body); 3]);
        let error = oauth.refresh_grant(&format!("outer-rt-top-{i}")).await.unwrap_err();
        let sends = mock.take().len();
        assert_eq!(
            (error.terminal, sends),
            (terminal, if terminal { 1 } else { 3 }),
            "outer {status} {body}"
        );
    }
    for (i, (status, body, terminal)) in [
        (400, r#"{"code":"invalid_grant"}"#, true),
        (401, r#"{"code":"refresh_token_reused"}"#, true),
        (400, r#"{"error":"server_error","code":"invalid_grant"}"#, false),
        (400, r#"{"code":"invalid_client"}"#, false),
    ]
    .into_iter()
    .enumerate()
    {
        mock.script(SIWC, vec![json(status, body); 3]);
        let error = oauth
            .refresh_siwc(&format!("siwc-rt-top-{i}"), "oaiapp_fake", "dev")
            .await
            .unwrap_err();
        let sends = mock.take().len();
        assert_eq!(
            (error.terminal, sends),
            (terminal, if terminal { 1 } else { 3 }),
            "siwc {status} {body}"
        );
    }
}

/// A token endpoint that reads each request whole, answers it with `head` and `body`
/// and then closes, and counts the connections.
async fn raw_endpoint(head: &'static str, body: &'static str) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/oauth/token", listener.local_addr().unwrap());
    let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = connections.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                // The headers, then as many body bytes as they announce.
                let complete = |r: &[u8]| {
                    let Some(end) = r.windows(4).position(|w| w == b"\r\n\r\n") else {
                        return false;
                    };
                    let head = String::from_utf8_lossy(&r[..end]).to_ascii_lowercase();
                    let length = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    r.len() >= end + 4 + length
                };
                while !complete(&request) {
                    let read = stream.read(&mut buffer).await.unwrap_or(0);
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..read]);
                }
                let _ = stream.write_all(format!("{head}\r\n\r\n{body}").as_bytes()).await;
            });
        }
    });
    (url, connections)
}

/// Sol review of 8596a77d (first-party `TokenRejection`: "HTTP status survives even when
/// its body cannot be read"): an outer refresh answered 401 with a body cut short is
/// refused for good, sent once, with its status. Controls: the same cut with 500 or 200
/// stays a transient transport failure, retried three times, and the code exchange keeps
/// its transport failure.
#[tokio::test]
async fn a_refusal_whose_body_cannot_be_read_keeps_its_status() {
    for (head, terminal, sends, status) in [
        (
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 100\r\nConnection: close",
            true,
            1,
            401,
        ),
        (
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 100\r\nConnection: close",
            false,
            3,
            502,
        ),
        (
            "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close",
            false,
            3,
            502,
        ),
    ] {
        let (url, connections) = raw_endpoint(head, "partial").await;
        let oauth = CodexOAuth::with_endpoints(wreq::Client::new(), &url, "", "")
            .with_retry_delay(std::time::Duration::from_millis(1));
        let error = oauth.refresh_grant("outer-rt-cut").await.unwrap_err();
        assert_eq!(
            (
                error.terminal,
                connections.load(std::sync::atomic::Ordering::SeqCst),
                error.error.status
            ),
            (terminal, sends, status),
            "{head}"
        );
    }
    let (url, _) = raw_endpoint(
        "HTTP/1.1 401 Unauthorized\r\nContent-Length: 100\r\nConnection: close",
        "partial",
    )
    .await;
    let oauth = CodexOAuth::with_endpoints(wreq::Client::new(), &url, "", "");
    let error = oauth
        .exchange("code", "http://localhost:1455/auth/callback", "verifier")
        .await
        .unwrap_err();
    assert_eq!(
        (error.status, String::from_utf8_lossy(&error.body).into_owned()),
        (502, "codex oauth request failed".to_owned()),
        "the code exchange keeps its transport failure"
    );
}

// ---- #92 A4 completion: refresh-level behaviour, through APIs that also exist before
// this change, so each test compiles against the earlier production code as a witness or
// a control. Sources: OpenAI SIWC "Token reference" (fields, `expires_in: 3600`, a
// replacement refresh token on every success), "Accounts and sessions" (the refresh
// form: issued client, `resource`, no `scope`, which keeps the grant) and "Errors and
// recovery" (unusable-token codes; temporary failures keep the credentials and back
// off). Fetched 2026-11-12. The token-answer reader's own tests are in
// `codex_siwc_tests.rs`.

const DIRECT: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";

/// A SIWC refresh answer with `expires_in` and `scope` spelled raw (`None` leaves the
/// member out), as the token reference lists its fields.
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

/// The documented answer, scope included: what every earlier version also accepts.
fn documented() -> String {
    answer(Some("3600"), Some(&format!(r#""{DIRECT}""#)), "")
}

/// A refresh answer whose token set is usable but whose expiry or scope cannot be read
/// lands its replacement token: the stored refresh token was spent, and losing the
/// replacement would leave only a reuse. Before, each was refused, retried three times
/// with the spent token, and the credential reported the failure. An unreadable expiry
/// clears the stored one, so the new token is due again only after `last_refresh`'s
/// five minutes (its access token here carries no JWT `exp`), never at once. A reported
/// scope is stored as reported; an absent or unreadable one keeps the stored grant.
#[tokio::test]
async fn a_rotation_is_kept_when_its_expiry_or_scope_cannot_be_read() {
    let mock = Mock::start().await;
    let r = refresher(&mock);
    let direct = format!(r#""{DIRECT}""#);
    // The first instant whose year, in the local offset production writes, has five
    // digits (Sol review of 5daaa76c: a UTC boundary is storable west of UTC).
    let unstorable = {
        use chrono::TimeZone as _;
        let last = chrono::Local
            .with_ymd_and_hms(9999, 12, 31, 23, 59, 59)
            .single()
            .unwrap();
        (last.timestamp() + 1 - Utc::now().timestamp()).to_string()
    };
    for (i, (case, expires_in, scope, expiry_known, want_scope)) in [
        (
            "string expires_in",
            Some(r#""3600""#),
            Some(direct.as_str()),
            false,
            DIRECT,
        ),
        (
            "decimal expires_in",
            Some("3600.0"),
            Some(direct.as_str()),
            false,
            DIRECT,
        ),
        (
            "exponent expires_in",
            Some("3.6e3"),
            Some(direct.as_str()),
            false,
            DIRECT,
        ),
        ("no expires_in", None, Some(direct.as_str()), false, DIRECT),
        ("zero expires_in", Some("0"), Some(direct.as_str()), false, DIRECT),
        (
            "expiry past year 9999",
            Some(unstorable.as_str()),
            Some(direct.as_str()),
            false,
            DIRECT,
        ),
        (
            "expires_in past i64",
            Some("18446744073709551615"),
            Some(direct.as_str()),
            false,
            DIRECT,
        ),
        ("null scope", Some("3600"), Some("null"), true, "stored-grant"),
        ("blank scope", Some("3600"), Some(r#"" ""#), true, "stored-grant"),
        (
            "scope without direct use",
            Some("3600"),
            Some(r#""openid""#),
            true,
            "openid",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let mut metadata = doc(&format!("keep-{i}"), true, 60 * 24 * 10, -1);
        metadata["siwc"]["scope"] = "stored-grant".into();
        metadata["siwc"]["expires_at"] = (Utc::now() - chrono::TimeDelta::minutes(1)).to_rfc3339().into();
        mock.script(SIWC, vec![json(200, &answer(expires_in, scope, ""))]);
        let (result, refused) = r
            .refresh_locked(&credential(&metadata), &metadata, RefreshMode::Due, &Refused::default())
            .await;
        assert_eq!(sent_to(&mock), [SIWC], "{case}: sent once");
        assert!(refused.is_empty(), "{case}: never a refusal");
        let patch = result.unwrap_or_else(|e| panic!("{case}: {e:?}"));
        let after = &patch
            .siwc
            .as_ref()
            .unwrap_or_else(|| panic!("{case}: no SIWC change"))
            .after;
        assert_eq!(
            (&after["access_token"], &after["refresh_token"]),
            (&"siwc-at-2".into(), &"siwc-rt-2".into()),
            "{case}: the replacement lands"
        );
        assert_eq!(after["scope"], want_scope, "{case}");
        let mut renewed = credential(&metadata);
        let mut partition = metadata["siwc"].as_object().unwrap().clone();
        partition.extend(after.clone());
        renewed.metadata["siwc"] = Value::Object(partition);
        let now = Utc::now();
        assert_eq!(
            crate::codex_siwc::refresh_due(&renewed, now),
            Some(false),
            "{case}: not due at once"
        );
        if expiry_known {
            assert!(after["expired"].is_string(), "{case}");
        } else {
            assert_eq!(
                (&after["expired"], &after["expires_at"]),
                (&Value::Null, &Value::Null),
                "{case}"
            );
            let later = now + chrono::TimeDelta::seconds(301);
            assert_eq!(
                crate::codex_siwc::refresh_due(&renewed, later),
                Some(true),
                "{case}: due after last_refresh's lead"
            );
        }
    }
    // Without a usable access token there is no token set to keep: refused, sent once.
    let metadata = doc("keep-no-access", true, 60 * 24 * 10, -1);
    mock.script(
        SIWC,
        vec![json(200, r#"{"refresh_token":"siwc-rt-2","expires_in":3600}"#); 3],
    );
    let (result, refused) = r
        .refresh_locked(&credential(&metadata), &metadata, RefreshMode::Due, &Refused::default())
        .await;
    assert_eq!(sent_to(&mock), [SIWC]);
    assert!(refused.is_empty() && result.is_err());
}

/// The SIWC refresh form, byte for byte (Accounts and sessions): form-encoded
/// `grant_type=refresh_token`, the issued client saved with the token set (never
/// `dynamic_agent_client`), the saved refresh token and `resource`, with no `scope`.
#[tokio::test]
async fn the_siwc_refresh_form_is_the_documented_one() {
    let mock = Mock::start().await;
    let r = refresher(&mock);
    let mut metadata = doc("form", true, 60 * 24 * 10, -1);
    metadata["siwc"]["client_id"] = "oaiapp_issued_fake".into();
    mock.script(SIWC, vec![json(200, &answer(Some("3600"), None, ""))]);
    let (result, refused) = r
        .refresh_locked(&credential(&metadata), &metadata, RefreshMode::Due, &Refused::default())
        .await;
    let sent = mock.take();
    assert_eq!(sent.len(), 1, "an answer without `scope` is accepted, not retried");
    let patch = result.expect("an answer without `scope` keeps the stored grant");
    assert!(!patch.is_empty() && refused.is_empty());
    assert_eq!(sent[0].header("content-type"), "application/x-www-form-urlencoded");
    assert_eq!(
        String::from_utf8_lossy(&sent[0].body),
        "client_id=oaiapp_issued_fake&grant_type=refresh_token&refresh_token=siwc-rt-form&resource=https%3A%2F%2Fapi.openai.com%2Fv1"
    );
}

/// An outer (ChatGPT OAuth) refresh whose `expires_in` puts the expiry past chrono's
/// range: before, adding it panicked inside the detached refresh task, the caller got a
/// 500 and the endpoint's rotated refresh token was lost. It now keeps the rotated
/// tokens, sent once, and leaves `expired` empty, so the JWT `exp` or `last_refresh`
/// schedules the next refresh.
#[tokio::test]
async fn an_outer_lifetime_past_chrono_keeps_the_rotated_token() {
    let mock = Mock::start().await;
    let oauth = CodexOAuth::with_endpoints(wreq::Client::new(), &format!("{}{OUTER}", mock.url), "", "")
        .with_retry_delay(std::time::Duration::from_millis(1));
    mock.script(
        OUTER,
        vec![json(
            200,
            r#"{"access_token":"outer-at-far","refresh_token":"outer-rt-far","expires_in":9000000000000000}"#,
        )],
    );
    let tokens = oauth.refresh_grant("outer-rt-overflow").await.unwrap();
    assert_eq!(sent_to(&mock), [OUTER]);
    assert_eq!(
        (tokens.refresh_token.as_str(), tokens.expired.as_str()),
        ("outer-rt-far", "")
    );
}

/// A due SIWC refresh that fails transiently while the native tokens are in use is
/// tolerated, and the scheduler checks again after 30 s. The failure is retained for the
/// token like an outer one, so those checks send nothing until it expires; before, each
/// sent the refresh token three more times. An explicit (management) refresh still asks
/// again. A refusal for good is never sent again, by either.
#[tokio::test]
async fn a_scheduled_siwc_failure_backs_off_and_an_explicit_refresh_asks_again() {
    let mock = Mock::start().await;
    let r = refresher(&mock);
    // Native in use and valid for ten days; the SIWC partition expired, so due.
    let c = credential(&doc("backoff", false, 60 * 24 * 10, -1));
    mock.script(SIWC, vec![json(503, r#"{"error":"temporarily_unavailable"}"#); 3]);
    assert!(r.prepare(&c).await.unwrap().set.is_empty(), "tolerated: native in use");
    assert_eq!(sent_to(&mock), [SIWC; 3]);
    for _ in 0..3 {
        assert!(r.prepare(&c).await.unwrap().set.is_empty());
        assert!(mock.take().is_empty(), "a retained failure sends nothing");
    }
    mock.script(OUTER, vec![outer_ok()]);
    mock.script(SIWC, vec![json(200, &answer(Some("3600"), None, ""))]);
    let patch = r.refresh_all(&c).await.unwrap();
    assert_eq!(sent_to(&mock), [OUTER, SIWC], "an explicit refresh asks again");
    assert_eq!(patch.set["siwc"]["refresh_token"], "siwc-rt-2");
    // A terminal refusal is retained for both kinds of caller.
    let dead = credential(&doc("backoff-dead", false, 60 * 24 * 10, -1));
    mock.script(SIWC, vec![json(400, r#"{"error":"refresh_token_reused"}"#)]);
    r.prepare(&dead).await.unwrap();
    assert_eq!(sent_to(&mock), [SIWC]);
    mock.script(OUTER, vec![outer_ok()]);
    let outer_only = r.refresh_all(&dead).await.unwrap();
    assert!(!outer_only.set.contains_key("siwc"));
    assert_eq!(sent_to(&mock), [OUTER], "the refused SIWC token is not sent again");
}

/// A SIWC token endpoint that holds every answer until `gate` opens, counting requests.
async fn gated_endpoint(answer: String) -> (String, Arc<tokio::sync::Notify>, Arc<std::sync::atomic::AtomicUsize>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}{SIWC}", listener.local_addr().unwrap());
    let gate = Arc::new(tokio::sync::Notify::new());
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (held, counted) = (gate.clone(), requests.clone());
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let (held, counted, answer) = (held.clone(), counted.clone(), answer.clone());
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                // The request line and headers, then the announced body.
                loop {
                    let read = stream.read(&mut buffer).await.unwrap_or(0);
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                    let length = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                held.notified().await;
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                    answer.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
            });
        }
    });
    (url, gate, requests)
}

/// A caller cancelled while its SIWC refresh is in flight does not strand the rotated
/// token: the grant runs detached, its answer is retained, and the next caller with the
/// same refresh token receives the replacement without sending the spent token again.
#[tokio::test]
async fn a_cancelled_siwc_refresh_keeps_its_replacement_token() {
    let (url, gate, requests) = gated_endpoint(documented()).await;
    let oauth = CodexOAuth::with_endpoints(wreq::Client::new(), "", "", "")
        .with_siwc_token_url(&url)
        .with_retry_delay(std::time::Duration::from_millis(1));
    let first = oauth.clone();
    let caller = tokio::spawn(async move { first.refresh_siwc("siwc-rt-cancel", "oaiapp_fake", "dev").await });
    let sent = async {
        while requests.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), sent)
        .await
        .expect("the grant was sent");
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    gate.notify_one();
    let tokens = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        oauth.refresh_siwc("siwc-rt-cancel", "oaiapp_fake", "dev"),
    )
    .await
    .expect("the detached grant ends")
    .unwrap();
    assert_eq!(tokens.refresh_token, "siwc-rt-2");
    assert_eq!(
        requests.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the spent token is sent once"
    );
}

/// Native (outer) and SIWC partitions recover independently on a due refresh. Requests
/// stay on the token set `use_siwc` selects: a SIWC failure never sends the outer
/// tokens in its place, and a native credential stays usable while its SIWC partition
/// fails or is refused.
#[tokio::test]
async fn native_and_siwc_partitions_recover_independently() {
    let mock = Mock::start().await;
    let r = refresher(&mock);
    let transient = || vec![json(503, r#"{"error":"temporarily_unavailable"}"#); 3];
    let refused = || vec![json(400, r#"{"error":"invalid_grant"}"#)];
    let siwc_ok = || vec![json(200, &documented())];
    let outer_refused = || vec![json(400, r#"{"error":"invalid_grant"}"#)];
    struct Row {
        case: &'static str,
        use_siwc: bool,
        outer_minutes: i64,
        siwc_minutes: i64,
        outer: Option<Vec<crate::codex_testkit::Reply>>,
        siwc: Vec<crate::codex_testkit::Reply>,
        sent: &'static [&'static str],
        ok: bool,
        patched: (bool, bool),
        refused: (bool, bool),
        active_refused: bool,
    }
    let ten_days = 60 * 24 * 10;
    let rows = [
        Row {
            case: "native in use, SIWC temporarily failing",
            use_siwc: false,
            outer_minutes: ten_days,
            siwc_minutes: -1,
            outer: None,
            siwc: transient(),
            sent: &[SIWC, SIWC, SIWC],
            ok: true,
            patched: (false, false),
            refused: (false, false),
            active_refused: false,
        },
        Row {
            case: "native in use, SIWC refused",
            use_siwc: false,
            outer_minutes: ten_days,
            siwc_minutes: -1,
            outer: None,
            siwc: refused(),
            sent: &[SIWC],
            ok: true,
            patched: (false, false),
            refused: (false, true),
            active_refused: false,
        },
        Row {
            case: "SIWC in use and still valid, temporarily failing",
            use_siwc: true,
            outer_minutes: ten_days,
            siwc_minutes: 4,
            outer: None,
            siwc: transient(),
            sent: &[SIWC, SIWC, SIWC],
            ok: true,
            patched: (false, false),
            refused: (false, false),
            active_refused: false,
        },
        Row {
            case: "SIWC in use and expired, temporarily failing",
            use_siwc: true,
            outer_minutes: ten_days,
            siwc_minutes: -1,
            outer: None,
            siwc: transient(),
            sent: &[SIWC, SIWC, SIWC],
            ok: false,
            patched: (false, false),
            refused: (false, false),
            active_refused: false,
        },
        Row {
            case: "SIWC in use and expired, refused",
            use_siwc: true,
            outer_minutes: ten_days,
            siwc_minutes: -1,
            outer: None,
            siwc: refused(),
            sent: &[SIWC],
            ok: false,
            patched: (false, false),
            refused: (false, true),
            active_refused: true,
        },
        Row {
            case: "both due: outer renewed, SIWC temporarily failing",
            use_siwc: true,
            outer_minutes: 60,
            siwc_minutes: 4,
            outer: Some(vec![outer_ok()]),
            siwc: transient(),
            sent: &[OUTER, SIWC, SIWC, SIWC],
            ok: true,
            patched: (true, false),
            refused: (false, false),
            active_refused: false,
        },
        Row {
            case: "both due: outer refused, SIWC renewed",
            use_siwc: true,
            outer_minutes: 60,
            siwc_minutes: 4,
            outer: Some(outer_refused()),
            siwc: siwc_ok(),
            sent: &[OUTER, SIWC],
            ok: true,
            patched: (false, true),
            refused: (true, false),
            active_refused: false,
        },
    ];
    for (i, row) in rows.into_iter().enumerate() {
        let metadata = doc(
            &format!("matrix-{i}"),
            row.use_siwc,
            row.outer_minutes,
            row.siwc_minutes,
        );
        let c = credential(&metadata);
        if let Some(outer) = row.outer {
            mock.script(OUTER, outer);
        }
        mock.script(SIWC, row.siwc);
        let (result, now_refused) = r
            .refresh_locked(&c, &metadata, RefreshMode::Due, &Refused::default())
            .await;
        let case = row.case;
        assert_eq!(sent_to(&mock), row.sent, "{case}: sends");
        assert_eq!(result.is_ok(), row.ok, "{case}: {result:?}");
        if let Ok(patch) = &result {
            assert_eq!(
                (patch.outer.is_some(), patch.siwc.is_some()),
                row.patched,
                "{case}: patch"
            );
        }
        assert_eq!(
            (now_refused.outer_refused(&c), now_refused.siwc_refused(&c)),
            row.refused,
            "{case}: refused"
        );
        assert_eq!(now_refused.active_refused(&c), row.active_refused, "{case}: active");
    }
}

/// A SIWC refresh answered 200 whose body is cut short: the endpoint accepted the grant
/// and may have spent the refresh token, so the refresh does not send it again (it did,
/// three times). The failure is transient and ambiguous, not a refusal: nothing is
/// refused or applied and the stored token stays. A due refresh keeps the failure as a
/// backoff, so the scheduler's next check sends nothing either. The outer refresh keeps
/// its own, separately sourced handling (`a_refusal_whose_body_cannot_be_read_keeps_its_status`).
#[tokio::test]
async fn a_siwc_200_cut_short_is_sent_once_and_backs_off() {
    let (url, connections) =
        raw_endpoint("HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close", "partial").await;
    let oauth = CodexOAuth::with_endpoints(wreq::Client::new(), "", "", "")
        .with_siwc_token_url(&url)
        .with_retry_delay(std::time::Duration::from_millis(1));
    let r = Refresher::new(oauth);
    let metadata = doc("cut", true, 60 * 24 * 10, -1);
    let sent = || connections.load(std::sync::atomic::Ordering::SeqCst);
    let (result, refused) = r
        .refresh_locked(&credential(&metadata), &metadata, RefreshMode::Due, &Refused::default())
        .await;
    assert_eq!(sent(), 1, "sent once, not again in the same refresh");
    assert!(refused.is_empty(), "not a refusal");
    let error = result.expect_err("the SIWC token in use has expired, so the failure is reported");
    assert_eq!(error.status, 502);
    let (again, refused) = r
        .refresh_locked(&credential(&metadata), &metadata, RefreshMode::Due, &Refused::default())
        .await;
    assert!(again.is_err() && refused.is_empty());
    assert_eq!(sent(), 1, "the next due check waits for the backoff");
}

/// A SIWC token endpoint that reads each request whole, answers with `head` and then
/// sends nothing more, holding the connection open; counts the connections.
async fn stalled_endpoint(head: &'static str) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}{SIWC}", listener.local_addr().unwrap());
    let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = connections.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    let read = stream.read(&mut buffer).await.unwrap_or(0);
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                    let length = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
                let _ = stream.write_all(format!("{head}\r\n\r\n").as_bytes()).await;
                let _ = stream.flush().await;
                // Hold the connection without a body until the test ends.
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                drop(stream);
            });
        }
    });
    (url, connections)
}

/// Sol review of 5daaa76c: a SIWC refresh whose 200 arrives but whose body stalls past
/// the attempt's deadline is answered, not an ordinary timeout: the endpoint may have
/// rotated the token, so the refresh sends it once, reports a transient failure (never
/// a refusal), and the next due check waits for the backoff. Before, the attempt's
/// timeout read as transient and the same refresh sent the token again, where a
/// `refresh_token_reused` answer would then refuse the session.
#[tokio::test]
async fn a_siwc_200_whose_body_stalls_is_sent_once_and_backs_off() {
    let (url, connections) =
        stalled_endpoint("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100").await;
    let oauth = CodexOAuth::with_endpoints(wreq::Client::new(), "", "", "")
        .with_siwc_token_url(&url)
        .with_retry_delay(std::time::Duration::from_millis(1))
        .with_grant_timeout(std::time::Duration::from_millis(300));
    let r = Refresher::new(oauth);
    let metadata = doc("stall", true, 60 * 24 * 10, -1);
    let sent = || connections.load(std::sync::atomic::Ordering::SeqCst);
    let (result, refused) = r
        .refresh_locked(&credential(&metadata), &metadata, RefreshMode::Due, &Refused::default())
        .await;
    assert_eq!(sent(), 1, "sent once, not again in the same refresh");
    assert!(refused.is_empty(), "not a refusal");
    let error = result.expect_err("the SIWC token in use has expired, so the failure is reported");
    assert_eq!(error.status, 502);
    assert!(
        String::from_utf8_lossy(&error.body).contains("the refresh token was not sent again"),
        "{error:?}"
    );
    let (again, refused) = r
        .refresh_locked(&credential(&metadata), &metadata, RefreshMode::Due, &Refused::default())
        .await;
    assert!(again.is_err() && refused.is_empty());
    assert_eq!(sent(), 1, "the next due check waits for the backoff");
}

/// An explicit refresh that starts while a scheduled refresh of the same token is in
/// flight joins it: one grant, both get the replacement, and the explicit caller's
/// bypass of a retained failure never starts a second, concurrent send of the token.
#[tokio::test]
async fn an_explicit_refresh_joins_an_in_flight_scheduled_one() {
    let (url, gate, requests) = gated_endpoint(documented()).await;
    let oauth = CodexOAuth::with_endpoints(wreq::Client::new(), "", "", "")
        .with_siwc_token_url(&url)
        .with_retry_delay(std::time::Duration::from_millis(1));
    let scheduled = oauth.clone();
    let first = tokio::spawn(async move {
        scheduled
            .refresh_siwc_as("siwc-rt-overlap", "oaiapp_fake", "dev", true)
            .await
    });
    let in_flight = async {
        while requests.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), in_flight)
        .await
        .expect("the scheduled grant was sent");
    let explicit = oauth.clone();
    let second = tokio::spawn(async move { explicit.refresh_siwc("siwc-rt-overlap", "oaiapp_fake", "dev").await });
    tokio::task::yield_now().await;
    gate.notify_one();
    let (a, b) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        (first.await.unwrap(), second.await.unwrap())
    })
    .await
    .expect("both callers end");
    assert_eq!(
        (a.unwrap().refresh_token, b.unwrap().refresh_token),
        ("siwc-rt-2".to_owned(), "siwc-rt-2".to_owned())
    );
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1, "one grant");
}
