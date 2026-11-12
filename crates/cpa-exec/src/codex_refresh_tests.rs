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
/// the answer is applied, and the stored refresh token stays for a later attempt.
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
            3
        ],
    );
    let (result, refused) = r
        .refresh_locked(&credential(&metadata), &metadata, RefreshMode::Due, &Refused::default())
        .await;
    assert_eq!(sent_to(&mock), [SIWC; 3], "retried as a transient fault");
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
            3
        ],
    );
    let (result, _) = r
        .refresh_locked(&credential(&expired), &expired, RefreshMode::Due, &Refused::default())
        .await;
    let error = result.unwrap_err();
    assert_eq!(
        String::from_utf8_lossy(&error.body),
        "SIWC token refresh failed after 3 attempts: OpenAI OAuth token response has invalid refresh_token"
    );
    mock.take();
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

// ---- #92 A4 completion: response and expiry variants, a controlled clock, backoff,
// cancellation and the native/SIWC partition matrix. Sources: OpenAI SIWC "Token
// reference" (fields, `expires_in: 3600`, a replacement refresh token on every success),
// "Accounts and sessions" (the refresh form: issued client, `resource`, no `scope`, which
// keeps the grant) and "Errors and recovery" (unusable-token codes; temporary failures
// keep the credentials and back off). Fetched 2026-11-12.

/// A fixed local clock: 2026-11-12T13:33:20Z.
fn clock() -> chrono::DateTime<chrono::Local> {
    use chrono::TimeZone as _;
    chrono::Local.timestamp_opt(1_794_490_400, 0).unwrap()
}

fn at(seconds: i64) -> chrono::DateTime<Utc> {
    clock().with_timezone(&Utc) + chrono::TimeDelta::seconds(seconds)
}

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

fn read(body: &str, exchange: bool) -> Result<crate::codex_siwc::Tokens, String> {
    crate::codex_siwc::token_response(body.as_bytes(), " oaiapp_fake ", " dev-fake ", exchange, clock())
        .map_err(|e| String::from_utf8_lossy(&e.body).into_owned())
}

/// The documented answer reads into a token set at the fixed clock: the real expiry
/// `expires_in` after it, the scheduling expiry three minutes earlier (Go's margin), and
/// the replacement refresh token. Every documented field may be present; the ones with
/// no documented constraint change nothing.
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
    let instant = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
    assert_eq!(instant(&tokens.last_refresh), at(0));
    assert_eq!(instant(&tokens.expires_at), at(3600));
    assert_eq!(instant(&tokens.expired), at(3600 - 180));
    // `earliest_refresh_at` has no documented format or meaning: whatever its spelling,
    // the token set is the same, and it is never persisted.
    for earliest in [
        "1794494000",
        r#""2026-11-12T14:33:20Z""#,
        r#""in an hour""#,
        "null",
        r#"{"seconds":3600}"#,
    ] {
        let with = read(
            &answer(
                Some("3600"),
                Some(&scope),
                &format!(r#""earliest_refresh_at":{earliest}"#),
            ),
            false,
        )
        .unwrap();
        assert_eq!(with, tokens, "earliest_refresh_at {earliest}");
        assert!(!with.to_map().contains_key("earliest_refresh_at"));
    }
}

/// `expires_in` is the documented positive JSON integer. Other spellings are refused at
/// the boundary, never coerced (Go v8.0.67 accepts `3600.0`, `3.6e3` and `"3600"`). A
/// lifetime past Go's `time.Duration` is refused: before, chrono's overflow fell back to
/// "expires now", so every check found the new token due again.
#[test]
fn siwc_expires_in_is_the_documented_integer() {
    let scope = format!(r#""{DIRECT}""#);
    let max = (i64::MAX / 1_000_000_000).to_string();
    let over = (i64::MAX / 1_000_000_000 + 1).to_string();
    for ok in ["3600", "1", "180", "181", max.as_str()] {
        assert!(read(&answer(Some(ok), Some(&scope), ""), false).is_ok(), "{ok}");
    }
    for refused in [
        "0",
        "-1",
        "3600.0",
        "3.6e3",
        "3600.5",
        r#""3600""#,
        "null",
        "true",
        "1e20",
        "18446744073709551616",
        "1000000000000",
        over.as_str(),
    ] {
        assert_eq!(
            read(&answer(Some(refused), Some(&scope), ""), false).unwrap_err(),
            "OpenAI OAuth token response has invalid expires_in",
            "{refused}"
        );
    }
    assert_eq!(
        read(&answer(None, Some(&scope), ""), false).unwrap_err(),
        "OpenAI OAuth token response has invalid expires_in",
        "absent"
    );
    // A one-second grant keeps its whole lifetime rather than going negative.
    let short = read(&answer(Some("1"), Some(&scope), ""), false).unwrap();
    assert_eq!(short.expired, short.expires_at);
    // The longest accepted lifetime is far from due, not due at once.
    let long = read(&answer(Some(&max), Some(&scope), ""), false).unwrap();
    let c = siwc_credential("long", &long);
    assert_eq!(crate::codex_siwc::refresh_due(&c, at(0)), Some(false));
    assert_eq!(crate::codex_siwc::refresh_due(&c, at(366 * 24 * 3600)), Some(false));
}

/// The other fields: tokens are non-blank strings, and the replacement refresh token is
/// required (token reference). A refresh may leave out `scope`: it asked with none, which
/// keeps the grant (Accounts and sessions; RFC 6749 §5.1), and the stored scope stays. A
/// present scope must be a non-blank string holding `chatgpt.tokens.use.direct`; the code
/// exchange must carry it and the ID token.
#[test]
fn siwc_token_answer_fields_are_read_at_the_boundary() {
    let scope = format!(r#""{DIRECT}""#);
    let body = |s: &str| s.to_owned();
    for (case, text, exchange, want) in [
        (
            "no replacement refresh token",
            body(r#"{"access_token":"a","expires_in":3600,"scope":"chatgpt.tokens.use.direct"}"#),
            false,
            Err("OpenAI OAuth token response has invalid refresh_token"),
        ),
        (
            "blank replacement",
            body(r#"{"access_token":"a","refresh_token":"  ","expires_in":3600}"#),
            false,
            Err("OpenAI OAuth token response has invalid refresh_token"),
        ),
        (
            "null replacement",
            body(r#"{"access_token":"a","refresh_token":null,"expires_in":3600}"#),
            false,
            Err("OpenAI OAuth token response has invalid refresh_token"),
        ),
        (
            "numeric access token",
            body(r#"{"access_token":5,"refresh_token":"r","expires_in":3600}"#),
            false,
            Err("OpenAI OAuth token response has invalid access_token"),
        ),
        (
            "null ID token",
            body(r#"{"access_token":"a","refresh_token":"r","id_token":null,"expires_in":3600}"#),
            false,
            Err("OpenAI OAuth token response has invalid id_token"),
        ),
        (
            "refresh without ID token or scope",
            body(r#"{"access_token":"a","refresh_token":"r","expires_in":3600}"#),
            false,
            Ok(""),
        ),
        (
            "blank scope",
            body(r#"{"access_token":"a","refresh_token":"r","expires_in":3600,"scope":" "}"#),
            false,
            Err("OpenAI OAuth token response has invalid scope"),
        ),
        (
            "null scope",
            body(r#"{"access_token":"a","refresh_token":"r","expires_in":3600,"scope":null}"#),
            false,
            Err("OpenAI OAuth token response has invalid scope"),
        ),
        (
            "scope without direct use",
            body(r#"{"access_token":"a","refresh_token":"r","expires_in":3600,"scope":"openid"}"#),
            false,
            Err("OpenAI OAuth grant did not include chatgpt.tokens.use.direct"),
        ),
        (
            "exchange without scope",
            body(r#"{"access_token":"a","refresh_token":"r","id_token":"i","expires_in":3600}"#),
            true,
            Err("OpenAI OAuth token response has invalid scope"),
        ),
        (
            "exchange without ID token",
            format!(r#"{{"access_token":"a","refresh_token":"r","expires_in":3600,"scope":{scope}}}"#),
            true,
            Err("OpenAI OAuth token response did not contain an ID token"),
        ),
        (
            "an array",
            body("[]"),
            false,
            Err("failed to parse SIWC token response"),
        ),
        ("null", body("null"), false, Err("failed to parse SIWC token response")),
        (
            "not JSON",
            body("not json"),
            false,
            Err("failed to parse SIWC token response"),
        ),
    ] {
        let got = read(&text, exchange);
        match want {
            Ok(scope) => assert_eq!(got.map(|t| t.scope).as_deref(), Ok(scope), "{case}"),
            Err(message) => assert_eq!(got.unwrap_err(), message, "{case}"),
        }
    }
    // An omitted scope keeps the partition's stored grant.
    let tokens = read(r#"{"access_token":"a2","refresh_token":"r2","expires_in":3600}"#, false).unwrap();
    let mut stored = credential(&doc("scope-kept", true, 60, 60));
    stored.metadata["siwc"]["scope"] = DIRECT.into();
    stored.metadata["siwc"]["scopes"] = serde_json::json!(DIRECT.split(' ').collect::<Vec<_>>());
    let patch = crate::codex_siwc::refresh_patch(&stored, &tokens);
    let siwc = &patch.set["siwc"];
    assert_eq!(
        (&siwc["access_token"], &siwc["refresh_token"]),
        (&"a2".into(), &"r2".into())
    );
    assert_eq!(siwc["scope"], DIRECT);
    assert_eq!(siwc["scopes"], stored.metadata["siwc"]["scopes"]);
}

/// A Codex credential whose SIWC partition is `tokens` (what a refresh writes).
fn siwc_credential(id: &str, tokens: &crate::codex_siwc::Tokens) -> Credential {
    let mut c = credential(&doc(id, true, 60 * 24 * 30, 60));
    c.metadata["siwc"] = Value::Object(tokens.to_map());
    c
}

/// The renewal schedule of a refreshed SIWC partition on a controlled clock: due five
/// minutes before the margin-deducted expiry (57 minutes after the grant, so at 52
/// minutes), usable until that expiry, and reported `valid` until the real one. An
/// `earliest_refresh_at` in the answer moves none of it. Without a refresh token or an
/// issued client the partition is never due, at any time: the loop stops.
#[test]
fn a_refreshed_siwc_partition_follows_a_controlled_clock() {
    let tokens = read(
        &answer(
            Some("3600"),
            Some(&format!(r#""{DIRECT}""#)),
            r#""earliest_refresh_at":1794497600"#,
        ),
        false,
    )
    .unwrap();
    let c = siwc_credential("clock", &tokens);
    let due = |t| crate::codex_siwc::refresh_due(&c, at(t));
    assert_eq!(
        (due(0), due(52 * 60 - 1), due(52 * 60)),
        (Some(false), Some(false), Some(true))
    );
    assert!(crate::codex_siwc::access_usable(&c, at(57 * 60 - 1)));
    assert!(!crate::codex_siwc::access_usable(&c, at(57 * 60)));
    assert_eq!(crate::codex_siwc::status(&c, false, at(3599)), "valid");
    assert_eq!(crate::codex_siwc::status(&c, false, at(3600)), "invalid");
    assert_eq!(crate::codex_siwc::status(&c, true, at(0)), "invalid", "rejected");
    // The JWT `exp` claim (integer Unix seconds, token reference) schedules a partition
    // that stores no expiry.
    use base64::Engine as _;
    let claims =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{}}}"#, at(3600).timestamp()));
    let mut jwt = credential(&doc("jwt", true, 60 * 24 * 30, 60));
    jwt.metadata["siwc"] = serde_json::json!({"access_token": format!("e30.{claims}.c2ln"),
        "refresh_token": "siwc-rt-jwt", "client_id": "oaiapp_fake"});
    let due = |t| crate::codex_siwc::refresh_due(&jwt, at(t));
    assert_eq!((due(55 * 60 - 1), due(55 * 60)), (Some(false), Some(true)));
    // Missing renewal credentials: never due, at any time.
    for (case, remove) in [("refresh token", "refresh_token"), ("issued client", "client_id")] {
        let mut missing = c.clone();
        missing.metadata["siwc"].as_object_mut().unwrap().remove(remove);
        for t in [0, 3600, 30 * 24 * 3600, 365 * 24 * 3600] {
            assert_eq!(crate::codex_siwc::refresh_due(&missing, at(t)), None, "{case} at {t}");
        }
    }
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
    assert!(!result.unwrap().is_empty() && refused.is_empty());
    let sent = mock.take();
    assert_eq!(sent.len(), 1);
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
    let (url, gate, requests) = gated_endpoint(answer(Some("3600"), None, "")).await;
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
    let siwc_ok = || vec![json(200, &answer(Some("3600"), None, ""))];
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
