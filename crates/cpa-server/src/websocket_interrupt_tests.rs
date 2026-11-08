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
    /// output) once. A response-steering stream settles one record for the whole
    /// connection when it ends, so its records are not per response and are not checked.
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
                if !steering {
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
                if !steering {
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
