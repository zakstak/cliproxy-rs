//! Interrupt acknowledgements on the Codex upstream socket, as current Codex
//! (`codex-rs/codex-api/src/endpoint/responses_websocket.rs`, e0aa043) expects them: after
//! it sends `response.interrupt` for a response, it reads until both the response's
//! terminal event and a `response.interrupt.accepted` or `.failed` carrying the same
//! `response_id` arrived, in either order. Scripted loopback upstreams, fake credentials.

use super::*;

const CREATED_R2: &str = r#"{"type":"response.created","response":{"id":"r2","output":[]}}"#;
const COMPLETED_R2: &str = r#"{"type":"response.completed","response":{"id":"r2","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#;
const INTERRUPTED_R1: &str = r#"{"type":"response.incomplete","response":{"id":"r1","status":"incomplete","incomplete_details":{"reason":"interrupted"},"output":[],"usage":{"input_tokens":1,"output_tokens":3,"total_tokens":4}}}"#;
const ACCEPTED_R1: &str = r#"{"type":"response.interrupt.accepted","response_id":"r1","sequence_number":3}"#;
const FAILED_R1: &str =
    r#"{"type":"response.interrupt.failed","response_id":"r1","error":{"code":"too_late","message":"already done"}}"#;
const ACCEPTED_OTHER: &str = r#"{"type":"response.interrupt.accepted","response_id":"other"}"#;

/// A turn on session "conn-1"; the continuation reuses the pooled socket.
async fn start(executor: &CodexExecutor, url: &str, continuation: bool) -> cpa_core::exec::ExecStream {
    let response = executor
        .execute_in_session(
            &credential(url),
            request(BODY),
            &Config::default(),
            &session(continuation),
        )
        .await
        .unwrap();
    let ResponseBody::Stream(stream) = response.body else {
        panic!("websocket turns stream");
    };
    stream
}

async fn next(stream: &mut cpa_core::exec::ExecStream) -> String {
    match tokio::time::timeout(Duration::from_secs(5), stream.next()).await {
        Ok(Some(Ok(chunk))) => String::from_utf8_lossy(&chunk).into_owned(),
        Ok(Some(Err(error))) => panic!("turn failed: {error}"),
        Ok(None) => panic!("turn ended early"),
        Err(_) => panic!("turn stalled"),
    }
}

/// Everything the turn still yields; a turn that never ends fails the test.
async fn rest_of(stream: cpa_core::exec::ExecStream) -> Vec<Result<String, ExecError>> {
    tokio::time::timeout(
        Duration::from_secs(5),
        stream
            .map(|item| item.map(|b| String::from_utf8_lossy(&b).into_owned()))
            .collect::<Vec<_>>(),
    )
    .await
    .expect("the turn never ended")
}

fn kinds(events: &[Result<String, ExecError>]) -> Vec<String> {
    events
        .iter()
        .map(|e| match e {
            Ok(text) => crate::rawjson::get(text, "type").str().to_owned(),
            Err(error) => format!("error {}: {}", error.status, String::from_utf8_lossy(&error.body)),
        })
        .collect()
}

fn texts(events: &[Result<String, ExecError>]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|e| e.as_ref().ok())
        .map(String::as_str)
        .collect()
}

/// The follow-up turn on the same socket sees only its own response.
async fn follow_up_is_clean(executor: &CodexExecutor, url: &str, up: &Upstream) {
    let events = rest_of(start(executor, url, true).await).await;
    assert_eq!(
        kinds(&events),
        ["response.created", "response.completed"],
        "follow-up: {events:?}"
    );
    assert_eq!(crate::rawjson::get(texts(&events)[0], "response.id").str(), "r2");
    assert_eq!(up.dials.lock().unwrap().len(), 1, "the socket was reused");
}

/// What upstream received after the turn's `response.create`.
async fn after_create(up: &Upstream) -> Vec<String> {
    let frames = received(up, 2).await;
    assert_eq!(
        crate::rawjson::get(&frames[0], "type").str(),
        "response.create",
        "{frames:?}"
    );
    frames[1..].to_vec()
}

async fn interrupt_r1(executor: &CodexExecutor) -> String {
    let outcome = executor
        .interrupt_session("conn-1", INTERRUPT_R1.as_bytes(), |_| true)
        .await;
    assert!(outcome.is_ok(), "{outcome:?}");
    format!("{outcome:?}")
}

/// The acknowledgement Codex waits for arrives after the terminal event: the turn keeps
/// reading until it has passed it on, byte for byte, then the socket serves the next turn.
#[tokio::test]
async fn an_acknowledgement_after_the_terminal_event_reaches_the_turn() {
    let (up, url) = upstream(vec![
        Act::Send(vec![CREATED]),
        Act::Send(vec![INTERRUPTED_R1, ACCEPTED_R1]),
        Act::Send(vec![CREATED_R2, COMPLETED_R2]),
    ])
    .await;
    let executor = CodexExecutor::new().unwrap();
    let mut stream = start(&executor, &url, false).await;
    assert_eq!(
        crate::rawjson::get(&next(&mut stream).await, "type").str(),
        "response.created"
    );
    interrupt_r1(&executor).await;
    let events = rest_of(stream).await;
    assert_eq!(
        kinds(&events),
        ["response.incomplete", "response.interrupt.accepted"],
        "{events:?}"
    );
    assert_eq!(texts(&events)[1], ACCEPTED_R1, "the acknowledgement is unchanged");
    assert_eq!(after_create(&up).await, [INTERRUPT_R1]);
    follow_up_is_clean(&executor, &url, &up).await;
}

/// The acknowledgement before the terminal event ends nothing: the terminal still does.
#[tokio::test]
async fn an_acknowledgement_before_the_terminal_event_passes_through() {
    let (up, url) = upstream(vec![
        Act::Send(vec![CREATED]),
        Act::Send(vec![ACCEPTED_R1, INTERRUPTED_R1]),
        Act::Send(vec![CREATED_R2, COMPLETED_R2]),
    ])
    .await;
    let executor = CodexExecutor::new().unwrap();
    let mut stream = start(&executor, &url, false).await;
    next(&mut stream).await;
    interrupt_r1(&executor).await;
    let events = rest_of(stream).await;
    assert_eq!(
        kinds(&events),
        ["response.interrupt.accepted", "response.incomplete"],
        "{events:?}"
    );
    assert_eq!(texts(&events)[0], ACCEPTED_R1);
    follow_up_is_clean(&executor, &url, &up).await;
}

/// A rejected interrupt is upstream's answer for the client, before or after the
/// terminal event; it is not a proxy failure and the socket stays usable.
#[tokio::test]
async fn a_rejected_interrupt_reaches_the_client_and_keeps_the_socket() {
    for (order, script) in [
        ("before", vec![FAILED_R1, COMPLETED]),
        ("after", vec![COMPLETED, FAILED_R1]),
    ] {
        let (up, url) = upstream(vec![
            Act::Send(vec![CREATED]),
            Act::Send(script),
            Act::Send(vec![CREATED_R2, COMPLETED_R2]),
        ])
        .await;
        let executor = CodexExecutor::new().unwrap();
        let mut stream = start(&executor, &url, false).await;
        next(&mut stream).await;
        interrupt_r1(&executor).await;
        let events = rest_of(stream).await;
        let mut want = vec!["response.interrupt.failed", "response.completed"];
        if order == "after" {
            want.reverse();
        }
        assert_eq!(kinds(&events), want, "{order}: {events:?}");
        assert!(texts(&events).contains(&FAILED_R1), "{order}: rejection changed");
        follow_up_is_clean(&executor, &url, &up).await;
    }
}

/// Only the acknowledgement for the interrupted response reaches the turn: Codex fails a
/// turn on an acknowledgement for any other response ID.
#[tokio::test]
async fn an_acknowledgement_for_another_response_is_dropped() {
    let (up, url) = upstream(vec![
        Act::Send(vec![CREATED]),
        Act::Send(vec![ACCEPTED_OTHER, COMPLETED, ACCEPTED_R1]),
        Act::Send(vec![CREATED_R2, COMPLETED_R2]),
    ])
    .await;
    let executor = CodexExecutor::new().unwrap();
    let mut stream = start(&executor, &url, false).await;
    next(&mut stream).await;
    interrupt_r1(&executor).await;
    let events = rest_of(stream).await;
    assert_eq!(
        kinds(&events),
        ["response.completed", "response.interrupt.accepted"],
        "{events:?}"
    );
    assert_eq!(texts(&events)[1], ACCEPTED_R1);
    follow_up_is_clean(&executor, &url, &up).await;
}

/// An acknowledgement that never comes holds the turn only for the bounded wait; when it
/// shows up during the next turn, that turn never sees it.
#[tokio::test]
async fn a_late_acknowledgement_never_reaches_the_next_turn() {
    let (up, url) = upstream(vec![
        Act::Send(vec![CREATED]),
        Act::Send(vec![INTERRUPTED_R1]),
        Act::Send(vec![ACCEPTED_R1, CREATED_R2, COMPLETED_R2]),
    ])
    .await;
    let executor = CodexExecutor::new().unwrap();
    let mut stream = start(&executor, &url, false).await;
    next(&mut stream).await;
    interrupt_r1(&executor).await;
    let started = Instant::now();
    let events = rest_of(stream).await;
    assert_eq!(kinds(&events), ["response.incomplete"], "{events:?}");
    assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    follow_up_is_clean(&executor, &url, &up).await;
}

/// The socket fails while the turn waits for the acknowledgement: the response already
/// ended, so the turn ends without an error and the session learns of the loss.
#[tokio::test]
async fn a_socket_lost_while_waiting_for_the_acknowledgement_ends_the_turn_cleanly() {
    let (_up, url) = upstream(vec![
        Act::Send(vec![CREATED]),
        Act::SendClose(vec![INTERRUPTED_R1], 1011),
    ])
    .await;
    let executor = CodexExecutor::new().unwrap();
    let mut stream = start(&executor, &url, false).await;
    next(&mut stream).await;
    interrupt_r1(&executor).await;
    let events = rest_of(stream).await;
    assert_eq!(kinds(&events), ["response.incomplete"], "{events:?}");
    tokio::time::timeout(Duration::from_secs(5), executor.ws.closed("conn-1"))
        .await
        .expect("the loss was not reported");
}

/// The interrupt arrives after the proxy read the terminal event and released the turn
/// (Codex had not read it yet). It goes upstream, and upstream's acknowledgement comes
/// back to the caller to deliver; the next turn is not affected.
#[tokio::test]
async fn an_interrupt_after_the_terminal_event_is_forwarded_and_acknowledged() {
    let (up, url) = upstream(vec![
        Act::Send(vec![CREATED, COMPLETED]),
        Act::Send(vec![ACCEPTED_R1]),
        Act::Send(vec![CREATED_R2, COMPLETED_R2]),
    ])
    .await;
    let executor = CodexExecutor::new().unwrap();
    let events = rest_of(start(&executor, &url, false).await).await;
    assert_eq!(kinds(&events), ["response.created", "response.completed"]);
    let outcome = interrupt_r1(&executor).await;
    assert_eq!(after_create(&up).await, [INTERRUPT_R1]);
    assert!(
        outcome.contains("response.interrupt.accepted"),
        "the acknowledgement comes back: {outcome}"
    );
    follow_up_is_clean(&executor, &url, &up).await;
}

/// Upstream never acknowledges the late interrupt: the call returns within the bounded
/// wait without an acknowledgement, and the socket stays usable.
#[tokio::test]
async fn an_unacknowledged_late_interrupt_returns_within_the_bound() {
    let (up, url) = upstream(vec![
        Act::Send(vec![CREATED, COMPLETED]),
        Act::Send(vec![]),
        Act::Send(vec![CREATED_R2, COMPLETED_R2]),
    ])
    .await;
    let executor = CodexExecutor::new().unwrap();
    rest_of(start(&executor, &url, false).await).await;
    let started = Instant::now();
    let outcome = interrupt_r1(&executor).await;
    assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    assert!(!outcome.contains("response.interrupt"), "{outcome}");
    assert_eq!(after_create(&up).await, [INTERRUPT_R1]);
    follow_up_is_clean(&executor, &url, &up).await;
}

/// The socket closes while a late interrupt waits: the caller gets the loss.
#[tokio::test]
async fn a_late_interrupt_on_a_closing_socket_is_rejected() {
    let (_up, url) = upstream(vec![Act::Send(vec![CREATED, COMPLETED]), Act::SendClose(vec![], 1011)]).await;
    let executor = CodexExecutor::new().unwrap();
    rest_of(start(&executor, &url, false).await).await;
    let outcome = executor
        .interrupt_session("conn-1", INTERRUPT_R1.as_bytes(), |_| true)
        .await;
    assert!(
        matches!(&outcome, Err(InterruptError::Rejected(text)) if text.contains("1011")),
        "{outcome:?}"
    );
}

/// Bootstrap buffering ends at a terminal event inside its window; a pending
/// acknowledgement still follows it.
#[tokio::test]
async fn a_buffered_turn_waits_for_the_acknowledgement_too() {
    let (up, url) = upstream(vec![
        Act::Send(vec![CREATED]),
        Act::Send(vec![INTERRUPTED_R1, ACCEPTED_R1]),
    ])
    .await;
    let executor = Arc::new(CodexExecutor::new().unwrap());
    let cfg = Config::parse("codex:\n  stream-bootstrap-buffering: true\n").unwrap();
    let turn = {
        let (executor, url) = (executor.clone(), url.clone());
        tokio::spawn(async move {
            executor
                .execute_in_session(&credential(&url), request(BODY), &cfg, &session(false))
                .await
        })
    };
    // `response.created` is held, so the turn has not returned when the interrupt goes.
    received(&up, 1).await;
    interrupt_r1(&executor).await;
    let response = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the buffered turn never returned")
        .unwrap()
        .unwrap();
    let ResponseBody::Stream(stream) = response.body else {
        panic!("websocket turns stream");
    };
    let events = rest_of(stream).await;
    assert_eq!(
        kinds(&events),
        ["response.created", "response.incomplete", "response.interrupt.accepted"],
        "{events:?}"
    );
}

const ZERO_INTERRUPTED_R1: &str = r#"{"type":"response.incomplete","response":{"id":"r1","status":"incomplete","incomplete_details":{"reason":"interrupted"},"output":[],"usage":{"input_tokens":5,"output_tokens":0,"total_tokens":5}}}"#;

/// Canceled after `response.created`, before the first delta: upstream ends the response
/// as interrupted with zero output tokens. That is the response Codex asked to stop, not
/// a silent abort: it reaches the client with its ID and usage, the acknowledgement still
/// follows, and the socket serves the next turn.
#[tokio::test]
async fn a_zero_token_interrupted_response_is_a_normal_end() {
    let (up, url) = upstream(vec![
        Act::Send(vec![CREATED]),
        Act::Send(vec![ZERO_INTERRUPTED_R1, ACCEPTED_R1]),
        Act::Send(vec![CREATED_R2, COMPLETED_R2]),
    ])
    .await;
    let executor = CodexExecutor::new().unwrap();
    let mut stream = start(&executor, &url, false).await;
    next(&mut stream).await;
    interrupt_r1(&executor).await;
    let events = rest_of(stream).await;
    assert_eq!(
        kinds(&events),
        ["response.incomplete", "response.interrupt.accepted"],
        "{events:?}"
    );
    let incomplete = texts(&events)[0];
    assert_eq!(crate::rawjson::get(incomplete, "response.id").str(), "r1");
    assert_eq!(crate::rawjson::get(incomplete, "response.usage.input_tokens").i64(), 5);
    assert_eq!(
        crate::rawjson::get(incomplete, "response.incomplete_details.reason").str(),
        "interrupted"
    );
    assert!(executor.ws.loss("conn-1").is_none(), "the socket was not dropped");
    follow_up_is_clean(&executor, &url, &up).await;
}

/// The same shape without a reason is still an upstream abort: a request-scoped 502.
#[tokio::test]
async fn an_unexplained_zero_output_incomplete_still_fails() {
    let (_up, url) = upstream(vec![Act::Send(vec![
        CREATED,
        r#"{"type":"response.incomplete","response":{"id":"r1","output":[],"usage":{"output_tokens":0}}}"#,
    ])])
    .await;
    let executor = CodexExecutor::new().unwrap();
    let events = rest_of(start(&executor, &url, false).await).await;
    let error = events
        .last()
        .and_then(|e| e.as_ref().err())
        .unwrap_or_else(|| panic!("{events:?}"));
    assert_eq!(error.status, 502, "{error}");
    assert_eq!(error.scope, FailureScope::Request);
}
