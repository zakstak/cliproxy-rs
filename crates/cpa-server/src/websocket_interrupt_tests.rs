//! `response.interrupt` acknowledgements through the whole handler, as current Codex
//! (`codex-rs/codex-api/src/endpoint/responses_websocket.rs`, e0aa043) waits for them:
//! after it interrupts a response it reads until the response's terminal event and a
//! `response.interrupt.accepted` (or `.failed`) with the same `response_id` both
//! arrived. Loopback upstreams, fake credentials, a private auth-dir.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message as AxMessage, WebSocket as AxSocket, WebSocketUpgrade};
use cpa_core::config::Config;
use cpa_exec::Executors;
use futures_util::StreamExt;
use wreq::ws::message::Message as WsMessage;

use crate::router;

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("127.0.0.1:{}", addr.port())
}

/// A WebSocket upstream answering the n-th client frame with `script[n]`; it records
/// the frames and counts dials.
async fn ws_upstream(script: Vec<Vec<&'static str>>) -> (String, Arc<Mutex<Vec<String>>>, Arc<Mutex<usize>>) {
    let frames: Arc<Mutex<Vec<String>>> = Arc::default();
    let dials: Arc<Mutex<usize>> = Arc::default();
    let script = Arc::new(Mutex::new(std::collections::VecDeque::from(script)));
    let state = (frames.clone(), dials.clone(), script);
    let url = serve(axum::Router::new().fallback(move |ws: WebSocketUpgrade| {
        let (frames, dials, script) = state.clone();
        async move {
            *dials.lock().unwrap() += 1;
            ws.on_upgrade(move |mut socket: AxSocket| async move {
                while let Some(Ok(AxMessage::Text(text))) = socket.recv().await {
                    frames.lock().unwrap().push(text.as_str().to_owned());
                    let reply = script.lock().unwrap().pop_front().unwrap_or_default();
                    for event in reply {
                        if socket.send(AxMessage::Text(event.into())).await.is_err() {
                            return;
                        }
                    }
                }
            })
        }
    }))
    .await;
    (url, frames, dials)
}

struct Proxy {
    socket: wreq::ws::WebSocket,
    /// Event types read so far, for failure messages.
    seen: Vec<String>,
    dir: std::path::PathBuf,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The proxy with one Codex API key (`websockets` as given) and a client connected.
async fn proxy(upstream: &str, websockets: bool) -> Proxy {
    proxy_with(upstream, websockets, "").await.0
}

/// [`proxy`] with `extra` appended to the config and usage statistics on; also returns
/// the runtime and the credential's ID.
async fn proxy_with(upstream: &str, websockets: bool, extra: &str) -> (Proxy, Arc<crate::Runtime>, String) {
    let dir = std::env::temp_dir().join(format!("cpa-ws-interrupt-ack-{}", uuid::Uuid::new_v4()));
    let ws = if websockets { "    websockets: true\n" } else { "" };
    let cfg = Config::parse(&format!(
        "auth-dir: {}\ncodex-api-key:\n  - api-key: sk-FAKE\n    base-url: http://{upstream}\n{ws}    models:\n      - name: gpt-fixture\n{extra}",
        dir.join("auths").display()
    ))
    .unwrap();
    let credentials = cpa_core::config::credentials::from_config(&cfg);
    let credential = credentials[0].id.clone();
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        meta: Default::default(),
        xai: Default::default(),
    };
    let rt = Arc::new(crate::testing::runtime(cfg, credentials, executors));
    rt.usage_queue().configure(
        true,
        &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
    );
    let proxy = serve(router(rt.clone())).await;
    let socket = wreq::Client::new()
        .websocket(format!("ws://{proxy}/v1/responses"))
        .send()
        .await
        .unwrap()
        .into_websocket()
        .await
        .unwrap();
    let client = Proxy {
        socket,
        seen: Vec::new(),
        dir,
    };
    (client, rt, credential)
}

impl Proxy {
    async fn send(&mut self, text: &str) {
        self.socket.send(WsMessage::text(text.to_owned())).await.unwrap();
    }

    /// The next text frame; Codex's own drain deadline is one second, so a frame later
    /// than three is as good as missing.
    async fn read(&mut self) -> String {
        let text = match tokio::time::timeout(Duration::from_secs(3), self.socket.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => text.as_str().to_owned(),
            other => panic!("no text frame after {:?}: {other:?}", self.seen),
        };
        self.seen.push(text.clone());
        text
    }

    async fn read_kind(&mut self) -> String {
        let text = self.read().await;
        cpa_common::rawjson::get(&text, "type").str().to_owned()
    }
}

const CREATE: &str = r#"{"type":"response.create","model":"gpt-fixture","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#;
const CREATE_R2: &str = r#"{"type":"response.create","model":"gpt-fixture","previous_response_id":"r1","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"next"}]}]}"#;
const INTERRUPT_R1: &str = r#"{"type":"response.interrupt","response_id":"r1","mode":"discard_partial_items"}"#;
const ACCEPTED_R1: &str = r#"{"type":"response.interrupt.accepted","response_id":"r1"}"#;
const CREATED_R1: &str = r#"{"type":"response.created","response":{"id":"r1","status":"in_progress","output":[]}}"#;
const COMPLETED_R1: &str = r#"{"type":"response.completed","response":{"id":"r1","status":"completed","output":[],"usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}}}"#;
const INTERRUPTED_R1: &str = r#"{"type":"response.incomplete","response":{"id":"r1","status":"incomplete","incomplete_details":{"reason":"interrupted"},"output":[],"usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}}}"#;
const CREATED_R2: &str = r#"{"type":"response.created","response":{"id":"r2","status":"in_progress","output":[]}}"#;
const COMPLETED_R2: &str = r#"{"type":"response.completed","response":{"id":"r2","status":"completed","output":[],"usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}}}"#;

/// The acknowledgement upstream sends after the terminal event reaches the client, and
/// the next request on the connection reuses the socket without seeing it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_upstream_acknowledgement_after_the_terminal_event_reaches_the_client() {
    let (upstream, frames, dials) = ws_upstream(vec![
        vec![CREATED_R1],
        vec![INTERRUPTED_R1, ACCEPTED_R1],
        vec![CREATED_R2, COMPLETED_R2],
    ])
    .await;
    let mut client = proxy(&upstream, true).await;
    client.send(CREATE).await;
    assert_eq!(client.read_kind().await, "response.created");
    client.send(INTERRUPT_R1).await;
    assert_eq!(client.read_kind().await, "response.incomplete");
    assert_eq!(client.read().await, ACCEPTED_R1, "the acknowledgement is unchanged");
    client.send(CREATE_R2).await;
    assert_eq!(client.read_kind().await, "response.created");
    assert_eq!(client.read_kind().await, "response.completed");
    let frames = frames.lock().unwrap().clone();
    assert_eq!(frames.len(), 3, "{frames:?}");
    assert_eq!(frames[1], INTERRUPT_R1, "the interrupt reaches upstream unchanged");
    assert_eq!(*dials.lock().unwrap(), 1, "one socket");
}

/// The interrupt arrives after the proxy finished the response: it goes upstream, and
/// upstream's acknowledgement reaches the client before the next request runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_interrupt_gets_upstreams_acknowledgement() {
    let (upstream, frames, dials) = ws_upstream(vec![
        vec![CREATED_R1, COMPLETED_R1],
        vec![ACCEPTED_R1],
        vec![CREATED_R2, COMPLETED_R2],
    ])
    .await;
    let mut client = proxy(&upstream, true).await;
    client.send(CREATE).await;
    assert_eq!(client.read_kind().await, "response.created");
    assert_eq!(client.read_kind().await, "response.completed");
    client.send(INTERRUPT_R1).await;
    assert_eq!(client.read().await, ACCEPTED_R1);
    client.send(CREATE_R2).await;
    assert_eq!(client.read_kind().await, "response.created");
    assert_eq!(client.read_kind().await, "response.completed");
    let frames = frames.lock().unwrap().clone();
    assert_eq!(frames.len(), 3, "{frames:?}");
    assert_eq!(frames[1], INTERRUPT_R1);
    assert_eq!(*dials.lock().unwrap(), 1, "one socket");
}

/// An HTTP turn has no upstream socket: the proxy cancels it and is the one that accepts
/// the interrupt, before the interrupted `response.incomplete`. An interrupt for the
/// response it just finished is accepted too instead of rejected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_http_turn_acknowledges_its_local_interrupt() {
    let calls: Arc<Mutex<usize>> = Arc::default();
    let counter = calls.clone();
    let upstream = serve(axum::Router::new().fallback(move || {
        let call = {
            let mut calls = counter.lock().unwrap();
            *calls += 1;
            *calls
        };
        async move {
            let first = concat!(
                "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\",\"status\":\"in_progress\",\"output\":[]}}\n\n",
                "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"id\":\"msg_partial\",\"content\":[{\"type\":\"output_text\",\"text\":\"partial\"}]}}\n\n",
            );
            let second = concat!(
                "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r2\",\"status\":\"in_progress\",\"output\":[]}}\n\n",
                "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r2\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":2,\"total_tokens\":5}}}\n\n",
            );
            let head = futures_util::stream::once(async move {
                Ok::<_, std::io::Error>(axum::body::Bytes::from(if call == 1 { first } else { second }))
            });
            // The first response never ends on its own.
            let body = if call == 1 {
                axum::body::Body::from_stream(head.chain(futures_util::stream::pending()))
            } else {
                axum::body::Body::from_stream(head)
            };
            ([("content-type", "text/event-stream")], body)
        }
    }))
    .await;
    let mut client = proxy(&upstream, false).await;
    client.send(CREATE).await;
    assert_eq!(client.read_kind().await, "response.created");
    assert_eq!(client.read_kind().await, "response.output_item.done");
    client.send(INTERRUPT_R1).await;
    assert_eq!(client.read().await, ACCEPTED_R1, "accepted before the terminal event");
    let incomplete = client.read().await;
    assert_eq!(
        cpa_common::rawjson::get(&incomplete, "type").str(),
        "response.incomplete",
        "{incomplete}"
    );
    assert_eq!(cpa_common::rawjson::get(&incomplete, "response.id").str(), "r1");
    client.send(INTERRUPT_R1).await;
    assert_eq!(client.read().await, ACCEPTED_R1, "the finished response's interrupt");
    // Without an upstream socket, the next turn carries the full input: a continuation
    // of `r1` would get the replay close (#28).
    client
        .send(r#"{"type":"response.create","model":"gpt-fixture","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},{"type":"message","role":"user","content":[{"type":"input_text","text":"next"}]}]}"#)
        .await;
    assert_eq!(client.read_kind().await, "response.created");
    assert_eq!(client.read_kind().await, "response.completed");
    assert_eq!(*calls.lock().unwrap(), 2);
}

/// zakstak/cliproxy-rs#7: Codex cancels a response after `response.created` and before
/// any output, and upstream ends it as `response.incomplete` with reason `interrupted`,
/// no output and `output_tokens` 0. The pinned client completes the turn on that event
/// and classifies `content_filter` and every other reason itself
/// (`codex-api/src/sse/responses.rs:451-518` at 44ccf394573c). Through the whole
/// handler, such an end reaches the client as sent, the socket and the credential serve
/// the next request, and the response's usage is recorded once. An incomplete response
/// that gives no reason stays a silent abort.
mod zero_output_ends {
    use super::*;
    use serde_json::Value;

    const INTERRUPTED: &str = r#"{"type":"response.incomplete","response":{"id":"r1","status":"incomplete","incomplete_details":{"reason":"interrupted"},"output":[],"usage":{"input_tokens":5,"output_tokens":0,"total_tokens":5}}}"#;
    const CONTENT_FILTER: &str = r#"{"type":"response.incomplete","response":{"id":"r1","status":"incomplete","incomplete_details":{"reason":"content_filter"},"output":[],"usage":{"input_tokens":5,"output_tokens":0,"total_tokens":5}}}"#;
    const MAX_OUTPUT: &str = r#"{"type":"response.incomplete","response":{"id":"r1","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[],"usage":{"input_tokens":5,"output_tokens":0,"total_tokens":5}}}"#;
    const UNEXPLAINED: &str = r#"{"type":"response.incomplete","response":{"id":"r1","status":"incomplete","output":[],"usage":{"input_tokens":5,"output_tokens":0,"total_tokens":5}}}"#;
    /// A follow-up with the full input, for a turn served over HTTP.
    const CREATE_FULL: &str = r#"{"type":"response.create","model":"gpt-fixture","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"next"}]}]}"#;
    const STEERING: &str = "codex:\n  response-steering: true\n";

    fn reason(terminal: &str) -> String {
        cpa_common::rawjson::get(terminal, "response.incomplete_details.reason")
            .str()
            .to_owned()
    }

    /// The usage records published by the time one carries the `input_tokens` 5 of the
    /// zero-output response, or the bound, plus any that follow shortly after.
    async fn records(rt: &crate::Runtime) -> Vec<Value> {
        let queue = rt.usage_queue();
        let mut got: Vec<Value> = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            while !got.iter().any(|r| r["tokens"]["input_tokens"] == 5) {
                got.extend(queue.pop_oldest(10).iter().map(|r| serde_json::from_slice(r).unwrap()));
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        got.extend(queue.pop_oldest(10).iter().map(|r| serde_json::from_slice(r).unwrap()));
        got
    }

    /// With `response-steering`, the records published while the connection is still
    /// open: [`records`] until the follow-up's record (`input_tokens` 3) is there too.
    async fn steering_records(rt: &crate::Runtime) -> Vec<Value> {
        let queue = rt.usage_queue();
        let mut got: Vec<Value> = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            while !(got.iter().any(|r| r["tokens"]["input_tokens"] == 5)
                && got.iter().any(|r| r["tokens"]["input_tokens"] == 3))
            {
                got.extend(queue.pop_oldest(10).iter().map(|r| serde_json::from_slice(r).unwrap()));
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        got.extend(queue.pop_oldest(10).iter().map(|r| serde_json::from_slice(r).unwrap()));
        got
    }

    /// Steering: the zero-output response's record ([`recorded_once`]) and the
    /// follow-up's own (`COMPLETED_R2`: 3 in, 2 out, 5 total), each once, as successes,
    /// and nothing else; each with its own execution ID.
    fn recorded_per_response(records: &[Value], case: &str) {
        recorded_once(records, case);
        let r2: Vec<&Value> = records.iter().filter(|r| r["tokens"]["input_tokens"] == 3).collect();
        assert_eq!(r2.len(), 1, "{case}: {records:?}");
        assert_eq!(r2[0]["tokens"]["output_tokens"], 2, "{case}");
        assert_eq!(r2[0]["tokens"]["total_tokens"], 5, "{case}");
        assert_eq!(records.len(), 2, "{case}: {records:?}");
        assert_ne!(records[0]["execution_id"], records[1]["execution_id"], "{case}");
    }

    /// Exactly one record carries the zero-output response's usage, as a success.
    fn recorded_once(records: &[Value], case: &str) {
        let r1: Vec<&Value> = records.iter().filter(|r| r["tokens"]["input_tokens"] == 5).collect();
        assert_eq!(r1.len(), 1, "{case}: {records:?}");
        assert_eq!(r1[0]["failed"], false, "{case}: {:?}", r1[0]);
        assert_eq!(r1[0]["tokens"]["output_tokens"], 0, "{case}");
        assert_eq!(r1[0]["tokens"]["total_tokens"], 5, "{case}");
        assert!(records.iter().all(|r| r["failed"] == false), "{case}: {records:?}");
    }

    /// Nothing else reaches the client.
    async fn quiet(client: &mut Proxy, case: &str) {
        if let Ok(frame) = tokio::time::timeout(Duration::from_millis(300), client.socket.next()).await {
            panic!("{case}: stale frame {frame:?}");
        }
    }

    /// The silent abort: the connection ends without the incomplete event, the attempt is
    /// recorded once as a 502, and the request-scoped failure cools nothing.
    async fn aborted(client: &mut Proxy, rt: &crate::Runtime, credential: &str, case: &str) {
        loop {
            match tokio::time::timeout(Duration::from_secs(3), client.socket.next()).await {
                Ok(Some(Ok(WsMessage::Text(text)))) => {
                    assert!(!text.as_str().contains("response.incomplete"), "{case}: {text:?}");
                }
                Ok(Some(Ok(WsMessage::Close(_)) | Err(_)) | None) => break,
                Ok(Some(Ok(_))) => {}
                Err(_) => panic!("{case}: the connection stayed open"),
            }
        }
        let queue = rt.usage_queue();
        let mut got: Vec<Value> = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            while got.is_empty() {
                got.extend(queue.pop_oldest(10).iter().map(|r| serde_json::from_slice(r).unwrap()));
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        got.extend(queue.pop_oldest(10).iter().map(|r| serde_json::from_slice(r).unwrap()));
        assert_eq!(got.len(), 1, "{case}: {got:?}");
        assert_eq!(got[0]["failed"], true, "{case}");
        assert_eq!(got[0]["fail"]["status_code"], 502, "{case}");
        assert!(rt.store().cooldowns(credential).is_empty(), "{case}: request-scoped");
    }

    /// Canceled before the first delta, acknowledged before or after the terminal event,
    /// with and without `response-steering`: both events arrive as upstream sent them, the
    /// next create runs on the same socket and sees only its own response, and the
    /// credential does not cool. A turn records the interrupted response's usage (zero
    /// output) once. With response steering one stream carries the whole connection, and
    /// each response still gets its own record (#25) while the connection is open: the
    /// interrupted one with its zero output, and the follow-up with its usage.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_interrupted_end_keeps_the_socket_the_credential_and_its_usage() {
        for steering in [false, true] {
            for ends in [[INTERRUPTED, ACCEPTED_R1], [ACCEPTED_R1, INTERRUPTED]] {
                let case = format!(
                    "steering={steering} ends={:?}",
                    ends.map(|e| cpa_common::rawjson::get(e, "type").str().to_owned())
                );
                let (upstream, frames, dials) =
                    ws_upstream(vec![vec![CREATED_R1], ends.to_vec(), vec![CREATED_R2, COMPLETED_R2]]).await;
                let (mut client, rt, credential) =
                    proxy_with(&upstream, true, if steering { STEERING } else { "" }).await;
                client.send(CREATE).await;
                assert_eq!(client.read().await, CREATED_R1, "{case}");
                client.send(INTERRUPT_R1).await;
                assert_eq!(
                    [client.read().await, client.read().await],
                    ends,
                    "{case}: as upstream sent them"
                );
                client.send(CREATE_R2).await;
                assert_eq!(
                    [client.read().await, client.read().await],
                    [CREATED_R2, COMPLETED_R2],
                    "{case}: the follow-up"
                );
                quiet(&mut client, &case).await;
                assert_eq!(*frames.lock().unwrap(), [CREATE, INTERRUPT_R1, CREATE_R2], "{case}");
                assert_eq!(*dials.lock().unwrap(), 1, "{case}: one socket");
                assert!(rt.store().cooldowns(&credential).is_empty(), "{case}");
                if steering {
                    recorded_per_response(&steering_records(&rt).await, &case);
                } else {
                    recorded_once(&records(&rt).await, &case);
                }
            }
        }
    }

    /// `content_filter` and `max_output_tokens` with zero output are the provider's
    /// answer: they reach the client unchanged, with their own reason, and the socket and
    /// the credential stay. An unexplained end sent before any event is delivered is
    /// an upstream silent abort: the client never gets it, the attempt is a request-scoped
    /// 502 that cools nothing, and the socket is gone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn named_reasons_stay_distinct_and_an_unexplained_end_stays_an_abort() {
        for steering in [false, true] {
            for terminal in [CONTENT_FILTER, MAX_OUTPUT] {
                let case = format!("steering={steering} reason={}", reason(terminal));
                let (upstream, frames, dials) =
                    ws_upstream(vec![vec![CREATED_R1, terminal], vec![CREATED_R2, COMPLETED_R2]]).await;
                let (mut client, rt, credential) =
                    proxy_with(&upstream, true, if steering { STEERING } else { "" }).await;
                client.send(CREATE).await;
                assert_eq!(
                    [client.read().await, client.read().await],
                    [CREATED_R1, terminal],
                    "{case}"
                );
                client.send(CREATE_R2).await;
                assert_eq!(
                    [client.read().await, client.read().await],
                    [CREATED_R2, COMPLETED_R2],
                    "{case}"
                );
                assert_eq!(frames.lock().unwrap().len(), 2, "{case}");
                assert_eq!(*dials.lock().unwrap(), 1, "{case}: one socket");
                assert!(rt.store().cooldowns(&credential).is_empty(), "{case}");
                if steering {
                    recorded_per_response(&steering_records(&rt).await, &case);
                } else {
                    recorded_once(&records(&rt).await, &case);
                }
            }
        }
        let (upstream, frames, _) = ws_upstream(vec![vec![UNEXPLAINED]]).await;
        let (mut client, rt, credential) = proxy_with(&upstream, true, "").await;
        client.send(CREATE).await;
        aborted(&mut client, &rt, &credential, "no reason before delivery").await;
        assert_eq!(frames.lock().unwrap().len(), 1);
    }

    /// A WebSocket turn served over HTTP (a credential without `websockets`), with
    /// bootstrap buffering holding `response.created` so the end arrives before anything
    /// was delivered: a zero-output end that names its reason reaches the client unchanged
    /// and the next request runs; without a reason it is the 502.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_turn_over_http_keeps_named_zero_output_ends() {
        for terminal in [INTERRUPTED, CONTENT_FILTER, MAX_OUTPUT, UNEXPLAINED] {
            let case = format!("reason={}", reason(terminal));
            let calls: Arc<Mutex<usize>> = Arc::default();
            let counter = calls.clone();
            let upstream = serve(axum::Router::new().fallback(move || {
                let call = {
                    let mut calls = counter.lock().unwrap();
                    *calls += 1;
                    *calls
                };
                async move {
                    let (created, end) = if call == 1 {
                        (CREATED_R1, terminal)
                    } else {
                        (CREATED_R2, COMPLETED_R2)
                    };
                    let kind = cpa_common::rawjson::get(end, "type").str().to_owned();
                    let body = format!("event: response.created\ndata: {created}\n\nevent: {kind}\ndata: {end}\n\n");
                    ([("content-type", "text/event-stream")], body)
                }
            }))
            .await;
            let (mut client, rt, credential) =
                proxy_with(&upstream, false, "codex:\n  stream-bootstrap-buffering: true\n").await;
            client.send(CREATE).await;
            if terminal == UNEXPLAINED {
                aborted(&mut client, &rt, &credential, &case).await;
                assert_eq!(*calls.lock().unwrap(), 1, "{case}");
                continue;
            }
            assert_eq!(
                [client.read().await, client.read().await],
                [CREATED_R1, terminal],
                "{case}"
            );
            client.send(CREATE_FULL).await;
            assert_eq!(
                [client.read().await, client.read().await],
                [CREATED_R2, COMPLETED_R2],
                "{case}"
            );
            assert_eq!(*calls.lock().unwrap(), 2, "{case}");
            assert!(rt.store().cooldowns(&credential).is_empty(), "{case}");
            recorded_once(&records(&rt).await, &case);
        }
    }
}

/// zakstak/cliproxy-rs#25 and #7: with `codex.response-steering`, one upstream socket and
/// one stream carry every response of the connection, and each response still gets a
/// usage record of its own, published as soon as its outcome is observed while the
/// connection stays open. Every case delivers an unknown upstream event (`BARRIER`)
/// after the events it checks: once the client has it, the proxy processed and delivered
/// everything before it, so no close can race the processing it checks.
mod steering_usage {
    use super::*;
    use serde_json::Value;

    const STEERING: &str = "codex:\n  response-steering: true\n";
    const END1: &str = r#"{"type":"response.completed","response":{"id":"r1","status":"completed","output":[],"usage":{"input_tokens":5,"output_tokens":3,"total_tokens":8}}}"#;
    const END2: &str = r#"{"type":"response.completed","response":{"id":"r2","status":"completed","output":[],"usage":{"input_tokens":11,"output_tokens":7,"total_tokens":18}}}"#;
    const INTERRUPTED: &str = r#"{"type":"response.incomplete","response":{"id":"r1","status":"incomplete","incomplete_details":{"reason":"interrupted"},"output":[],"usage":{"input_tokens":5,"output_tokens":0,"total_tokens":5}}}"#;
    /// An event the proxy does not know; it reaches the client as sent.
    const BARRIER: &str = r#"{"type":"test.delivery_barrier","after":"checked"}"#;

    /// What the upstream does with its socket once told to end it.
    #[derive(Clone, Copy, Debug)]
    enum Then {
        /// Keeps it until the test ends.
        Hold,
        /// Sends a close frame.
        Close,
        /// Drops it without a close frame.
        Drop,
    }

    struct Upstream {
        url: String,
        frames: Arc<Mutex<Vec<String>>>,
        dials: Arc<Mutex<usize>>,
        end: Arc<tokio::sync::Notify>,
        push: tokio::sync::mpsc::UnboundedSender<Vec<String>>,
    }

    impl Upstream {
        fn frames(&self) -> Vec<String> {
            self.frames.lock().unwrap().clone()
        }
    }

    /// A WebSocket upstream answering the n-th client frame with `script[n]`, sending
    /// what the test pushes, and ending its socket as `then` says when told to.
    async fn upstream(script: Vec<Vec<&'static str>>, then: Then) -> Upstream {
        let frames: Arc<Mutex<Vec<String>>> = Arc::default();
        let dials: Arc<Mutex<usize>> = Arc::default();
        let end = Arc::new(tokio::sync::Notify::new());
        let (push, pushed) = tokio::sync::mpsc::unbounded_channel::<Vec<String>>();
        let pushed = Arc::new(Mutex::new(Some(pushed)));
        let script = Arc::new(Mutex::new(std::collections::VecDeque::from(script)));
        let state = (frames.clone(), dials.clone(), end.clone(), pushed, script);
        let url = serve(axum::Router::new().fallback(move |ws: WebSocketUpgrade| {
            let (frames, dials, end, pushed, script) = state.clone();
            async move {
                *dials.lock().unwrap() += 1;
                ws.on_upgrade(move |mut socket: AxSocket| async move {
                    let mut pushed = pushed.lock().unwrap().take();
                    loop {
                        let reply: Vec<String> = tokio::select! {
                            message = socket.recv() => match message {
                                Some(Ok(AxMessage::Text(text))) => {
                                    frames.lock().unwrap().push(text.as_str().to_owned());
                                    let reply = script.lock().unwrap().pop_front().unwrap_or_default();
                                    reply.into_iter().map(str::to_owned).collect()
                                }
                                Some(Ok(_)) => continue,
                                _ => return,
                            },
                            Some(events) = async {
                                match pushed.as_mut() {
                                    Some(rx) => rx.recv().await,
                                    None => std::future::pending().await,
                                }
                            } => events,
                            () = end.notified() => {
                                match then {
                                    Then::Close => {
                                        let _ = socket.send(AxMessage::Close(None)).await;
                                    }
                                    Then::Drop => {}
                                    Then::Hold => continue,
                                }
                                return;
                            }
                        };
                        for event in reply {
                            if socket.send(AxMessage::Text(event.into())).await.is_err() {
                                return;
                            }
                        }
                    }
                })
            }
        }))
        .await;
        Upstream {
            url,
            frames,
            dials,
            end,
            push,
        }
    }

    fn pop(rt: &crate::Runtime) -> Vec<Value> {
        rt.usage_queue()
            .pop_oldest(50)
            .iter()
            .map(|r| serde_json::from_slice(r).unwrap())
            .collect()
    }

    /// The records published until `n` arrived (at most five seconds), plus any that
    /// follow within 500 ms: a record too many shows.
    async fn records(rt: &crate::Runtime, n: usize) -> Vec<Value> {
        let mut got = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            while got.len() < n {
                got.extend(pop(rt));
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        got.extend(pop(rt));
        got
    }

    fn tokens(record: &Value) -> [i64; 3] {
        let t = &record["tokens"];
        [
            t["input_tokens"].as_i64().unwrap(),
            t["output_tokens"].as_i64().unwrap(),
            t["total_tokens"].as_i64().unwrap(),
        ]
    }

    /// A success with these tokens.
    fn success(record: &Value, want: [i64; 3], case: &str) {
        assert_eq!(record["failed"], false, "{case}: {record}");
        assert_eq!(record["fail"]["status_code"], 200, "{case}: {record}");
        assert_eq!(tokens(record), want, "{case}: {record}");
    }

    /// A failure with this status and no tokens.
    fn failure(record: &Value, status: i64, case: &str) {
        assert_eq!(record["failed"], true, "{case}: {record}");
        assert_eq!(record["fail"]["status_code"], status, "{case}: {record}");
        assert_eq!(tokens(record), [0, 0, 0], "{case}: {record}");
    }

    /// Every record belongs to the one credential and model, each with its own execution
    /// ID. With steering they also share the request ID of the turn whose dispatch
    /// carries the whole stream (and whose trace captures it); without it, each turn is
    /// its own dispatch.
    fn same_credential(records: &[Value], case: &str) {
        same_connection_fields(
            records,
            &["source", "auth_index", "provider", "executor_type", "model", "endpoint"],
            case,
        );
    }

    /// [`same_credential`], and one request ID.
    fn same_connection(records: &[Value], case: &str) {
        assert!(!records[0]["request_id"].as_str().unwrap().is_empty(), "{case}");
        same_connection_fields(
            records,
            &[
                "request_id",
                "source",
                "auth_index",
                "provider",
                "executor_type",
                "model",
                "endpoint",
            ],
            case,
        );
    }

    fn same_connection_fields(records: &[Value], fields: &[&str], case: &str) {
        let first = &records[0];
        let mut ids = std::collections::HashSet::new();
        for r in records {
            for &field in fields {
                assert_eq!(r[field], first[field], "{case}: {field} of {r}");
            }
            assert_eq!(r["provider"], "codex", "{case}");
            assert_eq!(r["model"], "gpt-fixture", "{case}");
            assert!(
                ids.insert(r["execution_id"].as_str().unwrap().to_owned()),
                "{case}: {records:?}"
            );
        }
    }

    /// How the connection ends after the checked records.
    #[derive(Clone, Copy, Debug)]
    enum End {
        /// The client sends a close frame.
        Graceful,
        /// The client drops its connection.
        Abrupt,
        /// The upstream ends its socket (`Then::Close` or `Then::Drop`).
        Upstream,
    }

    async fn end(mut client: Proxy, how: End, upstream: &Upstream) {
        match how {
            End::Graceful => {
                client.socket.send(WsMessage::Close(None)).await.unwrap();
                closed(&mut client).await;
            }
            End::Abrupt => drop(client),
            End::Upstream => {
                upstream.end.notify_one();
                closed(&mut client).await;
            }
        }
    }

    /// The proxy ends the client's connection.
    async fn closed(client: &mut Proxy) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match client.socket.next().await {
                    Some(Ok(WsMessage::Close(_)) | Err(_)) | None => break,
                    Some(Ok(_)) => {}
                }
            }
        })
        .await
        .expect("the proxy closes the connection");
    }

    /// Reads exactly these frames, as upstream sent them.
    async fn expect(client: &mut Proxy, frames: &[&str], case: &str) {
        for frame in frames {
            assert_eq!(client.read().await, *frame, "{case}");
        }
    }

    /// Two completed responses on one steering socket: two records with their own usage,
    /// published while the connection stays open. However the connection then ends, no
    /// record follows and neither changes; a request-scoped end cools nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn each_completed_response_is_recorded_while_the_connection_stays_open() {
        for (how, then) in [
            (End::Graceful, Then::Hold),
            (End::Abrupt, Then::Hold),
            (End::Upstream, Then::Close),
            (End::Upstream, Then::Drop),
        ] {
            let case = format!("{how:?} {then:?}");
            let up = upstream(vec![vec![CREATED_R1, END1], vec![CREATED_R2, END2, BARRIER]], then).await;
            let (mut client, rt, credential) = proxy_with(&up.url, true, STEERING).await;
            client.send(CREATE).await;
            expect(&mut client, &[CREATED_R1, END1], &case).await;
            client.send(CREATE_R2).await;
            expect(&mut client, &[CREATED_R2, END2, BARRIER], &case).await;
            let open = records(&rt, 2).await;
            assert_eq!(open.len(), 2, "{case}: {open:?}");
            success(&open[0], [5, 3, 8], &case);
            success(&open[1], [11, 7, 18], &case);
            same_connection(&open, &case);
            end(client, how, &up).await;
            let after = records(&rt, 0).await;
            assert!(after.is_empty(), "{case}: {after:?}");
            assert_eq!(up.frames(), [CREATE, CREATE_R2], "{case}");
            assert_eq!(*up.dials.lock().unwrap(), 1, "{case}");
            assert!(rt.store().cooldowns(&credential).is_empty(), "{case}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn successor_records_keep_the_client_alias_before_model_routing() {
        let up = upstream(
            vec![vec![CREATED_R1, END1], vec![CREATED_R2, END2, BARRIER]],
            Then::Hold,
        )
        .await;
        let config = format!("        alias: gpt-alias\n{STEERING}");
        let (mut client, rt, credential) = proxy_with(&up.url, true, &config).await;
        let first = CREATE.replacen("gpt-fixture", "gpt-alias", 1);
        let second = CREATE_R2.replacen(
            r#""model":"gpt-fixture""#,
            r#""model":"gpt-alias","service_tier":"flex","reasoning":{"effort":"high"}"#,
            1,
        );
        client.send(&first).await;
        expect(&mut client, &[CREATED_R1, END1], "alias first").await;
        client.send(&second).await;
        expect(&mut client, &[CREATED_R2, END2, BARRIER], "alias successor").await;
        let open = records(&rt, 2).await;
        assert_eq!(open.len(), 2, "{open:?}");
        same_connection(&open, "alias");
        for record in &open {
            assert_eq!(record["alias"], "gpt-alias", "{record}");
        }
        assert_eq!(open[1]["reasoning_effort"], "high");
        assert_eq!(open[1]["service_tier"], "flex");
        assert_eq!(
            up.frames(),
            [CREATE.to_owned(), second.replacen("gpt-alias", "gpt-fixture", 1)],
            "only the model value is routed"
        );
        end(client, End::Graceful, &up).await;
        assert!(records(&rt, 0).await.is_empty());
        assert_eq!(*up.dials.lock().unwrap(), 1);
        assert!(rt.store().cooldowns(&credential).is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn settled_steering_records_keep_the_time_of_the_first_payload() {
        let up = upstream(vec![vec![], vec![]], Then::Hold).await;
        let (mut client, rt, _) = proxy_with(&up.url, true, STEERING).await;
        for (index, create, created, terminal) in [(1, CREATE, CREATED_R1, END1), (2, CREATE_R2, CREATED_R2, END2)] {
            client.send(create).await;
            tokio::time::timeout(Duration::from_secs(5), async {
                while up.frames().len() < index {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .expect("the request reached upstream");
            // Delay both the first payload and the terminal separately. TTFT must
            // include the first wait, without becoming the terminal's latency.
            tokio::time::sleep(Duration::from_millis(40)).await;
            up.push.send(vec![created.to_owned()]).unwrap();
            expect(&mut client, &[created], "first payload").await;
            tokio::time::sleep(Duration::from_millis(40)).await;
            up.push.send(vec![terminal.to_owned(), BARRIER.to_owned()]).unwrap();
            expect(&mut client, &[terminal, BARRIER], "terminal").await;
            let got = records(&rt, 1).await;
            assert_eq!(got.len(), 1, "{got:?}");
            let ttft = got[0]["ttft_ms"].as_i64().unwrap();
            let latency = got[0]["latency_ms"].as_i64().unwrap();
            assert!(ttft >= 35, "response {index}: {got:?}");
            assert!(latency - ttft >= 35, "response {index}: {got:?}");
        }
        end(client, End::Graceful, &up).await;
        assert!(records(&rt, 0).await.is_empty());
    }

    /// An interrupted zero-output response and its successor, acknowledged before or
    /// after the terminal event: the provider's interrupted end is its own success with
    /// its reported usage, not a cancellation, and the successor has its own record.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_interrupted_response_and_its_successor_keep_their_own_usage() {
        for ends in [[INTERRUPTED, ACCEPTED_R1], [ACCEPTED_R1, INTERRUPTED]] {
            let case = format!(
                "{:?}",
                ends.map(|e| cpa_common::rawjson::get(e, "type").str().to_owned())
            );
            let up = upstream(
                vec![vec![CREATED_R1], ends.to_vec(), vec![CREATED_R2, END2, BARRIER]],
                Then::Drop,
            )
            .await;
            let (mut client, rt, credential) = proxy_with(&up.url, true, STEERING).await;
            client.send(CREATE).await;
            expect(&mut client, &[CREATED_R1], &case).await;
            client.send(INTERRUPT_R1).await;
            expect(&mut client, &ends, &case).await;
            client.send(CREATE_R2).await;
            expect(&mut client, &[CREATED_R2, END2, BARRIER], &case).await;
            let open = records(&rt, 2).await;
            assert_eq!(open.len(), 2, "{case}: {open:?}");
            success(&open[0], [5, 0, 5], &case);
            success(&open[1], [11, 7, 18], &case);
            same_connection(&open, &case);
            end(client, End::Upstream, &up).await;
            let after = records(&rt, 0).await;
            assert!(after.is_empty(), "{case}: {after:?}");
            assert_eq!(up.frames(), [CREATE, INTERRUPT_R1, CREATE_R2], "{case}");
            assert_eq!(*up.dials.lock().unwrap(), 1, "{case}");
            assert!(rt.store().cooldowns(&credential).is_empty(), "{case}");
        }
    }

    /// A response still running when the connection ends is the only record that
    /// follows: the client leaving cancels it (the proxy's 499 accounting convention, no
    /// tokens), an upstream loss fails it (502), and the completed response before it
    /// keeps its record.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn only_the_running_response_settles_with_the_connection() {
        for (how, then, status) in [
            (End::Graceful, Then::Hold, 499),
            (End::Abrupt, Then::Hold, 499),
            (End::Upstream, Then::Close, 502),
            (End::Upstream, Then::Drop, 502),
        ] {
            let case = format!("{how:?} {then:?}");
            let up = upstream(vec![vec![CREATED_R1, END1], vec![CREATED_R2, BARRIER]], then).await;
            let (mut client, rt, credential) = proxy_with(&up.url, true, STEERING).await;
            client.send(CREATE).await;
            expect(&mut client, &[CREATED_R1, END1], &case).await;
            client.send(CREATE_R2).await;
            expect(&mut client, &[CREATED_R2, BARRIER], &case).await;
            let open = records(&rt, 1).await;
            assert_eq!(open.len(), 1, "{case}: {open:?}");
            success(&open[0], [5, 3, 8], &case);
            end(client, how, &up).await;
            let after = records(&rt, 1).await;
            assert_eq!(after.len(), 1, "{case}: {after:?}");
            failure(&after[0], status, &case);
            if status == 499 {
                assert_eq!(after[0]["fail"]["body"], "context canceled", "{case}");
            }
            same_connection(&[open[0].clone(), after[0].clone()], &case);
            assert!(rt.store().cooldowns(&credential).is_empty(), "{case}");
        }
    }

    /// A create sent upstream that never started settles with the connection as the
    /// running response does. A create upstream rejects before it starts is its own
    /// failed record at once; the rejection reaches the client as sent and the socket
    /// serves the next create.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn creates_that_never_start_are_failures_of_their_own() {
        for (how, then, status) in [(End::Graceful, Then::Hold, 499), (End::Upstream, Then::Close, 502)] {
            let case = format!("unstarted {how:?}");
            let up = upstream(vec![vec![CREATED_R1, END1], vec![BARRIER]], then).await;
            let (mut client, rt, _) = proxy_with(&up.url, true, STEERING).await;
            client.send(CREATE).await;
            expect(&mut client, &[CREATED_R1, END1], &case).await;
            client.send(CREATE_R2).await;
            expect(&mut client, &[BARRIER], &case).await;
            let open = records(&rt, 1).await;
            assert_eq!(open.len(), 1, "{case}: {open:?}");
            success(&open[0], [5, 3, 8], &case);
            end(client, how, &up).await;
            let after = records(&rt, 1).await;
            assert_eq!(after.len(), 1, "{case}: {after:?}");
            failure(&after[0], status, &case);
        }

        const REJECTED: &str =
            r#"{"type":"error","status":400,"error":{"type":"invalid_request_error","message":"rejected"}}"#;
        const CREATE_R3: &str = r#"{"type":"response.create","model":"gpt-fixture","previous_response_id":"r1","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"third"}]}]}"#;
        const CREATED_R3: &str =
            r#"{"type":"response.created","response":{"id":"r3","status":"in_progress","output":[]}}"#;
        const END3: &str = r#"{"type":"response.completed","response":{"id":"r3","status":"completed","output":[],"usage":{"input_tokens":2,"output_tokens":1,"total_tokens":3}}}"#;
        let case = "rejected";
        let up = upstream(
            vec![vec![CREATED_R1, END1], vec![REJECTED], vec![CREATED_R3, END3, BARRIER]],
            Then::Hold,
        )
        .await;
        let (mut client, rt, credential) = proxy_with(&up.url, true, STEERING).await;
        client.send(CREATE).await;
        expect(&mut client, &[CREATED_R1, END1], case).await;
        client.send(CREATE_R2).await;
        expect(&mut client, &[REJECTED], case).await;
        client.send(CREATE_R3).await;
        expect(&mut client, &[CREATED_R3, END3, BARRIER], case).await;
        let open = records(&rt, 3).await;
        assert_eq!(open.len(), 3, "{case}: {open:?}");
        success(&open[0], [5, 3, 8], case);
        failure(&open[1], 400, case);
        success(&open[2], [2, 1, 3], case);
        same_connection(&open, case);
        assert_eq!(up.frames(), [CREATE, CREATE_R2, CREATE_R3], "{case}");
        assert_eq!(*up.dials.lock().unwrap(), 1, "{case}");
        assert!(rt.store().cooldowns(&credential).is_empty(), "{case}");
        end(client, End::Graceful, &up).await;
        let after = records(&rt, 0).await;
        assert!(after.is_empty(), "{case}: {after:?}");
    }

    /// `r1` and `r2` run and a third create waits upstream when a failure arrives. One
    /// that names `r1` (as `response.id` or as `response_id`) is `r1`'s record, not the
    /// waiting create's, and `r2` runs on; the waiting create later starts as the
    /// explicit request it is, with its own facts. A failure naming a response that
    /// already settled adds no record and takes nothing from the waiting create. One
    /// naming a response the socket never saw start is still that create's rejection
    /// while its started-response history is complete.
    /// Every event reaches the client as sent; nothing more settles at the close.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failure_is_the_record_of_the_response_it_names() {
        const CREATE_AUTO: &str =
            r#"{"type":"response.create","model":"gpt-fixture","service_tier":"auto","input":[]}"#;
        const CREATE_FLEX: &str =
            r#"{"type":"response.create","model":"gpt-fixture","service_tier":"flex","input":[]}"#;
        const CREATE_PRIORITY: &str =
            r#"{"type":"response.create","model":"gpt-fixture","service_tier":"priority","input":[]}"#;
        const FAILED_R1: &str = r#"{"type":"response.failed","response":{"id":"r1","status":"failed","output":[],"error":{"type":"invalid_request_error","message":"r1 failed"}}}"#;
        const ERROR_R1: &str = r#"{"type":"error","status":400,"response_id":"r1","error":{"type":"invalid_request_error","message":"r1 rejected"}}"#;
        const UNSTARTED: &str = r#"{"type":"error","status":400,"response_id":"never-started","error":{"type":"invalid_request_error","message":"rejected before it started"}}"#;
        const CREATED_R3: &str =
            r#"{"type":"response.created","response":{"id":"r3","status":"in_progress","output":[]}}"#;
        const END3: &str = r#"{"type":"response.completed","response":{"id":"r3","status":"completed","output":[],"usage":{"input_tokens":2,"output_tokens":1,"total_tokens":3}}}"#;
        for (case, settled_first, failed) in [
            ("older open response.id", false, FAILED_R1),
            ("older open response_id", false, ERROR_R1),
            ("settled", true, FAILED_R1),
            ("never started", false, UNSTARTED),
        ] {
            let first: Vec<&'static str> = if settled_first {
                vec![CREATED_R1, END1]
            } else {
                vec![CREATED_R1]
            };
            let up = upstream(vec![first.clone(), vec![CREATED_R2], vec![failed, BARRIER]], Then::Hold).await;
            let (mut client, rt, credential) = proxy_with(&up.url, true, STEERING).await;
            client.send(CREATE_AUTO).await;
            expect(&mut client, &first, case).await;
            client.send(CREATE_FLEX).await;
            expect(&mut client, &[CREATED_R2], case).await;
            client.send(CREATE_PRIORITY).await;
            expect(&mut client, &[failed, BARRIER], case).await;
            let tier = |r: &Value| r["service_tier"].as_str().unwrap().to_owned();
            let open = records(&rt, 1).await;
            assert_eq!(open.len(), 1, "{case}: {open:?}");
            match case {
                "settled" => success(&open[0], [5, 3, 8], case),
                _ => failure(&open[0], 400, case),
            }
            let failed_tier = if case == "never started" { "priority" } else { "auto" };
            assert_eq!(
                tier(&open[0]),
                if settled_first { "auto" } else { failed_tier },
                "{case}"
            );
            // The rest of the conversation: r2 ends, then r3 starts and ends unless the
            // failure rejected its create; then r1 ends if it still runs.
            let mut rest = vec![END2];
            if case != "never started" {
                rest.extend([CREATED_R3, END3]);
            } else {
                rest.push(END1);
            }
            rest.push(BARRIER);
            up.push.send(rest.iter().map(|s| (*s).to_owned()).collect()).unwrap();
            expect(&mut client, &rest, case).await;
            let later = records(&rt, 2).await;
            assert_eq!(later.len(), 2, "{case}: {later:?}");
            success(&later[0], [11, 7, 18], case);
            assert_eq!(tier(&later[0]), "flex", "{case}");
            if case == "never started" {
                success(&later[1], [5, 3, 8], case);
                assert_eq!(tier(&later[1]), "auto", "{case}");
            } else {
                success(&later[1], [2, 1, 3], case);
                assert_eq!(tier(&later[1]), "priority", "{case}: r3 keeps its create's facts");
            }
            let mut all = open.clone();
            all.extend(later);
            same_connection(&all, case);
            assert_eq!(up.frames(), [CREATE_AUTO, CREATE_FLEX, CREATE_PRIORITY], "{case}");
            end(client, End::Graceful, &up).await;
            let after = records(&rt, 0).await;
            assert!(after.is_empty(), "{case}: {after:?}");
            assert_eq!(*up.dials.lock().unwrap(), 1, "{case}");
            assert!(rt.store().cooldowns(&credential).is_empty(), "{case}");
        }
    }

    /// Once history has expired, an unmatched failure ID cannot prove that the pending
    /// create was rejected. Preserve the frame and fail the socket as ambiguous, instead
    /// of charging that create the old response's outcome. A retained settled ID still
    /// leaves the pending create and its facts intact.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn expired_history_cannot_charge_a_pending_create_an_old_failure() {
        const PRIORITY: &str =
            r#"{"type":"response.create","model":"gpt-fixture","service_tier":"priority","input":[]}"#;
        const CREATED_PENDING: &str =
            r#"{"type":"response.created","response":{"id":"pending","status":"in_progress","output":[]}}"#;
        const END_PENDING: &str = r#"{"type":"response.completed","response":{"id":"pending","status":"completed","output":[],"usage":{"input_tokens":11,"output_tokens":7,"total_tokens":18}}}"#;
        for (failed_id, ambiguous) in [("r0", true), ("never-seen", true), ("r16", false)] {
            let up = upstream(vec![], Then::Hold).await;
            let (mut client, rt, credential) = proxy_with(&up.url, true, STEERING).await;
            for index in 0..17 {
                client.send(CREATE).await;
                tokio::time::timeout(Duration::from_secs(5), async {
                    while up.frames().len() <= index {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                })
                .await
                .expect("the create reached upstream");
                let created = format!(
                    r#"{{"type":"response.created","response":{{"id":"r{index}","status":"in_progress","output":[]}}}}"#
                );
                let terminal = END1.replacen("r1", &format!("r{index}"), 1);
                up.push
                    .send(vec![created.clone(), terminal.clone(), BARRIER.to_owned()])
                    .unwrap();
                expect(&mut client, &[&created, &terminal, BARRIER], failed_id).await;
            }
            let completed = pop(&rt);
            assert_eq!(completed.len(), 17, "{failed_id}: {completed:?}");
            for record in &completed {
                success(record, [5, 3, 8], failed_id);
            }
            client.send(PRIORITY).await;
            tokio::time::timeout(Duration::from_secs(5), async {
                while up.frames().len() < 18 {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .expect("the pending create reached upstream");
            let failed = format!(
                r#"{{"type":"error","status":400,"response_id":"{failed_id}","error":{{"message":"late failure"}}}}"#
            );
            let mut events = vec![failed.clone()];
            if !ambiguous {
                events.push(BARRIER.to_owned());
            }
            up.push.send(events).unwrap();
            assert_eq!(client.read().await, failed, "upstream bytes are preserved");
            if ambiguous {
                let got = records(&rt, 1).await;
                assert_eq!(got.len(), 1, "{failed_id}: {got:?}");
                failure(&got[0], 502, failed_id);
                assert_eq!(got[0]["service_tier"], "priority");
                assert!(
                    got[0]["fail"]["body"]
                        .as_str()
                        .unwrap()
                        .contains("cannot associate websocket failure"),
                    "{got:?}"
                );
                closed(&mut client).await;
            } else {
                assert_eq!(client.read().await, BARRIER);
                assert!(pop(&rt).is_empty(), "a settled response produces no new record");
                up.push
                    .send([CREATED_PENDING, END_PENDING, BARRIER].map(str::to_owned).to_vec())
                    .unwrap();
                expect(&mut client, &[CREATED_PENDING, END_PENDING, BARRIER], failed_id).await;
                let got = records(&rt, 1).await;
                assert_eq!(got.len(), 1, "{got:?}");
                success(&got[0], [11, 7, 18], failed_id);
                assert_eq!(got[0]["service_tier"], "priority");
                end(client, End::Graceful, &up).await;
            }
            assert!(records(&rt, 0).await.is_empty(), "no extra settlement");
            assert_eq!(up.frames().len(), 18);
            assert_eq!(up.frames().last().unwrap(), PRIORITY);
            assert_eq!(*up.dials.lock().unwrap(), 1);
            assert!(rt.store().cooldowns(&credential).is_empty());
        }
    }

    /// Evicting an earlier occurrence of an ID must keep a newer retained occurrence's
    /// settings. Its late failure is still a known settled response's, and cannot close
    /// the socket or abort the unrelated pending request.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reused_response_keeps_its_settings_when_the_older_start_is_evicted() {
        const PRIORITY: &str = r#"{"type":"response.create","model":"gpt-fixture","service_tier":"priority","reasoning":{"effort":"high"},"input":[]}"#;
        const LATE_ERROR: &str =
            r#"{"type":"error","status":400,"response_id":"r0","error":{"message":"late failure"}}"#;
        let up = upstream(vec![], Then::Hold).await;
        let (mut client, rt, credential) = proxy_with(&up.url, true, STEERING).await;
        let mut ids = (0..16).map(|i| format!("r{i}")).collect::<Vec<_>>();
        ids.extend(["r0".to_owned(), "r17".to_owned()]);
        for (index, id) in ids.iter().enumerate() {
            client.send(CREATE).await;
            tokio::time::timeout(Duration::from_secs(5), async {
                while up.frames().len() <= index {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .expect("the create reached upstream");
            let created = format!(
                r#"{{"type":"response.created","response":{{"id":"{id}","status":"in_progress","output":[]}}}}"#
            );
            let terminal = END1.replacen("r1", id, 1);
            up.push
                .send(vec![created.clone(), terminal.clone(), BARRIER.to_owned()])
                .unwrap();
            expect(&mut client, &[&created, &terminal, BARRIER], id).await;
        }
        let completed = pop(&rt);
        assert_eq!(completed.len(), 18, "{completed:?}");
        same_connection(&completed, "reused ID");
        for record in &completed {
            success(record, [5, 3, 8], "reused ID");
        }
        client.send(PRIORITY).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while up.frames().len() < 19 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the pending create reached upstream");
        up.push.send([LATE_ERROR, BARRIER].map(str::to_owned).to_vec()).unwrap();
        expect(&mut client, &[LATE_ERROR, BARRIER], "late retained error").await;
        assert!(pop(&rt).is_empty(), "a known settled response adds no record");
        up.push
            .send([CREATED_R2, END2, BARRIER].map(str::to_owned).to_vec())
            .unwrap();
        expect(&mut client, &[CREATED_R2, END2, BARRIER], "pending create").await;
        let got = records(&rt, 1).await;
        assert_eq!(got.len(), 1, "{got:?}");
        success(&got[0], [11, 7, 18], "pending create");
        assert_eq!(got[0]["service_tier"], "priority");
        assert_eq!(got[0]["reasoning_effort"], "high");
        end(client, End::Graceful, &up).await;
        assert!(records(&rt, 0).await.is_empty());
        assert_eq!(up.frames().len(), 19);
        assert_eq!(up.frames().last().unwrap(), PRIORITY);
        assert_eq!(*up.dials.lock().unwrap(), 1);
        assert!(rt.store().cooldowns(&credential).is_empty());
    }

    /// Frames the proxy answers itself never go upstream and get no record.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn frames_refused_locally_get_no_record() {
        const APPEND: &str = r#"{"type":"response.append","input":[]}"#;
        const UNKNOWN: &str = r#"{"type":"response.unknown_request"}"#;
        let case = "refused";
        let up = upstream(
            vec![vec![CREATED_R1, END1], vec![CREATED_R2, END2, BARRIER]],
            Then::Hold,
        )
        .await;
        let (mut client, rt, _) = proxy_with(&up.url, true, STEERING).await;
        client.send(CREATE).await;
        expect(&mut client, &[CREATED_R1, END1], case).await;
        for refused in [APPEND, UNKNOWN] {
            client.send(refused).await;
            let reply = client.read().await;
            assert_eq!(cpa_common::rawjson::get(&reply, "type").str(), "error", "{reply}");
        }
        client.send(CREATE_R2).await;
        expect(&mut client, &[CREATED_R2, END2, BARRIER], case).await;
        let open = records(&rt, 2).await;
        assert_eq!(open.len(), 2, "{case}: {open:?}");
        success(&open[0], [5, 3, 8], case);
        success(&open[1], [11, 7, 18], case);
        assert_eq!(up.frames(), [CREATE, CREATE_R2], "{case}");
    }

    /// Steering: upstream accepts a steer of `r1`, ends `r1` and starts an automatic
    /// successor on its own. A create sent meanwhile waits in the proxy and goes upstream
    /// once the successor ended. Each of the three responses has its own record: the
    /// successor with the facts of the request whose settings it continues (`r1`'s), the
    /// queued create with its own (reasoning effort and service tier). A create still
    /// queued when the client leaves never went upstream and gets no record; the
    /// response still running is canceled.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_automatic_successor_and_a_queued_create_get_their_own_records() {
        const STEER: &str = r#"{"type":"response.steer","previous_response_id":"r1","input":"more"}"#;
        const STEER_ACCEPTED: &str =
            r#"{"type":"response.steer.accepted","steer":{"id":"s1","previous_response_id":"r1"}}"#;
        const CREATED_AUTO: &str = r#"{"type":"response.created","response":{"id":"auto","previous_response_id":"r1","status":"in_progress","output":[]}}"#;
        const END_AUTO: &str = r#"{"type":"response.completed","response":{"id":"auto","previous_response_id":"r1","status":"completed","output":[],"usage":{"input_tokens":7,"output_tokens":2,"total_tokens":9}}}"#;
        const CREATE_QUEUED: &str = r#"{"type":"response.create","model":"gpt-fixture","previous_response_id":"auto","service_tier":"flex","reasoning":{"effort":"high"},"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"next"}]}]}"#;
        /// Answered by the proxy itself: its reply proves the frame before it was read.
        const PROBE: &str = r#"{"type":"response.unknown_request"}"#;
        for leave_early in [false, true] {
            let case = format!("leave_early={leave_early}");
            let up = upstream(
                vec![vec![CREATED_R1], vec![STEER_ACCEPTED], vec![CREATED_R2, END2, BARRIER]],
                Then::Hold,
            )
            .await;
            let (mut client, rt, _) = proxy_with(&up.url, true, STEERING).await;
            client.send(CREATE).await;
            expect(&mut client, &[CREATED_R1], &case).await;
            client.send(STEER).await;
            expect(&mut client, &[STEER_ACCEPTED], &case).await;
            client.send(CREATE_QUEUED).await;
            client.send(PROBE).await;
            let reply = client.read().await;
            assert_eq!(
                cpa_common::rawjson::get(&reply, "type").str(),
                "error",
                "{case}: {reply}"
            );
            assert_eq!(up.frames(), [CREATE, STEER], "{case}: the create waits");
            if leave_early {
                end(client, End::Graceful, &up).await;
                let after = records(&rt, 1).await;
                assert_eq!(after.len(), 1, "{case}: {after:?}");
                failure(&after[0], 499, &case);
                assert_eq!(up.frames(), [CREATE, STEER], "{case}: never sent");
                continue;
            }
            up.push
                .send([END1, CREATED_AUTO, END_AUTO].map(str::to_owned).to_vec())
                .unwrap();
            expect(
                &mut client,
                &[END1, CREATED_AUTO, END_AUTO, CREATED_R2, END2, BARRIER],
                &case,
            )
            .await;
            let open = records(&rt, 3).await;
            assert_eq!(open.len(), 3, "{case}: {open:?}");
            success(&open[0], [5, 3, 8], &case);
            success(&open[1], [7, 2, 9], &case);
            success(&open[2], [11, 7, 18], &case);
            same_connection(&open, &case);
            for field in ["reasoning_effort", "service_tier", "alias"] {
                assert_eq!(open[1][field], open[0][field], "{case}: successor {field}");
            }
            assert_eq!(open[0]["service_tier"], "auto", "{case}");
            assert_eq!(open[2]["service_tier"], "flex", "{case}");
            assert_eq!(open[2]["reasoning_effort"], "high", "{case}");
            assert_ne!(open[0]["reasoning_effort"], "high", "{case}");
            assert_eq!(up.frames(), [CREATE, STEER, CREATE_QUEUED], "{case}");
            end(client, End::Graceful, &up).await;
            let after = records(&rt, 0).await;
            assert!(after.is_empty(), "{case}: {after:?}");
        }
    }

    /// A terminal event without usage stays without it: the record has no tokens and no
    /// usage flag, exactly as a turn without steering records it, and the event reaches
    /// the client unchanged.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_response_without_usage_is_recorded_as_a_turn_records_it() {
        const BARE_END2: &str =
            r#"{"type":"response.completed","response":{"id":"r2","status":"completed","output":[]}}"#;
        let mut shapes = Vec::new();
        for steering in [false, true] {
            let case = format!("steering={steering}");
            let mut r2 = vec![CREATED_R2, BARE_END2];
            if steering {
                r2.push(BARRIER);
            }
            let up = upstream(vec![vec![CREATED_R1, END1], r2.clone()], Then::Hold).await;
            let (mut client, rt, _) = proxy_with(&up.url, true, if steering { STEERING } else { "" }).await;
            client.send(CREATE).await;
            expect(&mut client, &[CREATED_R1, END1], &case).await;
            client.send(CREATE_R2).await;
            expect(&mut client, &r2, &case).await;
            let open = records(&rt, 2).await;
            assert_eq!(open.len(), 2, "{case}: {open:?}");
            success(&open[0], [5, 3, 8], &case);
            success(&open[1], [0, 0, 0], &case);
            assert!(open[1].get("usage_observed").is_none(), "{case}: {}", open[1]);
            shapes.push((open[1]["tokens"].clone(), open[1]["token_breakdown"].clone()));
        }
        assert_eq!(shapes[0], shapes[1], "the same record shape with and without steering");
    }

    /// A duplicate terminal event is not a second record, and a later response that
    /// reuses an ID gets its own record instead of merging into the earlier one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_duplicate_terminal_is_one_record_and_a_reused_id_is_another() {
        const REUSED_END: &str = r#"{"type":"response.completed","response":{"id":"r1","status":"completed","output":[],"usage":{"input_tokens":11,"output_tokens":7,"total_tokens":18}}}"#;
        let case = "duplicates";
        let up = upstream(
            vec![vec![CREATED_R1, END1, END1], vec![CREATED_R1, REUSED_END, BARRIER]],
            Then::Hold,
        )
        .await;
        let (mut client, rt, _) = proxy_with(&up.url, true, STEERING).await;
        client.send(CREATE).await;
        expect(&mut client, &[CREATED_R1, END1, END1], case).await;
        client.send(CREATE_R2).await;
        expect(&mut client, &[CREATED_R1, REUSED_END, BARRIER], case).await;
        let open = records(&rt, 2).await;
        assert_eq!(open.len(), 2, "{case}: {open:?}");
        success(&open[0], [5, 3, 8], case);
        success(&open[1], [11, 7, 18], case);
        same_connection(&open, case);
    }

    /// The control without steering: each turn is its own attempt and record, published
    /// while the connection stays open, and none follows when it closes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn turns_without_steering_are_recorded_per_turn() {
        let case = "no steering";
        let up = upstream(vec![vec![CREATED_R1, END1], vec![CREATED_R2, END2]], Then::Close).await;
        let (mut client, rt, credential) = proxy_with(&up.url, true, "").await;
        client.send(CREATE).await;
        expect(&mut client, &[CREATED_R1, END1], case).await;
        client.send(CREATE_R2).await;
        expect(&mut client, &[CREATED_R2, END2], case).await;
        let open = records(&rt, 2).await;
        assert_eq!(open.len(), 2, "{case}: {open:?}");
        success(&open[0], [5, 3, 8], case);
        success(&open[1], [11, 7, 18], case);
        same_credential(&open, case);
        assert_ne!(
            open[0]["request_id"], open[1]["request_id"],
            "{case}: a dispatch per turn"
        );
        end(client, End::Upstream, &up).await;
        let after = records(&rt, 0).await;
        assert!(after.is_empty(), "{case}: {after:?}");
        assert!(rt.store().cooldowns(&credential).is_empty(), "{case}");
    }
}
