//! The outcome a translated Codex stream settles in the attempt's record when its
//! consumer drops it (#54): which steps end the upstream, and which queued items keep the
//! outcome of the step they came from. The router tests
//! (`cpa-server/tests/codex_translated_usage_cancel.rs`) cover the same through the
//! server; these reach the orders a socket cannot pin: a drop between two polls while
//! held steps or translated events are still queued.

use super::*;
use cpa_core::exec::{Caller, UsageSink};

/// The failures the stream settled in the attempt's record, as (status, text).
#[derive(Default)]
struct Settled(std::sync::Mutex<Vec<(u16, String)>>);

impl cpa_core::exec::UsageObserver for Settled {
    fn response_body(&self, _: Format, _: &[u8]) {}
    fn response_line(&self, _: Format, _: &[u8]) {}
    fn request(&self, _: Format, _: &[u8]) {}
    fn publish_failure(&self, status: u16, body: &str) {
        self.0.lock().unwrap().push((status, body.to_owned()));
    }
    fn publish_failure_with_usage(&self, status: u16, body: &str) {
        self.0.lock().unwrap().push((status, format!("with usage: {body}")));
    }
    fn publish(&self) {
        self.0.lock().unwrap().push((0, "publish".into()));
    }
}

impl Settled {
    fn get(&self) -> Vec<(u16, String)> {
        self.0.lock().unwrap().clone()
    }
}

const CREATED: &str = r#"{"type":"response.created","response":{"id":"resp_1","object":"response","model":"gpt-5.5","status":"in_progress","output":[]}}"#;
const DELTA: &str =
    r#"{"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"hi"}"#;
const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"resp_1","object":"response","model":"gpt-5.5","status":"completed","output":[],"usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}}}"#;

/// One upstream event, processed as the translated route does.
fn step(payload: &str) -> Step {
    Processor::new(false, false).event(format!("data: {payload}\n").as_bytes())
}

fn failure() -> ExecError {
    response::request_scoped(502, "upstream broke")
}

/// What the stream records for `error` if the consumer leaves before taking it.
/// An upstream failure's record, which keeps the usage reported before it (U1, #99).
fn recorded(error: &ExecError) -> (u16, String) {
    (
        error.status,
        format!("with usage: {}", String::from_utf8_lossy(&error.body)),
    )
}

/// `rest` as the upstream after the held steps: these steps, then an upstream that stays
/// open.
fn open(steps: Vec<Step>) -> Option<Steps> {
    Some(
        futures_util::stream::iter(steps)
            .chain(futures_util::stream::pending())
            .boxed(),
    )
}

/// A Chat Completions client's stream over `held` then `rest`, reporting to the returned
/// record.
fn chat_stream(held: Vec<Step>, rest: Option<Steps>) -> (ExecStream, std::sync::Arc<Settled>) {
    let settled = std::sync::Arc::new(Settled::default());
    let body = Bytes::from_static(br#"{"model":"gpt-5.5","messages":[{"role":"user","content":"hi"}],"stream":true}"#);
    let req = ExecRequest {
        operation: Operation::Generate,
        source_format: Format::OpenAI,
        response_format: Format::OpenAI,
        requested_model: "gpt-5.5".into(),
        model: "gpt-5.5".into(),
        original_body: body.clone(),
        body,
        stream: true,
        alt: None,
        session: None,
        headers: HeaderMap::new(),
        execution_session: None,
        derived_session: None,
        resolved_model: None,
        usage: UsageSink::new(settled.clone()),
        request_path: "/v1/chat/completions".into(),
        caller: Caller {
            principal: String::new(),
            source: "authorization",
            provider: String::new(),
        },
    };
    let translated = r#"{"model":"gpt-5.5","stream":true,"input":[]}"#;
    (client_stream(&req, translated, held.into(), rest), settled)
}

/// A consumer that leaves before the upstream ended cancels the attempt, once: while the
/// stream awaits the upstream, or between two polls with held steps still queued (the
/// bootstrap handed them over, but they did not end the upstream).
#[tokio::test]
async fn an_unfinished_stream_dropped_records_one_cancellation() {
    use futures_util::FutureExt;
    for awaiting in [true, false] {
        let (mut stream, record) = chat_stream(vec![step(CREATED), step(DELTA)], open(vec![]));
        assert!(stream.next().await.unwrap().is_ok());
        if awaiting {
            while let Some(item) = stream.next().now_or_never() {
                assert!(item.unwrap().is_ok());
            }
        }
        assert!(record.get().is_empty());
        drop(stream);
        assert_eq!(
            record.get(),
            [(499, "context canceled".to_owned())],
            "awaiting={awaiting}"
        );
    }
}

/// The terminal event ends the upstream once the stream observes it, held by the bootstrap
/// or read after it: a consumer that leaves with its translated events still queued
/// leaves the record to the tracker, which has the observed usage.
#[tokio::test]
async fn an_observed_terminal_event_is_not_a_cancellation() {
    let cases = [
        (vec![step(CREATED), step(DELTA), step(COMPLETED)], None),
        (vec![step(CREATED)], open(vec![step(DELTA), step(COMPLETED)])),
    ];
    for (n, (held, rest)) in cases.into_iter().enumerate() {
        let (mut stream, record) = chat_stream(held, rest);
        let mut events = 0;
        loop {
            let event = stream.next().await.unwrap().unwrap();
            events += 1;
            if String::from_utf8_lossy(&event).contains("finish_reason\":\"stop") {
                break;
            }
        }
        assert!(events > 1, "case {n}");
        drop(stream);
        assert!(record.get().is_empty(), "case {n}: {:?}", record.get());
    }
}

/// An upstream that ended with no event (the bootstrap saw a clean, empty body) is not a
/// cancellation: the server records the empty stream itself.
#[tokio::test]
async fn an_empty_upstream_is_not_a_cancellation() {
    let (stream, record) = chat_stream(vec![], None);
    drop(stream);
    assert!(record.get().is_empty(), "{:?}", record.get());
}

/// An observed upstream failure is the record until the consumer takes it: queued in the
/// held steps, or read after them with the translated lines that preceded it in its event
/// still queued. Once the consumer takes it, the consumer settles it.
#[tokio::test]
async fn an_observed_upstream_failure_is_never_a_cancellation() {
    let fail = |before: Option<&str>| Step::Fail {
        error: failure(),
        body: None,
        before: before.map(|b| Bytes::from(format!("data: {b}\n\n"))),
    };
    let expected = recorded(&failure());
    assert_ne!(expected.0, 499);

    // Held: the bootstrap observed it.
    let (stream, record) = chat_stream(vec![step(CREATED), fail(None)], None);
    drop(stream);
    assert_eq!(record.get(), std::slice::from_ref(&expected));

    // Read after the held steps, with its event's earlier line still queued.
    let (mut stream, record) = chat_stream(vec![step(CREATED)], open(vec![fail(Some(DELTA))]));
    loop {
        let event = stream.next().await.unwrap().unwrap();
        if String::from_utf8_lossy(&event).contains("\"hi\"") {
            break;
        }
    }
    drop(stream);
    assert_eq!(record.get(), std::slice::from_ref(&expected));

    // Taken by the consumer.
    let (mut stream, record) = chat_stream(vec![step(CREATED)], open(vec![fail(Some(DELTA))]));
    let mut taken = None;
    while let Some(item) = stream.next().await {
        if let Err(error) = item {
            taken = Some(error);
            break;
        }
    }
    assert_eq!(recorded(&taken.expect("the stream ends with the failure")), expected);
    drop(stream);
    assert!(record.get().is_empty(), "the consumer settles what it took");
}
