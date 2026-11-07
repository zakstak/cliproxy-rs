//! The capture upstream records what it receives byte for byte and answers from its
//! script. Payloads come from `cpa_exec::codex_contract`, which also shows that route
//! tests can combine the two.

use std::time::Duration;

use cpa_exec::codex_contract::fixture;
use futures_util::StreamExt;
use wreq::ws::message::Message;

use super::*;

#[tokio::test]
async fn http_request_body_and_headers_are_recorded_exactly() {
    let body = fixture("probe-request-unknown-fields").bytes();
    let stream = fixture("sse-completed").bytes();
    let upstream = CaptureUpstream::start(Script {
        http: vec![Reply::sse(stream.clone())],
        ..Script::default()
    })
    .await;

    let response = wreq::Client::new()
        .post(format!("{}/responses?x=1", upstream.base_url))
        .header("x-probe", "a")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    assert_eq!(response.bytes().await.unwrap(), stream);

    let [exchange] = upstream.http().try_into().unwrap();
    assert_eq!(exchange.method, Method::POST);
    assert_eq!(exchange.uri.to_string(), "/responses?x=1");
    assert_eq!(exchange.headers["x-probe"], "a");
    assert_eq!(exchange.body, body);

    // An unscripted request is visible as a 500, not a hang.
    let extra = wreq::Client::new()
        .post(format!("{}/responses", upstream.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(extra.status().as_u16(), 500);
}

#[tokio::test]
async fn websocket_frames_and_handshake_are_recorded_and_answered() {
    let create = fixture("ws-create-first").bytes();
    let interrupt = fixture("ws-interrupt").bytes();
    let events = fixture("ws-events-interrupt").frames();
    let upstream = CaptureUpstream::start(Script {
        ws_handshake: vec![("x-reasoning-included".into(), "true".into())],
        ws: vec![events[..1].to_vec(), events[1..].to_vec()],
        ..Script::default()
    })
    .await;

    let ws_url = upstream.base_url.replacen("http://", "ws://", 1);
    let response = wreq::Client::new()
        .websocket(format!("{ws_url}/responses"))
        .header("openai-beta", "responses_websockets=2026-02-06")
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["x-reasoning-included"], "true");
    let mut socket = response.into_websocket().await.unwrap();

    let mut received = Vec::new();
    for (frame, replies) in [(&create, 1), (&interrupt, events.len() - 1)] {
        socket
            .send(Message::text(std::str::from_utf8(frame).unwrap()))
            .await
            .unwrap();
        for _ in 0..replies {
            match tokio::time::timeout(Duration::from_secs(5), socket.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => received.push(text.as_str().to_owned()),
                other => panic!("expected a text frame, got {other:?}"),
            }
        }
    }
    let expected: Vec<String> = events.iter().map(|e| String::from_utf8(e.to_vec()).unwrap()).collect();
    assert_eq!(received, expected);

    let [recorded] = upstream.sockets().try_into().unwrap();
    assert_eq!(recorded.uri.path(), "/responses");
    assert_eq!(recorded.headers["openai-beta"], "responses_websockets=2026-02-06");
    assert_eq!(recorded.frames, vec![create, interrupt]);
}

#[tokio::test]
async fn a_refused_upgrade_is_recorded_and_answered_with_the_script() {
    let upstream = CaptureUpstream::start(Script {
        ws_reject: Some(Reply::json(426, r#"{"error":{"message":"upgrade required"}}"#)),
        ..Script::default()
    })
    .await;

    let ws_url = upstream.base_url.replacen("http://", "ws://", 1);
    let response = wreq::Client::new()
        .websocket(format!("{ws_url}/responses"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 426);
    let [recorded] = upstream.sockets().try_into().unwrap();
    assert!(recorded.frames.is_empty());
    assert!(upstream.http().is_empty());
}
