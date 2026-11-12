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
