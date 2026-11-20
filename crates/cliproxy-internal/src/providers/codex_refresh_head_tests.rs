//! #92 refresh tests that need APIs this branch adds (`with_grant_timeout`,
//! `refresh_siwc_as`). They live apart from `codex_refresh_tests.rs`, whose tests use only
//! APIs the base also has, so that file can be composed onto the base's production code
//! as a witness tree; this module is declared only by this branch's `codex_refresh.rs`.

use std::sync::Arc;

use super::tests::{SIWC, credential, doc, documented, gated_endpoint};
use super::*;

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
