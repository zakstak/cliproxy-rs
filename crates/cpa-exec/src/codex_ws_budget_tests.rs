//! FP4 #118: an upstream's advertised message limit (`x-codex-websocket-max-message-bytes`
//! on its 101), read as first-party Codex reads it (openai/codex `ed7ca22bb60a`, inspected
//! at `5c3ff1dabf64`). A longer request is refused before any byte is written, as the
//! 413 `message_too_big` a WebSocket client receives as close 1009, and is never sent
//! elsewhere. The raw-TCP upstream of [`super::fragment`] reports every client frame.
//! Fake credentials, loopback only.

use super::fragment::{Plan, messages, raw_upstream};
use super::*;

/// The first-party parser: the first value, `str::parse::<usize>` or nothing.
#[test]
fn peer_limits_read_as_the_first_party_parser_reads_them() {
    let read = |values: &[&str]| {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(
                "x-codex-websocket-max-message-bytes",
                http::HeaderValue::from_str(value).unwrap(),
            );
        }
        peer_message_limit(&headers)
    };
    assert_eq!(read(&[]), None, "absent");
    for invalid in [
        "",
        "abc",
        "-1",
        "1.5",
        "1e3",
        " 5",
        "5 ",
        "0x10",
        "99999999999999999999999",
    ] {
        assert_eq!(read(&[invalid]), None, "{invalid:?}");
    }
    assert_eq!(read(&["0"]), Some(0), "zero is a limit");
    assert_eq!(read(&["+5"]), Some(5));
    assert_eq!(read(&["007"]), Some(7));
    assert_eq!(read(&["5", "9"]), Some(5), "the first value decides");
    assert_eq!(read(&["abc", "9"]), None, "an unreadable first value is no limit");
}

/// Below, at and above the advertised limit, a zero limit, an absent or unreadable one, and
/// one larger than the proxy's own 64 MiB receive limit. A request the limit admits is
/// written as before; a longer one fails with the request-scoped 413 before any data
/// frame reaches the upstream, on the one socket that was dialed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_over_the_upstream_limit_is_refused_before_dispatch() {
    let sent = request(BODY).body.len();
    let cases = [
        (None, true),
        (Some("abc".to_owned()), true),
        (Some(sent.to_string()), true),
        (Some((64 * 1024 * 1024 + 1).to_string()), true),
        (Some((sent - 1).to_string()), false),
        (Some("0".to_owned()), false),
        (Some("1".to_owned()), false),
    ];
    for (limit, admitted) in cases {
        let (url, mut frames) = raw_upstream(Plan {
            limit: limit.clone(),
            answer: true,
            ..Plan::default()
        })
        .await;
        let executor = CodexExecutor::new().unwrap();
        let result = executor
            .execute_in_session(&credential(&url), request(BODY), &Config::default(), &session(false))
            .await;
        if admitted {
            let events: Vec<String> = collect(result.unwrap()).await.into_iter().map(Result::unwrap).collect();
            assert_eq!(events.len(), 2, "{limit:?}: {events:?}");
            let (got, _) = messages(&mut frames, 1).await;
            assert_eq!(got[0].bytes.len(), sent, "{limit:?}");
            executor.close_session("conn-1");
            continue;
        }
        let Err(error) = result else {
            panic!("{limit:?}: a request over the limit is refused");
        };
        assert_eq!((error.status, error.scope), (413, FailureScope::Request), "{limit:?}");
        let body = String::from_utf8_lossy(&error.body).into_owned();
        assert_eq!(crate::rawjson::get(&body, "error.code").str(), "message_too_big");
        assert_eq!(
            crate::rawjson::get(&body, "error.message").str(),
            format!(
                "request exceeds the upstream websocket message limit of {} bytes",
                limit.as_deref().unwrap()
            )
        );
        // The refused socket is closed; nothing but the close frame ever reached it.
        while let Ok(Some(frame)) = tokio::time::timeout(Duration::from_secs(2), frames.recv()).await {
            assert_eq!(frame.opcode, 8, "{limit:?}: no data frame is sent");
        }
    }
}

/// The limit applies to every write on the socket, not just a turn's create: a steering
/// frame or interrupt longer than it is refused unsent as well.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_write_on_a_limited_socket_respects_the_limit() {
    let (url, mut frames) = raw_upstream(Plan {
        limit: Some("8".to_owned()),
        ..Plan::default()
    })
    .await;
    let executor = CodexExecutor::new().unwrap();
    let conn = pooled(&executor, &url).await;
    conn.send_create(Bytes::from_static(b"12345678"), None).await.unwrap();
    let (got, _) = messages(&mut frames, 1).await;
    assert_eq!(got[0].bytes, b"12345678");
    let failure = conn
        .send_authorized("123456789".into(), None)
        .await
        .expect_err("one byte over");
    assert!(failure.unsent);
    assert_eq!(failure.error.status, 413);
    assert!(conn.link.lock().unwrap().lost.is_none(), "the socket itself is fine");
    executor.close_session("conn-1");
}
