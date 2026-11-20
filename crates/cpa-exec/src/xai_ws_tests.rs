//! Local xAI WebSocket session contracts. The historical corpus contains only
//! client requests and scripted peer stimuli; Rust owns expected state transitions,
//! identities, compaction, errors, translation semantics, usage and capture.

use super::*;
use std::sync::Mutex as StdMutex;

use axum::extract::ws::{CloseFrame, Message as AxMessage, WebSocket as AxSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, State};
use axum::response::{IntoResponse, Response};
use cpa_core::exec::{Caller, Operation};
use serde_json::Value;

use crate::usage_testkit::{Captured, published, sinks};

const FIXTURE: &str = include_str!("../tests/fixtures/xai_ws_inputs.json");
const COMPARED_HEADERS: [&str; 4] = ["Authorization", "Content-Type", "X-Grok-Conv-Id", "X-Trace"];

#[derive(Default)]
struct Recorded {
    upgrades: Vec<String>,
    frames: Vec<String>,
    http: Vec<String>,
    pongs: Vec<String>,
}

#[derive(Default)]
struct Script {
    turn: Option<Value>,
    recorded: Recorded,
}

type Shared = Arc<StdMutex<Script>>;

fn canonical(name: &str) -> String {
    name.split('-')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + &chars.as_str().to_ascii_lowercase(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("-")
}

async fn handler(State(shared): State<Shared>, req: axum::extract::Request) -> Response {
    let (mut parts, body) = req.into_parts();
    let uri = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_default();
    if parts.headers.contains_key("upgrade") {
        let mut lines: Vec<String> = COMPARED_HEADERS
            .iter()
            .filter_map(|name| {
                parts
                    .headers
                    .get(name.to_ascii_lowercase())
                    .map(|v| format!("{}: {}", canonical(name), v.to_str().unwrap()))
            })
            .collect();
        lines.sort();
        let reject = {
            let mut script = shared.lock().unwrap();
            script.recorded.upgrades.push(format!("{uri}\n{}", lines.join("\n")));
            script
                .turn
                .as_ref()
                .and_then(|t| t.get("reject"))
                .filter(|r| !r.is_null())
                .cloned()
        };
        if let Some(reject) = reject {
            let status = http::StatusCode::from_u16(reject["status"].as_u64().unwrap() as u16).unwrap();
            return (
                status,
                [("content-type", "application/json")],
                reject["body"].as_str().unwrap().to_owned(),
            )
                .into_response();
        }
        let upgrade = WebSocketUpgrade::from_request_parts(&mut parts, &()).await.unwrap();
        return upgrade.on_upgrade(move |socket| serve_socket(socket, shared));
    }
    let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    let compact = {
        let mut script = shared.lock().unwrap();
        script
            .recorded
            .http
            .push(format!("{} {uri}\n{}", parts.method, String::from_utf8_lossy(&body)));
        script
            .turn
            .as_ref()
            .and_then(|t| t.get("compact"))
            .filter(|r| !r.is_null())
            .cloned()
    };
    match compact {
        Some(reply) => (
            http::StatusCode::from_u16(reply["status"].as_u64().unwrap() as u16).unwrap(),
            [("content-type", "application/json")],
            reply["body"].as_str().unwrap().to_owned(),
        )
            .into_response(),
        None => http::StatusCode::from_u16(599).unwrap().into_response(),
    }
}

async fn serve_socket(mut socket: AxSocket, shared: Shared) {
    while let Some(Ok(message)) = socket.recv().await {
        let AxMessage::Text(text) = message else { continue };
        let previous = crate::rawjson::get(text.as_str(), "previous_response_id")
            .str()
            .to_owned();
        let act = {
            let mut script = shared.lock().unwrap();
            script.recorded.frames.push(text.as_str().to_owned());
            let n = script.recorded.frames.len();
            script
                .turn
                .as_ref()
                .and_then(|t| t["acts"].as_array())
                .and_then(|acts| acts.get(n - 1))
                .cloned()
        };
        let Some(act) = act else { continue };
        if let Some(ping) = act["ping"].as_str() {
            let _ = socket.send(AxMessage::Ping(ping.as_bytes().to_vec().into())).await;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
            while let Ok(Some(Ok(message))) = tokio::time::timeout_at(deadline, socket.recv()).await {
                if let AxMessage::Pong(data) = message {
                    let pong = String::from_utf8_lossy(&data).into_owned();
                    shared.lock().unwrap().recorded.pongs.push(pong);
                    break;
                }
            }
        }
        let quoted = serde_json::to_string(&previous).unwrap();
        for frame in act["send"].as_array().into_iter().flatten() {
            let frame = frame.as_str().unwrap().replace("\"PREVIOUS_ID\"", &quoted);
            if socket.send(AxMessage::text(frame)).await.is_err() {
                return;
            }
        }
        if act["drop"].as_bool().unwrap_or(false) {
            // No close frame: the connection just ends.
            return;
        }
        if act["binary"].as_bool().unwrap_or(false) {
            let _ = socket.send(AxMessage::Binary(vec![1u8, 2, 3].into())).await;
        }
        if let Some(code) = act["close"].as_u64().filter(|c| *c != 0) {
            let _ = socket
                .send(AxMessage::Close(Some(CloseFrame {
                    code: code as u16,
                    reason: act["close_text"].as_str().unwrap_or_default().into(),
                })))
                .await;
            return;
        }
    }
}

fn credential(id: &str, attrs: &Value, addr: &str) -> Credential {
    let mut metadata = serde_json::Map::new();
    metadata.insert("type".into(), "xai".into());
    let mut c = Credential::from_file(
        std::path::Path::new("/fixture"),
        std::path::Path::new("/fixture/xai.json"),
        metadata,
    )
    .unwrap();
    c.id = id.to_owned();
    for (k, v) in attrs.as_object().unwrap() {
        if k == "access_token" {
            c.metadata.insert(k.clone(), v.clone());
            continue;
        }
        c.attributes
            .insert(k.clone(), v.as_str().unwrap().replace("UPSTREAM", addr));
    }
    c
}

fn request(turn: &Value, session: &str, usage: cpa_core::exec::UsageSink) -> ExecRequest {
    let payload = Bytes::from(turn["payload"].as_str().unwrap().to_owned());
    let headers = HeaderMap::new();
    // Go's executor-side session fallback (EnsureSessionContext -> CanonicalSessionID).
    let meta = cpa_common::session::Meta {
        execution_session: Some(session),
        derived: None,
    };
    let id = cpa_common::session::extract_session_id(&headers, &payload, &meta);
    let model = turn["model"].as_str().unwrap_or("grok-4.3").to_owned();
    ExecRequest {
        operation: Operation::Generate,
        source_format: Format::OpenAIResponse,
        response_format: turn["response"]
            .as_str()
            .and_then(Format::parse)
            .unwrap_or(Format::OpenAIResponse),
        requested_model: model.clone(),
        model,
        original_body: payload.clone(),
        body: payload,
        stream: true,
        alt: None,
        session: Some(id)
            .filter(|id| !id.is_empty())
            .map(|id| cpa_common::session::bound_session_identity(&id)),
        execution_session: Some(session.to_owned()),
        derived_session: None,
        resolved_model: None,
        usage,
        request_path: "/v1/responses".into(),
        headers,
        caller: Caller {
            principal: String::new(),
            source: "authorization",
            provider: String::new(),
        },
    }
}

fn decoded_events(chunks: &[String]) -> Vec<Value> {
    let mut events = Vec::new();
    for chunk in chunks {
        if let Ok(value) = serde_json::from_str::<Value>(chunk) {
            events.push(value);
            continue;
        }
        for data in chunk
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim)
            .filter(|data| *data != "[DONE]")
        {
            events.push(serde_json::from_str(data).expect("client event JSON"));
        }
    }
    events
}

fn text_fields(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                if matches!(key.as_str(), "text" | "delta" | "output" | "content")
                    && let Some(text) = value.as_str()
                    && !text.is_empty()
                {
                    out.push(text.into());
                }
                text_fields(value, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                text_fields(item, out);
            }
        }
        _ => {}
    }
}

/// An independent model consumes only client and peer input. It owns socket lifecycle,
/// response identity and transcript obligations; output records were removed from the
/// historical seeds. Pure translator and bridge suites separately pin their full schemas.
#[tokio::test]
async fn session_inputs_obey_transport_identity_transcript_compaction_tool_and_usage_contracts() {
    let scenarios: Value = serde_json::from_str(FIXTURE).unwrap();
    for scenario in scenarios["scenarios"].as_array().unwrap() {
        let name = scenario["name"].as_str().unwrap();
        let shared: Shared = Arc::default();
        let app = axum::Router::new().fallback(handler).with_state(shared.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let executor = XaiExecutor::with_client(wreq::Client::new());
        let cfg = Config::default();
        let session = scenario["session"].as_str().unwrap();
        let credentials: HashMap<String, Credential> = scenario["auths"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(id, attrs)| (id.clone(), credential(id, attrs, &addr)))
            .collect();
        let mut live: Option<String> = None;
        let mut last_target: Option<String> = None;
        let mut transcript_text = Vec::new();
        let mut ids = std::collections::BTreeMap::<String, String>::new();
        let mut sequence = 0;
        let mut compacted: Option<Value> = None;
        for (index, turn) in scenario["turns"].as_array().unwrap().iter().enumerate() {
            let context = format!("{name} turn {index}");
            {
                let mut script = shared.lock().unwrap();
                script.turn = Some(turn.clone());
                script.recorded = Recorded::default();
            }
            if turn["close"] == true {
                executor.close_session(session);
                live = None;
                last_target = None;
                ids.clear();
                transcript_text.clear();
                compacted = None;
                continue;
            }
            let credential = &credentials[turn["auth"].as_str().unwrap()];
            let source: Value = serde_json::from_str(turn["payload"].as_str().unwrap()).unwrap();
            let continuation = turn["continuation"] == true;
            let compact = source["input"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["type"] == "compaction_trigger"));
            let target_changed = last_target.as_deref().is_some_and(|id| id != credential.id);
            let previous = source["previous_response_id"].as_str().unwrap_or_default();
            let upstream_previous = ids.get(previous).map(String::as_str).unwrap_or(previous).to_owned();
            let replay = continuation && (compact || live.as_deref() != Some(credential.id.as_str()));
            let non_trigger = source["input"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter(|item| item["type"] != "compaction_trigger")
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let empty_compact = compact
                && transcript_text.is_empty()
                && compacted.is_none()
                && non_trigger.is_empty()
                && previous.is_empty();
            let acts = turn["acts"].as_array().cloned().unwrap_or_default();
            let mut peer = Vec::new();
            for act in &acts {
                for frame in act["send"].as_array().into_iter().flatten() {
                    let frame = frame
                        .as_str()
                        .unwrap()
                        .replace("\"PREVIOUS_ID\"", &serde_json::to_string(&upstream_previous).unwrap());
                    if let Ok(value) = serde_json::from_str::<Value>(&frame) {
                        peer.push(value);
                    }
                }
            }
            let reject = turn.get("reject").filter(|reply| !reply.is_null());
            let peer_error = peer.iter().find(|event| event.get("error").is_some());
            let read_error = acts.iter().find(|act| {
                act["binary"] == true || act["drop"] == true || act["close"].as_u64().is_some_and(|code| code != 0)
            });
            let error_status = if replay {
                Some(426)
            } else if empty_compact {
                Some(400)
            } else if compact {
                let response: Value = serde_json::from_str(turn["compact"]["body"].as_str().unwrap()).unwrap();
                (!response["output"].as_array().is_some_and(|items| {
                    items.iter().any(|item| {
                        item["type"] == "compaction"
                            && item["encrypted_content"].as_str().is_some_and(|s| !s.is_empty())
                    })
                }))
                .then_some(502)
            } else if let Some(reject) = reject {
                Some(local_error_status(
                    reject["status"].as_u64().unwrap() as u16,
                    &reject["body"].to_string(),
                ))
            } else if let Some(event) = peer_error {
                Some(local_error_status(
                    event["status"]
                        .as_u64()
                        .or_else(|| event["status_code"].as_u64())
                        .or_else(|| event["error"]["code"].as_str().and_then(|s| s.parse().ok()))
                        .unwrap_or_else(|| {
                            if event.to_string().contains("Request validation error") {
                                400
                            } else {
                                500
                            }
                        }) as u16,
                    &event.to_string(),
                ))
            } else {
                read_error.map(|act| {
                    if act["close"] == 1009 {
                        413
                    } else if source.to_string().contains("apply_patch") {
                        502
                    } else {
                        500
                    }
                })
            };
            let (recorder, capture, usage) = sinks();
            let exec_session = ExecSession {
                id: session.into(),
                continuation,
                lease: None,
            };
            let response = executor
                .execute_in_session(credential, request(turn, session, usage), &cfg, &exec_session)
                .await;
            let mut chunks = Vec::new();
            let mut error = None;
            match response {
                Err(e) => error = Some(e),
                Ok(response) => {
                    let ResponseBody::Stream(mut stream) = response.body else {
                        panic!("{context}: streaming")
                    };
                    while let Some(item) = stream.next().await {
                        match item {
                            Ok(chunk) => chunks.push(String::from_utf8(chunk.to_vec()).unwrap()),
                            Err(e) => error = Some(e),
                        }
                    }
                }
            }
            assert_eq!(error.as_ref().map(|e| e.status), error_status, "{context}: {error:?}");
            if let Some(error) = &error {
                if error.status == 426 {
                    assert!(error.is_replay_required(), "{context}");
                }
                if error.status == 429 {
                    assert_eq!(error.retry_after, Some(Duration::from_secs(86400)), "{context}");
                }
            }
            let recorded = std::mem::take(&mut shared.lock().unwrap().recorded);
            let events = decoded_events(&chunks);
            if replay || empty_compact {
                assert!(
                    recorded.frames.is_empty() && recorded.http.is_empty() && recorded.upgrades.is_empty(),
                    "{context}: refusal sends nothing"
                );
            } else if compact {
                assert_eq!(recorded.http.len(), 1, "{context}");
                assert!(recorded.frames.is_empty(), "{context}");
                let (line, body) = recorded.http[0].split_once('\n').unwrap();
                assert_eq!(line, "POST /v1/responses/compact", "{context}");
                let body: Value = serde_json::from_str(body).unwrap();
                assert!(!body.to_string().contains("compaction_trigger"), "{context}");
                assert_eq!(body["model"], turn["model"].as_str().unwrap_or("grok-4.3"), "{context}");
                if !transcript_text.is_empty() {
                    let wire = body.to_string();
                    for text in &transcript_text {
                        assert!(wire.contains(text), "{context}: transcript text {text:?} preserved");
                    }
                }
                if error.is_none() {
                    let reply: Value = serde_json::from_str(turn["compact"]["body"].as_str().unwrap()).unwrap();
                    let item = reply["output"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|item| item["type"] == "compaction")
                        .unwrap();
                    assert!(
                        events
                            .iter()
                            .any(|event| event.to_string().contains(item["encrypted_content"].as_str().unwrap())),
                        "{context}: compact state reaches client"
                    );
                    compacted = Some(item.clone());
                    transcript_text.clear();
                    ids.insert(reply["id"].as_str().unwrap().into(), String::new());
                }
            } else {
                if reject.is_none() {
                    assert!(!recorded.frames.is_empty(), "{context}: valid request reaches peer");
                }
                assert_eq!(
                    recorded.upgrades.len(),
                    usize::from(live.as_deref() != Some(credential.id.as_str())),
                    "{context}: dial only for new target"
                );
                for upgrade in &recorded.upgrades {
                    let mut lines = upgrade.lines();
                    assert_eq!(lines.next(), Some("/v1/responses"), "{context}");
                    let values = lines.collect::<Vec<_>>();
                    assert!(
                        values.contains(&"Content-Type: application/json"),
                        "{context}: JSON handshake"
                    );
                    assert!(
                        values
                            .iter()
                            .any(|line| line.strip_prefix("X-Grok-Conv-Id: ").is_some_and(|id| !id.is_empty())),
                        "{context}: bound conversation identity"
                    );
                    if let Some(trace) = credential.attributes.get("header:X-Trace") {
                        assert!(
                            values.contains(&format!("X-Trace: {trace}").as_str()),
                            "{context}: operator header"
                        );
                    }
                    let token = credential
                        .attributes
                        .get("api_key")
                        .map(String::as_str)
                        .or_else(|| credential.str("access_token"))
                        .unwrap_or_default();
                    assert!(
                        values.contains(&format!("Authorization: Bearer {token}").as_str()),
                        "{context}: selected credential {upgrade}"
                    );
                }
                for frame in &recorded.frames {
                    let body: Value = serde_json::from_str(frame).unwrap();
                    assert_eq!(body["type"], "response.create", "{context}");
                    assert_eq!(body["store"], true, "{context}");
                    assert_eq!(
                        body["model"],
                        turn["model"].as_str().unwrap_or("grok-4.3"),
                        "{context}: routed model"
                    );
                    let mut source_text = Vec::new();
                    text_fields(&source["input"], &mut source_text);
                    if let Some(text) = source["input"].as_str() {
                        source_text.push(text.to_owned());
                    }
                    for text in &source_text {
                        assert!(frame.contains(text), "{context}: current input text {text:?} preserved");
                    }
                    assert!(body.get("stream").is_none(), "{context}");
                    assert!(body.get("background").is_none(), "{context}");
                    if body["previous_response_id"].as_str().is_some_and(|s| !s.is_empty()) {
                        assert!(body.get("instructions").is_none(), "{context}");
                        assert_eq!(body["previous_response_id"], upstream_previous, "{context}");
                    }
                    if target_changed && !previous.is_empty() {
                        assert!(
                            body.get("previous_response_id").is_none(),
                            "{context}: replay loses foreign response ID"
                        );
                        for text in &transcript_text {
                            assert!(
                                frame.contains(text),
                                "{context}: replay retains transcript text {text:?}"
                            );
                        }
                    }
                    if let Some(item) = &compacted
                        && (previous.is_empty() || ids.get(previous).is_some_and(String::is_empty))
                    {
                        assert!(
                            body["input"]
                                .as_array()
                                .is_some_and(|items| items.iter().any(|value| value["type"] == "compaction"
                                    && value["encrypted_content"] == item["encrypted_content"])),
                            "{context}: compacted state replay"
                        );
                    }
                }
                let expected_pings: Vec<String> = acts
                    .iter()
                    .take(recorded.frames.len())
                    .filter_map(|act| act["ping"].as_str().map(str::to_owned))
                    .collect();
                assert_eq!(recorded.pongs, expected_pings, "{context}: ping payloads");
                if error.is_none() {
                    assert!(!events.is_empty(), "{context}: successful turn emits events");
                    if source["generate"] == false {
                        assert!(
                            events.iter().any(|event| event["type"] == "response.completed"
                                && event["response"]["status"] == "completed"),
                            "{context}: prewarm completes"
                        );
                    }
                    for upstream in &peer {
                        let kind = upstream["type"].as_str().unwrap_or_default();
                        if kind == "response.output_text.delta" {
                            assert!(
                                events.iter().any(|event| event["delta"] == upstream["delta"]),
                                "{context}: text delta preserved"
                            );
                        }
                    }
                    if let Some(upstream) = peer.iter().find_map(|event| event["response"]["id"].as_str()) {
                        let expected = if ids.contains_key(upstream)
                            || (!upstream_previous.is_empty() && upstream == upstream_previous)
                        {
                            sequence += 1;
                            format!("{upstream}-xai-{sequence}")
                        } else {
                            upstream.into()
                        };
                        assert!(
                            events.iter().any(|event| event["response"]["id"].is_string()),
                            "{context}: response identity is emitted"
                        );
                        assert!(
                            events
                                .iter()
                                .filter_map(|event| event["response"]["id"].as_str())
                                .all(|id| id == expected),
                            "{context}: consistent unique response ID"
                        );
                        ids.insert(upstream.into(), upstream.into());
                        ids.insert(expected, upstream.into());
                    }
                    assert_tool_contract(name, &events);
                    if let Some(completed) = peer.iter().find(|event| event["type"] == "response.completed") {
                        if previous.is_empty() && source["type"] != "response.append" {
                            transcript_text.clear();
                        }
                        text_fields(&source["input"], &mut transcript_text);
                        text_fields(&completed["response"]["output"], &mut transcript_text);
                        compacted = None;
                    }
                }
            }
            let reports = recorder.0.lock().unwrap();
            let sent = !recorded.frames.is_empty() || !recorded.http.is_empty();
            let arrived = peer.iter().any(|event| event.is_object()) || !recorded.http.is_empty();
            assert!(
                crate::usage_testkit::ttft_problem(&reports, sent, arrived).is_none(),
                "{context}: TTFT"
            );
            let usage = published(&reports, error.is_some());
            if let Some(usage) = usage {
                assert_eq!(usage["failed"], error.is_some(), "{context}");
                if error.is_some() {
                    assert_eq!(usage["input"], 0, "{context}");
                    assert_eq!(usage["output"], 0, "{context}");
                } else {
                    let source_usage = if compact {
                        serde_json::from_str::<Value>(turn["compact"]["body"].as_str().unwrap()).unwrap()["usage"]
                            .clone()
                    } else {
                        peer.iter()
                            .find(|event| {
                                matches!(event["type"].as_str(), Some("response.completed" | "response.done"))
                            })
                            .map(|event| event["response"]["usage"].clone())
                            .unwrap_or(Value::Null)
                    };
                    if source_usage.is_object() {
                        for (local, upstream) in [
                            ("input", "input_tokens"),
                            ("output", "output_tokens"),
                            ("total", "total_tokens"),
                        ] {
                            assert_eq!(
                                usage[local].as_i64().unwrap(),
                                source_usage[upstream].as_i64().unwrap_or(0),
                                "{context}: {local}"
                            );
                        }
                    }
                }
            }
            drop(reports);
            let problems = capture_problems(
                name,
                turn,
                &recorded,
                &capture.take(),
                credential,
                &addr,
                error.as_ref(),
            );
            assert!(problems.is_empty(), "{context}: {problems:#?}");
            if !compact {
                if error.is_none() {
                    last_target = Some(credential.id.clone());
                }
                live = if error.is_none() {
                    Some(credential.id.clone())
                } else {
                    None
                };
            }
        }
        executor.close_session(session);
        server.abort();
    }
}

fn local_error_status(status: u16, body: &str) -> u16 {
    if body.contains("free-usage-exhausted") || body.contains("included free usage exhausted") {
        429
    } else if status == 403
        && (body.contains("bad-credentials") || body.contains("Access token could not be validated"))
    {
        401
    } else {
        status
    }
}

fn assert_tool_contract(name: &str, events: &[Value]) {
    let text = serde_json::to_string(events).unwrap();
    if name.starts_with("ApplyPatchTransport/") {
        assert!(text.contains("custom_tool_call"), "{name}: custom tool restored");
        assert!(text.contains("中😀"), "{name}: unicode patch survives");
        assert!(text.contains("apply_patch"), "{name}: client tool name restored");
    }
    if name == "RestoresNamespaceToolCalls" {
        assert!(
            events
                .iter()
                .any(|event| event["item"]["name"] == "web_search_exa" && event["item"]["namespace"] == "mcp__exa"),
            "{name}: namespace restored {events:?}"
        );
    }
    if name == "RestoresAliasedWebSearch" {
        assert!(
            events.iter().any(|event| event["item"]["name"] == "web_search"),
            "{name}: alias restored"
        );
        assert!(!text.contains("clientfn_web_search"));
    }
    if name == "DoesNotRestoreNamespacedClientfnWebSearch" {
        assert!(
            events
                .iter()
                .any(|event| event["item"]["name"] == "clientfn_web_search" && event["item"]["namespace"] == "acme")
        );
        assert!(
            events
                .iter()
                .any(|event| event["item"]["name"] == "web_search" && event["item"]["call_id"] == "call_2")
        );
    }
    if name.starts_with("Preserves") && name.ends_with("WithXSearch") {
        assert!(
            !events.iter().any(|event| event["item"]["call_id"] == "xs_call-1"),
            "built-in server search calls are hidden while client declarations remain"
        );
        assert!(events.iter().any(|event| {
            event["item"]["call_id"]
                .as_str()
                .is_some_and(|id| id == "call_plain" || id == "call_custom")
                && event["item"]["name"] == "x_keyword_search"
        }));
    }
    if name == "NormalizesReasoningTextEvents" {
        assert!(
            events
                .iter()
                .any(|event| event["type"] == "response.reasoning_summary_text.delta" && event["delta"] == "thinking")
        );
        assert!(
            !events
                .iter()
                .any(|event| event["type"] == "response.reasoning_text.delta")
        );
    }
}

/// A continuation request is captured before the retained-socket check, even when
/// replay is required and no frame goes out.
const WS_REQUEST_WITHOUT_FRAME: [&str; 1] = ["continuation_without_socket"];

/// One capture event as the order check sees it.
fn shape(event: &Captured) -> String {
    match event {
        Captured::Request {
            websocket: true,
            url,
            body,
            ..
        } => format!("ws-request {url} {body}"),
        Captured::Request { method, url, body, .. } => format!("http-request {method} {url} {body}"),
        Captured::Metadata(status, _) => format!("metadata {status}"),
        Captured::Chunk(body) => format!("chunk {body}"),
        Captured::Error(error) => format!("error {error}"),
        Captured::Handshake(status, _) => format!("handshake {status}"),
        Captured::WsResponse(payload) => format!("response {payload}"),
        Captured::WsError(stage, _) => format!("ws-error {stage}"),
    }
}

/// Capture expectations come from the local peer's actual requests and its scripted replies:
/// a compact request is an HTTP attempt (`recordXAIRequest`, metadata, whole body); a
/// WebSocket turn logs its request frame (`RecordAPIWebsocketRequest`), then a rejected
/// upgrade as an HTTP attempt (GET on the http URL, metadata, body) or a new socket's
/// 101 handshake, then every upstream message (`AppendAPIWebsocketResponse`) and the
/// terminal error's stage.
fn capture_problems(
    name: &str,
    turn: &Value,
    recorded: &Recorded,
    events: &[Captured],
    credential: &Credential,
    addr: &str,
    delivered_error: Option<&ExecError>,
) -> Vec<String> {
    let mut problems = Vec::new();
    let mut want = Vec::new();
    for http in &recorded.http {
        let (line, body) = http.split_once('\n').unwrap_or((http, ""));
        let uri = line.split(' ').nth(1).unwrap_or_default();
        let reply = &turn["compact"];
        want.push(format!("http-request POST http://{addr}{uri} {body}"));
        want.push(format!("metadata {}", reply["status"]));
        want.push(format!("chunk {}", reply["body"].as_str().unwrap()));
    }
    let uri = recorded.upgrades.first().map(|u| u.lines().next().unwrap_or_default());
    let ws_url = format!("ws://{addr}{}", uri.unwrap_or("/v1/responses"));
    let websocket =
        !recorded.upgrades.is_empty() || !recorded.frames.is_empty() || WS_REQUEST_WITHOUT_FRAME.contains(&name);
    if websocket {
        match recorded.frames.first() {
            Some(frame) => want.push(format!("ws-request {ws_url} {frame}")),
            None => want.push(format!("ws-request {ws_url} <any>")),
        }
        let reject = turn.get("reject").filter(|r| !r.is_null());
        for _ in &recorded.upgrades {
            match reject {
                Some(reply) => {
                    want.push(format!("http-request GET http://{addr}{} ", uri.unwrap()));
                    want.push(format!("metadata {}", reply["status"]));
                    want.push(format!("chunk {}", reply["body"].as_str().unwrap()));
                }
                None => want.push("handshake 101".to_owned()),
            }
        }
        let acts = turn["acts"].as_array().cloned().unwrap_or_default();
        let mut stage = None;
        for (frame, act) in recorded.frames.iter().zip(&acts) {
            let previous = crate::rawjson::get(frame, "previous_response_id").str().to_owned();
            let quoted = serde_json::to_string(&previous).unwrap();
            for message in act["send"].as_array().into_iter().flatten() {
                let message = message.as_str().unwrap().replace("\"PREVIOUS_ID\"", &quoted);
                let message = message.trim();
                if message.is_empty() {
                    continue;
                }
                want.push(format!("response {message}"));
                if serde_json::from_str::<Value>(message).is_ok_and(|event| event.get("error").is_some()) {
                    stage = Some("upstream_error");
                }
            }
            // The session reader hands a binary message, a close or a dropped connection
            // to the turn as a read error.
            if act["binary"] == true || act["close"].as_u64().is_some_and(|c| c != 0) || act["drop"] == true {
                stage = Some("read");
            }
        }
        // A read error with an unfinished apply_patch call ends through the bridge's
        // failure, which Go returns before logging the read.
        let bridge_failure = delivered_error
            .is_some_and(|error| error.body.as_ref() == cpa_translate::APPLY_PATCH_UPSTREAM_ERROR.as_bytes());
        if let Some(stage) = stage.filter(|s| !(*s == "read" && bridge_failure)) {
            want.push(format!("ws-error {stage}"));
        }
    }
    let got: Vec<String> = events
        .iter()
        .map(|e| match e {
            Captured::Request {
                websocket: true, url, ..
            } if recorded.frames.is_empty() => {
                format!("ws-request {url} <any>")
            }
            e => shape(e),
        })
        .collect();
    if got != want {
        problems.push(format!("capture:\n  want {want:#?}\n  got  {got:#?}"));
    }
    // Capture must report the same error delivered by this execution, independent
    // of either a Go recorder or a WebSocket library's diagnostic rendering.
    for event in events {
        match event {
            Captured::WsError(stage, text) => {
                if !delivered_error.is_some_and(|error| text.as_bytes() == error.body.as_ref()) {
                    problems.push(format!(
                        "capture {stage} error {text:?} disagrees with delivered error {delivered_error:?}"
                    ));
                }
            }
            Captured::Metadata(_, headers)
                if turn.get("reject").is_some_and(|r| !r.is_null())
                    && !headers.iter().any(|(name, value)| {
                        name.eq_ignore_ascii_case("content-type") && value == "application/json"
                    }) =>
            {
                problems.push(format!("capture rejection headers: {headers:?}"))
            }
            _ => {}
        }
    }
    let (auth_type, auth_value) = cpa_core::registry::dynamic::account_info(credential);
    for event in events {
        if let Captured::Request {
            websocket,
            method,
            headers,
            provider,
            auth,
            ..
        } = event
        {
            let want_auth = [
                credential.id.clone(),
                credential.label.clone(),
                auth_type.to_owned(),
                auth_value.clone(),
            ];
            if provider != "xai" || auth != &want_auth {
                problems.push(format!("capture account: {provider} {auth:?}"));
            }
            let has = |n: &str, v: &str| headers.iter().any(|(hn, hv)| hn.eq_ignore_ascii_case(n) && hv == v);
            if *websocket && method != "WEBSOCKET" {
                problems.push(format!("capture ws method: {method}"));
            }
            // websocketUpgradeRequestLog: the upgrade headers a rejected dial carried.
            if method == "GET" && !(has("Connection", "Upgrade") && has("Upgrade", "websocket")) {
                problems.push(format!("capture rejected upgrade headers: {headers:?}"));
            }
        }
    }
    problems
}

#[test]
fn stored_websocket_flags_use_explicit_attribute_then_typed_metadata() {
    let mut c = credential("a", &serde_json::json!({}), "");
    assert!(!websockets_enabled(&c));
    c.metadata.insert("websockets".into(), Value::Bool(true));
    assert!(websockets_enabled(&c));
    c.attributes.insert("websockets".into(), "false".into());
    assert!(!websockets_enabled(&c), "a parsable attribute wins");
    c.attributes.insert("websockets".into(), "maybe".into());
    assert!(websockets_enabled(&c), "an unparsable attribute falls back to metadata");
    c.metadata.insert("websockets".into(), Value::String(" TRUE ".into()));
    assert!(websockets_enabled(&c));
    c.metadata.insert("websockets".into(), Value::from(1));
    assert!(!websockets_enabled(&c), "numbers are not bools");
}

#[test]
fn bare_error_frames_prefer_the_first_positive_status() {
    for (code, nested_status, outer_code, message, expected) in [
        (" +00429 ", "401", "402", "", 429),
        ("-1", " 408 ", "402", "", 408),
        ("0", "4_01", "418", "", 418),
        ("9223372036854775808", "+", "  ", "Request validation error", 400),
        ("4.0", "", "", "", 500),
        ("65536", "401", "", "", 500),
    ] {
        let frame = serde_json::json!({
            "error": {"code": code, "status": nested_status, "message": message},
            "code": outer_code,
        });
        let error = ws_error(frame.to_string().as_bytes()).unwrap();
        assert_eq!(error.status, expected, "{frame}");
    }
}

/// Standard scheme conversion preserves the resource path, escaped query and port.
#[test]
fn websocket_urls_keep_the_http_resource_and_refuse_missing_or_unsupported_origins() {
    for (input, expected) in [
        (
            "http://example.invalid/v1/responses?x=a%2Fb",
            "ws://example.invalid/v1/responses?x=a%2Fb",
        ),
        (
            "https://example.invalid:8443/v1/responses",
            "wss://example.invalid:8443/v1/responses",
        ),
        (" ws://127.0.0.1:9/responses ", "ws://127.0.0.1:9/responses"),
        ("wss://example.invalid/responses", "wss://example.invalid/responses"),
    ] {
        assert_eq!(ws_url(input).unwrap(), expected);
    }
    for input in [
        "",
        "/responses",
        "https:///",
        "ftp://example.invalid/responses",
        "http://[broken",
    ] {
        let error = ws_url(input).unwrap_err();
        assert_eq!(
            (error.status, error.scope),
            (0, cpa_core::exec::FailureScope::Model),
            "{input}"
        );
        assert!(!error.body.is_empty());
        assert_eq!(error.retry_after, None);
    }
}

/// Go sets no read deadline on xAI sockets; the shared reader's must outlast any session.
#[test]
fn xai_sockets_have_no_read_deadline() {
    assert!(Ws::default().pool.idle >= Duration::from_secs(365 * 24 * 3600));
}

/// Go's reader always delivers its terminal read (`sendTerminalWebsocketRead` waits for
/// room). Here the queue is full when the close 1009 arrives: 4097 deltas fill the
/// 4096-slot queue while the consumer pauses, so the reader's error cannot be queued and
/// must still reach the turn as the request-scoped 413.
#[tokio::test]
async fn terminal_error_survives_a_full_queue() {
    let shared: Shared = Arc::default();
    let app = axum::Router::new().fallback(handler).with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let delta = r#"{"type":"response.output_text.delta","item_id":"m","output_index":0,"content_index":0,"delta":"x"}"#;
    let turn = serde_json::json!({
        "auth": "auth-a",
        "payload": r#"{"model":"grok-4.3","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],"stream":true}"#,
        "acts": [{"send": vec![delta; 4097], "close": 1009, "close_text": "too big"}],
    });
    shared.lock().unwrap().turn = Some(turn.clone());
    let attrs = serde_json::json!({"api_key": "sk-fake", "base_url": "http://UPSTREAM/v1", "websockets": "true"});
    let credential = credential("auth-a", &attrs, &addr);
    let executor = XaiExecutor::with_client(wreq::Client::new());
    let session = ExecSession {
        id: "sess-full".into(),
        continuation: false,
        lease: None,
    };
    let response = executor
        .execute_in_session(
            &credential,
            request(&turn, "sess-full", Default::default()),
            &Config::default(),
            &session,
        )
        .await
        .unwrap();
    let ResponseBody::Stream(mut stream) = response.body else {
        panic!("buffered response");
    };
    // Let the reader fill the queue, take one event so it queues the last delta, then pause
    // again while it reads the close.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(stream.next().await.unwrap().is_ok());
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut events = 1;
    let mut last = None;
    while let Some(item) = stream.next().await {
        match item {
            Ok(_) => events += 1,
            Err(e) => last = Some(e),
        }
    }
    assert_eq!(events, 4097);
    let error = last.expect("terminal error");
    assert_eq!(error.status, 413);
    assert_eq!(
        String::from_utf8_lossy(&error.body),
        r#"{"error":{"message":"upstream websocket message too big","type":"invalid_request_error","code":"message_too_big"}}"#
    );
    executor.close_session("sess-full");
    server.abort();
}

/// Go `EmitWebSocketResponseEvent` beside `AppendAPIWebsocketResponse`: every non-empty
/// upstream frame, trimmed, goes to the attempt's observer with the executor identifier,
/// the selected credential and the upstream model. The executor awaits each
/// observation before it handles the frame, as Go calls the observer in its read loop.
#[tokio::test]
async fn upstream_frames_reach_the_websocket_observer() {
    /// (provider, credential ID, model, payload).
    type Frame = (String, String, String, Vec<u8>);
    /// The frames observed, and the order observations and client chunks finished in.
    #[derive(Default)]
    struct Seen(std::sync::Mutex<Vec<Frame>>, Arc<std::sync::Mutex<Vec<&'static str>>>);
    impl cpa_core::exec::WebSocketObserver for Seen {
        fn response<'a>(
            &'a self,
            provider: &'a str,
            credential: &'a Credential,
            model: &'a str,
            payload: &'a [u8],
        ) -> futures_util::future::BoxFuture<'a, ()> {
            Box::pin(async move {
                // A slow observer: the frame must not reach the client before it returns.
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                self.0.lock().unwrap().push((
                    provider.to_owned(),
                    credential.id.clone(),
                    model.to_owned(),
                    payload.to_vec(),
                ));
                self.1.lock().unwrap().push("observed");
            })
        }
    }
    let shared: Shared = Arc::default();
    let app = axum::Router::new().fallback(handler).with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let created = r#"{"type":"response.created","response":{"id":"r1","status":"in_progress","output":[]}}"#;
    let done = r#"{"type":"response.completed","response":{"id":"r1","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#;
    let turn = serde_json::json!({
        "auth": "auth-a",
        "payload": r#"{"model":"grok-4.3","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],"stream":true}"#,
        "acts": [{"send": [format!("  {created}\n"), "   ".to_owned(), done.to_owned()]}],
    });
    shared.lock().unwrap().turn = Some(turn.clone());
    let attrs = serde_json::json!({"api_key": "sk-fake", "base_url": "http://UPSTREAM/v1", "websockets": "true"});
    let credential = credential("auth-a", &attrs, &addr);
    let executor = XaiExecutor::with_client(wreq::Client::new());
    let session = ExecSession {
        id: "sess-observe".into(),
        continuation: false,
        lease: None,
    };
    let seen = Arc::new(Seen::default());
    let usage = cpa_core::exec::UsageSink::default().with_websocket(Some(seen.clone()));
    let response = executor
        .execute_in_session(
            &credential,
            request(&turn, "sess-observe", usage),
            &Config::default(),
            &session,
        )
        .await
        .unwrap();
    let ResponseBody::Stream(mut stream) = response.body else {
        panic!("buffered response");
    };
    let order = seen.1.clone();
    while let Some(item) = stream.next().await {
        item.unwrap();
        order.lock().unwrap().push("chunk");
    }
    // Each frame's observation finishes before its client chunk.
    let order = order.lock().unwrap().clone();
    assert_eq!(order.first(), Some(&"observed"), "{order:?}");
    assert_eq!(order.iter().filter(|o| **o == "observed").count(), 2, "{order:?}");
    assert_eq!(order.last(), Some(&"chunk"), "{order:?}");
    let seen = seen.0.lock().unwrap().clone();
    let expected: Vec<Frame> = [created, done]
        .into_iter()
        .map(|frame| {
            (
                "xai".into(),
                credential.id.clone(),
                "grok-4.3".into(),
                frame.as_bytes().to_vec(),
            )
        })
        .collect();
    assert_eq!(seen, expected);
    executor.close_session("sess-observe");
    server.abort();
}

// --- unit-level ports of xai_websockets_executor_test.go -------------------------------

/// The server's `classify::upstream_attempted` (Go `UpstreamAttempted`).
fn attempted(e: &ExecError) -> bool {
    !e.headers.is_empty() || e.scope == FailureScope::Transport || e.status == 0
}

/// TestXAIWebsocketsEnabledForConfigAPIKey, directly and through the config synthesizer.
#[test]
fn websockets_enabled_for_config_api_key() {
    let attrs = serde_json::json!({"api_key": "xai-key", "websockets": "true"});
    assert!(websockets_enabled(&credential("a", &attrs, "")));
    let cfg = Config::parse("api-keys:\n  xai:\n    - base-url: http://127.0.0.1:9/v1\n      keys:\n        - api-key: xai-key\n          websockets: true\n").unwrap();
    let synthesized = cpa_core::config::credentials::from_config(&cfg);
    let xai = synthesized
        .iter()
        .find(|c| c.provider == "xai")
        .expect("xai credential");
    assert!(websockets_enabled(xai));
}

/// TestXAIWebsocketMissingRequiredSessionDoesNotMarkUpstreamAttempt and
/// TestXAIWebsocketsRequiredUpstreamRejectsCompactionHTTPFallback: a continuation without
/// a retained socket is the request-scoped replay signal and no upstream attempt.
#[tokio::test]
async fn missing_required_session_is_replay_without_attempt() {
    let executor = XaiExecutor::with_client(wreq::Client::new());
    let attrs = serde_json::json!({"api_key": "xai-key", "websockets": "true", "base_url": "http://127.0.0.1:9/v1"});
    let credential = credential("xai-required-session", &attrs, "");
    let session = ExecSession {
        id: "missing-xai-session".into(),
        continuation: true,
        lease: None,
    };
    for payload in [
        r#"{"model":"grok-4","previous_response_id":"resp-1","input":[{"type":"message","role":"user","content":"hello"}]}"#,
        r#"{"model":"grok-4","input":[{"type":"compaction_trigger"}]}"#,
    ] {
        let turn = serde_json::json!({"model": "grok-4", "payload": payload});
        let Err(error) = executor
            .execute_in_session(
                &credential,
                request(&turn, "missing-xai-session", Default::default()),
                &Config::default(),
                &session,
            )
            .await
        else {
            panic!("{payload}: continuation succeeded");
        };
        assert!(error.is_replay_required(), "{payload}");
        assert_eq!(error.scope, FailureScope::Request);
        assert!(!attempted(&error), "{payload}: marked as an upstream attempt");
    }
}

/// TestXAIWebsocketsExecuteStreamHandshakeFreeUsageExhaustedSetsRetryAfter's
/// `UpstreamAttempted`: a rejected upgrade is an upstream attempt (Go marks it on the dial
/// error), while a successful handshake alone marks nothing
/// (TestXAIWebsocketSuccessfulHandshakeDoesNotMarkRequestAttempt: no error exists).
#[tokio::test]
async fn rejected_handshake_is_an_upstream_attempt() {
    let app = axum::Router::new().fallback(|| async {
        (
            http::StatusCode::TOO_MANY_REQUESTS,
            [("content-type", "application/json")],
            r#"{"code":"subscription:free-usage-exhausted","error":"You've used all the included free usage for now."}"#,
        )
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let attrs =
        serde_json::json!({"base_url": "http://UPSTREAM/v1", "websockets": "true", "access_token": "xai-token"});
    let credential = credential("xai-auth-free-usage", &attrs, &addr);
    let executor = XaiExecutor::with_client(wreq::Client::new());
    let turn = serde_json::json!({"payload": r#"{"model":"grok-4.3","input":"hello"}"#});
    let session = ExecSession {
        id: "sess-attempt".into(),
        continuation: false,
        lease: None,
    };
    let Err(error) = executor
        .execute_in_session(
            &credential,
            request(&turn, "sess-attempt", Default::default()),
            &Config::default(),
            &session,
        )
        .await
    else {
        panic!("rejected upgrade succeeded");
    };
    assert_eq!(error.status, 429);
    assert_eq!(error.retry_after, Some(Duration::from_secs(24 * 3600)));
    assert!(attempted(&error));
    server.abort();
}

/// The headers of one loopback upgrade and the first frame sent on it.
type Upgrade = Arc<StdMutex<Option<(HeaderMap, String)>>>;

/// Accepts the upgrade, keeps its headers and first frame, and completes the turn.
async fn witness(State(seen): State<Upgrade>, req: axum::extract::Request) -> Response {
    let (mut parts, _) = req.into_parts();
    let headers = parts.headers.clone();
    let upgrade = WebSocketUpgrade::from_request_parts(&mut parts, &()).await.unwrap();
    upgrade.on_upgrade(move |mut socket| async move {
        let mut headers = Some(headers);
        while let Some(Ok(message)) = socket.recv().await {
            let AxMessage::Text(text) = message else { continue };
            if let Some(headers) = headers.take() {
                *seen.lock().unwrap() = Some((headers, text.as_str().to_owned()));
            }
            let completed = r#"{"type":"response.completed","response":{"id":"resp-1","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#;
            if socket.send(AxMessage::text(completed)).await.is_err() {
                return;
            }
        }
    })
}

/// #88 on the upstream Responses WebSocket: the handshake takes only `User-Agent` from
/// the caller (its first value, trimmed, or the Grok CLI's when absent or blank), a
/// `header:` attribute still wins, and the frame keeps the client's opaque members.
/// Go's handshake never carries the chat proxy identity. This is local policy evidence,
/// not evidence that xAI accepts any of these identities.
#[tokio::test]
async fn caller_user_agent_on_websocket() {
    const OPAQUE_MEMBER: &str = r#""future_field" : {"opaque":"\u0061", "big":12345678901234567890}, "#;
    let payload =
        format!(r#"{{{OPAQUE_MEMBER}"model":"grok-4.3","input":[{{"type":"message","role":"user","content":"hi"}}]}}"#);
    let mut frames = Vec::new();
    for (caller, custom, expected) in [
        (None, None, "xai-grok-workspace/1.0.44"),
        (Some(""), None, "xai-grok-workspace/1.0.44"),
        (Some(" \t "), None, "xai-grok-workspace/1.0.44"),
        (Some("caller-ws/2.0"), None, "caller-ws/2.0"),
        (Some("\u{a0} caller-ws/2.0 \t"), None, "caller-ws/2.0"),
        (Some("caller-\u{e9}/2.0"), None, "caller-\u{e9}/2.0"),
        (Some("caller-ws/2.0"), Some("operator/3.0"), "operator/3.0"),
        (Some("caller-ws/2.0"), Some("$X-Explicit-UA"), "referenced/4.0"),
        (Some("caller-ws/2.0"), Some("$X-Missing"), "caller-ws/2.0"),
    ] {
        let case = format!("caller {caller:?}, header:User-Agent {custom:?}");
        let seen: Upgrade = Arc::default();
        let app = axum::Router::new().fallback(witness).with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let attrs =
            serde_json::json!({"api_key": "upstream-FAKE", "base_url": "http://UPSTREAM/v1", "websockets": "true"});
        let mut c = credential("xai-ua", &attrs, &addr);
        if let Some(custom) = custom {
            c.attributes.insert("header:User-Agent".into(), custom.into());
        }
        let mut req = request(
            &serde_json::json!({ "payload": payload }),
            "sess-ua",
            Default::default(),
        );
        for (name, value) in [
            ("authorization", "Bearer caller-secret-FAKE"),
            ("cookie", "session=caller-cookie-FAKE"),
            ("x-api-key", "caller-api-key-FAKE"),
            ("proxy-authorization", "Basic caller-proxy-FAKE"),
            ("x-amz-security-token", "caller-token-FAKE"),
            ("x-unrelated", "caller-extra"),
            ("x-explicit-ua", "referenced/4.0"),
        ] {
            req.headers.insert(name, value.parse().unwrap());
        }
        if let Some(caller) = caller {
            req.headers.insert("user-agent", caller.parse().unwrap());
            req.headers.append("user-agent", "second-client/9.0".parse().unwrap());
        }
        let executor = XaiExecutor::with_client(wreq::Client::new());
        // A private auth-dir that is never created: a WebSocket turn reads no logins.
        let cfg = Config {
            auth_dir: std::env::temp_dir().join(format!("cpa-xai-ws-ua-{}", std::process::id())),
            ..Default::default()
        };
        let session = ExecSession {
            id: "sess-ua".into(),
            continuation: false,
            lease: None,
        };
        let response = executor
            .execute_in_session(&c, req, &cfg, &session)
            .await
            .unwrap_or_else(|e| panic!("{case}: {e:?}"));
        let ResponseBody::Stream(stream) = response.body else {
            panic!("{case}: buffered response");
        };
        for chunk in stream.collect::<Vec<_>>().await {
            chunk.unwrap_or_else(|e| panic!("{case}: {e:?}"));
        }
        let (headers, frame) = seen.lock().unwrap().take().expect("the upstream saw a frame");
        server.abort();
        let agents: Vec<&[u8]> = headers.get_all("user-agent").iter().map(|v| v.as_bytes()).collect();
        assert_eq!(agents, [expected.as_bytes()], "{case}");
        assert_eq!(headers["authorization"], "Bearer upstream-FAKE", "{case}");
        for name in [
            "cookie",
            "x-api-key",
            "proxy-authorization",
            "x-amz-security-token",
            "x-unrelated",
            "x-explicit-ua",
            "x-xai-token-auth",
            "x-grok-client-version",
        ] {
            assert!(!headers.contains_key(name), "{case}: {name} reached upstream");
        }
        assert!(frame.contains(OPAQUE_MEMBER), "{case}: {frame}");
        frames.push(frame);
    }
    assert!(frames.windows(2).all(|w| w[0] == w[1]), "{frames:?}");
}

/// TestMapXAIWebsocketWriteErrorStopsRetryForMessageTooBig: a write failure after the
/// upstream closed with 1009 is the request-scoped 413 and never retries; any other write
/// failure retries on a fresh socket.
#[test]
fn write_error_after_message_too_big_stops_retry() {
    let too_big = pool_error(ExecError::local(
        413,
        FailureScope::Request,
        r#"{"error":{"message":"frame too large","type":"invalid_request_error","code":"message_too_big"}}"#,
    ));
    assert_eq!(too_big.status, 413);
    assert_eq!(too_big.scope, FailureScope::Request);
    assert_eq!(
        String::from_utf8_lossy(&too_big.body),
        r#"{"error":{"message":"upstream websocket message too big","type":"invalid_request_error","code":"message_too_big"}}"#
    );
    assert!(!should_retry_send(&too_big));
    let stale = pool_error(ExecError::local(
        502,
        FailureScope::Transport,
        "codex websockets executor: write failed",
    ));
    assert!(should_retry_send(&stale));
    assert_eq!(
        String::from_utf8_lossy(&stale.body),
        "xai websockets executor: write failed"
    );
}

/// TestXAIWebsocketPostCompactionAppendWithoutPreviousReplaysCompactedTranscript.
#[test]
fn post_compaction_append_replays_compacted_transcript() {
    let state = Arc::new(IdState::default());
    state.replace(&[br#"{"type":"compaction","encrypted_content":"compact-state"}"#]);
    state.map("resp-compact", "");
    let input = |body: &[u8]| -> Vec<String> {
        gj::get(body, "input")
            .array()
            .iter()
            .map(|i| String::from_utf8_lossy(i.raw()).into_owned())
            .collect()
    };

    let full = br#"{"type":"response.create","model":"grok-4.3","input":[{"type":"message","id":"msg-full"}]}"#;
    let out = IdMapper::new(state.clone(), full).upstream_request(full.to_vec());
    assert_eq!(
        input(&out).len(),
        1,
        "a self-contained response.create replayed the transcript"
    );

    let append = br#"{"type":"response.append","model":"grok-4.3","input":[{"type":"message","id":"msg-2","role":"user","content":"second"}]}"#;
    let got = IdMapper::new(state.clone(), append).upstream_request(append.to_vec());
    let items = input(&got);
    assert_eq!(items.len(), 2, "{}", String::from_utf8_lossy(&got));
    assert_eq!(gj::get(items[0].as_bytes(), "type").str(), "compaction");
    assert_eq!(gj::get(items[1].as_bytes(), "id").str(), "msg-2");

    state.record(
        &got,
        br#"{"type":"response.completed","response":{"id":"resp-after-compact","output":[{"type":"message","id":"out-2"}]}}"#,
        true,
    );
    let next = br#"{"type":"response.create","model":"grok-4.3","input":[{"type":"message","id":"msg-3"}]}"#;
    let out = IdMapper::new(state, next).upstream_request(next.to_vec());
    let items = input(&out);
    assert_eq!(
        items.len(),
        1,
        "replay not cleared after success: {}",
        String::from_utf8_lossy(&out)
    );
    assert_eq!(gj::get(items[0].as_bytes(), "id").str(), "msg-3");
}

/// TestXAIWebsocketPostCompactionWarmupPreservesTranscriptForLaterCompaction.
#[test]
fn post_compaction_warmup_preserves_transcript() {
    let state = Arc::new(IdState::default());
    state.replace(&[br#"{"type":"compaction","encrypted_content":"compact-state"}"#]);
    let warmup = br#"{"type":"response.append","model":"grok-4.3","generate":false,"input":[{"type":"message","id":"warmup-context"}]}"#;
    let mut mapper = IdMapper::new(state.clone(), warmup);
    let warmup_upstream = mapper.upstream_request(warmup.to_vec());
    assert!(
        mapper.replayed_compacted,
        "post-compaction warmup did not mark the replay"
    );
    state.record(
        &warmup_upstream,
        br#"{"type":"response.completed","response":{"id":"resp-warmup","output":[]}}"#,
        true,
    );

    let append =
        br#"{"type":"response.append","model":"grok-4.3","input":[{"type":"message","id":"msg-after-warmup"}]}"#;
    let append_upstream = IdMapper::new(state.clone(), append).upstream_request(append.to_vec());
    let input = gj::get(&append_upstream, "input");
    assert_eq!(input.array().len(), 1, "{}", String::from_utf8_lossy(&append_upstream));
    assert_eq!(input.get("0.id").str(), "msg-after-warmup");
    state.record(
        &append_upstream,
        br#"{"type":"response.completed","response":{"id":"resp-after-warmup","output":[{"type":"message","id":"out-after-warmup"}]}}"#,
        false,
    );

    let transcript = state.snapshot().unwrap();
    let kinds: Vec<String> = gj::parse(&transcript)
        .array()
        .iter()
        .map(|i| i.get("type").str().into_owned())
        .collect();
    assert_eq!(kinds, ["compaction", "message", "message", "message"]);
}

/// TestXAIWebsocketEmptyFullResetClearsPendingCompactionReplay.
#[test]
fn empty_full_reset_clears_pending_compaction_replay() {
    let state = Arc::new(IdState::default());
    state.replace(&[br#"{"type":"compaction","encrypted_content":"stale-compact-state"}"#]);
    state.record(
        br#"{"type":"response.create","model":"grok-4.3","input":[]}"#,
        br#"{"type":"response.completed","response":{"id":"resp-empty","output":[]}}"#,
        true,
    );
    let append = br#"{"type":"response.append","model":"grok-4.3","input":[{"type":"message","id":"msg-new"}]}"#;
    let got = IdMapper::new(state, append).upstream_request(append.to_vec());
    let input = gj::get(&got, "input");
    assert_eq!(input.array().len(), 1, "{}", String::from_utf8_lossy(&got));
    assert_eq!(input.get("0.id").str(), "msg-new");
}

/// TestValidateXAIWebsocketCompactionResponse.
#[test]
fn compaction_requires_string_response_identity_and_opaque_compacted_state() {
    let (id, item) = validate_compaction(
        br#"{"id":"resp_compact","output":[{"type":"compaction","encrypted_content":"opaque-state"}]}"#,
    )
    .unwrap();
    assert_eq!(id, "resp_compact");
    assert_eq!(gj::get(&item, "encrypted_content").str(), "opaque-state");
    for payload in [
        &b""[..],
        b"{}",
        br#"{"id":"resp_empty","output":[]}"#,
        br#"{"id":123,"output":[{"type":"compaction","encrypted_content":"opaque"}]}"#,
        br#"{"id":"resp_object","output":{"0":{"type":"compaction","encrypted_content":"opaque"}}}"#,
        br#"{"id":"resp_numeric_state","output":[{"type":"compaction","encrypted_content":123}]}"#,
        br#"{"id":"resp_missing_state","output":[{"type":"compaction"}]}"#,
    ] {
        assert!(
            validate_compaction(payload).is_err(),
            "{}",
            String::from_utf8_lossy(payload)
        );
    }
}

/// TestBuildXAIWebsocketRequestBodySetsStoreAndKeepsPromptCacheKey.
#[test]
fn request_frame_sets_store_and_keeps_prompt_cache_key() {
    let frame = request_frame(br#"{"model":"grok-4.3","stream":true,"stream_options":{"include_usage":true},"background":true,"prompt_cache_key":"cache-1","previous_response_id":"resp-prev","instructions":"system prompt","input":[{"type":"message","role":"user","content":"hello"}]}"#, |b| b);
    assert_eq!(gj::get(&frame, "type").str(), "response.create");
    for absent in ["stream", "stream_options", "background", "instructions"] {
        assert!(
            !gj::get(&frame, absent).exists(),
            "{absent}: {}",
            String::from_utf8_lossy(&frame)
        );
    }
    assert_eq!(gj::get(&frame, "prompt_cache_key").str(), "cache-1");
    assert!(gj::get(&frame, "store").bool());
}

/// Without a capture observer the turn's capture keeps no copy of the request frame.
#[test]
fn disabled_capture_keeps_no_frame() {
    struct Ignore;
    impl cpa_core::exec::CaptureObserver for Ignore {
        fn record(&self, _: cpa_core::exec::CaptureEvent<'_>, _: &cpa_core::exec::SentCredentials) {}
    }
    let turn = serde_json::json!({"payload": r#"{"model":"grok-4.3","input":"hi"}"#});
    let c = credential("xai-1", &serde_json::json!({"api_key": "sk-fake"}), "127.0.0.1:1");
    let frame = br#"{"type":"response.create","input":"hi"}"#;
    let headers = HeaderMap::new();
    let req = request(&turn, "s1", cpa_core::exec::UsageSink::default());
    let disabled = WsCapture::new(&req, &c, "ws://127.0.0.1:1/v1/responses", &headers, frame);
    assert!(disabled.frame.is_empty() && disabled.url.is_empty());
    let capture = cpa_core::exec::CaptureSink::new(Arc::new(Ignore));
    let req = request(&turn, "s1", cpa_core::exec::UsageSink::default().with_capture(capture));
    let enabled = WsCapture::new(&req, &c, "ws://127.0.0.1:1/v1/responses", &headers, frame);
    assert_eq!(enabled.frame, frame);
}

/// TestXAIWebsockets_PingHandlerDoesNotBlockOnWriteMu: an upstream ping is answered while
/// the socket's writer is held (Go holds `writeMu`; here the pooled socket's sink lock),
/// because the reader sends the pong.
#[tokio::test]
async fn ping_is_answered_while_the_writer_is_held() {
    let (pong_tx, mut pong_rx) = tokio::sync::mpsc::channel::<String>(1);
    let app = axum::Router::new().fallback(move |upgrade: WebSocketUpgrade| {
        let pong_tx = pong_tx.clone();
        async move {
            upgrade.on_upgrade(move |mut socket: AxSocket| async move {
                // The turn's request, then its completion.
                let _ = socket.recv().await;
                let completed = r#"{"type":"response.completed","response":{"id":"resp-1","output":[]}}"#;
                let _ = socket.send(AxMessage::text(completed)).await;
                // Ping once the test holds the writer.
                tokio::time::sleep(Duration::from_millis(300)).await;
                let _ = socket
                    .send(AxMessage::Ping(b"xai-keepalive-ping".to_vec().into()))
                    .await;
                while let Some(Ok(message)) = socket.recv().await {
                    if let AxMessage::Pong(data) = message {
                        let _ = pong_tx.send(String::from_utf8_lossy(&data).into_owned()).await;
                    }
                }
            })
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let attrs = serde_json::json!({"api_key": "xai-key", "base_url": "http://UPSTREAM/v1", "websockets": "true"});
    let credential = credential("xai-ping", &attrs, &addr);
    let executor = XaiExecutor::with_client(wreq::Client::new());
    let session = ExecSession {
        id: "test-xai-keepalive".into(),
        continuation: false,
        lease: None,
    };
    let turn = serde_json::json!({"payload": r#"{"model":"grok-4","input":"ping"}"#});
    let response = executor
        .execute_in_session(
            &credential,
            request(&turn, "test-xai-keepalive", Default::default()),
            &Config::default(),
            &session,
        )
        .await
        .unwrap();
    let ResponseBody::Stream(mut stream) = response.body else {
        panic!("buffered response");
    };
    while stream.next().await.is_some() {}
    drop(stream);
    let conn = executor
        .ws
        .pool
        .session("test-xai-keepalive")
        .current()
        .expect("pooled socket");
    let _writer = conn.sink.lock().await;
    let pong = tokio::time::timeout(Duration::from_secs(2), pong_rx.recv())
        .await
        .expect("pong starved by the held writer");
    assert_eq!(pong.as_deref(), Some("xai-keepalive-ping"));
    executor.close_session("test-xai-keepalive");
    server.abort();
}

/// A loopback xAI upstream answering every frame with a completed `resp-1`, so a repeated
/// ID gets a `-xai-N` downstream suffix.
async fn repeating_upstream() -> (Shared, String, tokio::task::JoinHandle<()>) {
    let completed = r#"{"type":"response.completed","response":{"id":"resp-1","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#;
    let shared: Shared = Arc::default();
    let acts: Vec<Value> = (0..8).map(|_| serde_json::json!({"send": [completed]})).collect();
    shared.lock().unwrap().turn = Some(serde_json::json!({ "acts": acts }));
    let app = axum::Router::new().fallback(handler).with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (shared, addr, server)
}

/// One turn on `session` with `previous_response_id` = `previous` (none when empty);
/// returns the downstream chunks.
async fn turn_with(
    executor: &XaiExecutor,
    credential: &Credential,
    session: &str,
    previous: &str,
    cfg: &Config,
    continuation: bool,
) -> Result<Vec<String>, ExecError> {
    let payload = if previous.is_empty() {
        r#"{"model":"grok-4.3","input":[{"type":"message","role":"user","content":"hi"}]}"#.to_owned()
    } else {
        format!(
            r#"{{"model":"grok-4.3","previous_response_id":"{previous}","input":[{{"type":"message","role":"user","content":"hi"}}]}}"#
        )
    };
    let req = request(&serde_json::json!({ "payload": payload }), session, Default::default());
    let exec_session = ExecSession {
        id: session.to_owned(),
        continuation,
        lease: None,
    };
    let response = executor.execute_in_session(credential, req, cfg, &exec_session).await?;
    let ResponseBody::Stream(stream) = response.body else {
        panic!("buffered response");
    };
    let chunks = stream
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    Ok(chunks.iter().map(|c| String::from_utf8_lossy(c).into_owned()).collect())
}

fn upstream_previous(shared: &Shared) -> Vec<String> {
    let script = shared.lock().unwrap();
    script
        .recorded
        .frames
        .iter()
        .map(|f| crate::rawjson::get(f, "previous_response_id").str().to_owned())
        .collect()
}

/// Go 1e37b52b for xAI: after a reload that changes only `requests.proxy-url`, the open
/// socket keeps serving its credential and the session's upstream response IDs (a
/// downstream `resp-1-xai-1` still reaches upstream as `resp-1`), while a new session
/// dials through the new proxy.
#[tokio::test]
async fn a_global_proxy_reload_keeps_the_open_socket() {
    let (shared, addr, server) = repeating_upstream().await;
    let executor = XaiExecutor::with_client(wreq::Client::new());
    let attrs = serde_json::json!({"api_key": "xai-key", "websockets": "true", "base_url": "http://UPSTREAM/v1"});
    let credential = credential("xai-reload", &attrs, &addr);
    // Nothing listens on port 1: a dial through this proxy fails.
    let reloaded = Config::parse("requests:\n  proxy-url: http://127.0.0.1:1\n").unwrap();
    let session = "xai-reload-session";
    let default = Config::default();
    turn_with(&executor, &credential, session, "", &default, false)
        .await
        .unwrap();
    let second = turn_with(&executor, &credential, session, "resp-1", &default, true)
        .await
        .unwrap();
    assert!(
        second.iter().any(|c| c.contains(r#""id":"resp-1-xai-1""#)),
        "{second:?}"
    );
    for continuation in [true, false] {
        turn_with(&executor, &credential, session, "resp-1-xai-1", &reloaded, continuation)
            .await
            .unwrap_or_else(|e| panic!("continuation={continuation}: {e:?}"));
    }
    assert_eq!(
        shared.lock().unwrap().recorded.upgrades.len(),
        1,
        "every turn kept the first socket"
    );
    assert_eq!(
        upstream_previous(&shared),
        ["", "resp-1", "resp-1", "resp-1"],
        "the upstream ID mapping survived"
    );
    let error = turn_with(&executor, &credential, "xai-reload-new", "", &reloaded, false)
        .await
        .expect_err("a new socket dials through the reloaded proxy");
    assert_eq!(error.scope, FailureScope::Transport, "{error}");
    assert_eq!(shared.lock().unwrap().recorded.upgrades.len(), 1);
    executor.close_session(session);
    server.abort();
}

/// A credential removal drops a session and its ID state together, and a turn takes the
/// ID state only after it holds its session (Go `closeXAIWebsocketSessionsInStoreForAuthID`
/// and `ExecuteStream`). A turn that waits on its session while the removal runs keeps
/// its mapping in the live state, so the next turn still sends upstream `resp-1` for
/// the `resp-1-xai-1` that turn returned.
#[tokio::test]
async fn a_removal_during_a_turn_keeps_the_replacement_sessions_ids() {
    let (shared, addr, server) = repeating_upstream().await;
    let executor = Arc::new(XaiExecutor::with_client(wreq::Client::new()));
    let attrs = serde_json::json!({"api_key": "xai-key", "websockets": "true", "base_url": "http://UPSTREAM/v1"});
    let credential = credential("xai-removed", &attrs, &addr);
    let (session, cfg) = ("xai-race-session", Config::default());
    turn_with(&executor, &credential, session, "", &cfg, false)
        .await
        .unwrap();

    let (reached_tx, reached) = tokio::sync::oneshot::channel();
    let (resume, resume_rx) = tokio::sync::oneshot::channel();
    *executor.ws.pause.lock().unwrap() = Some((reached_tx, resume_rx));
    let racing = {
        let (executor, credential, cfg) = (executor.clone(), credential.clone(), cfg.clone());
        tokio::spawn(async move { turn_with(&executor, &credential, session, "resp-1", &cfg, false).await })
    };
    reached.await.expect("the turn holds its session");
    executor.ws.close_credential("xai-removed");
    resume.send(()).unwrap();
    let chunks = racing.await.unwrap().unwrap();
    assert!(
        chunks.iter().any(|c| c.contains(r#""id":"resp-1-xai-1""#)),
        "{chunks:?}"
    );

    turn_with(&executor, &credential, session, "resp-1-xai-1", &cfg, false)
        .await
        .unwrap();
    assert_eq!(upstream_previous(&shared), ["", "resp-1", "resp-1"]);
    executor.close_session(session);
    server.abort();
}

/// TestPayloadBarrierXAIWebsocketRetry (868e137): the rules run on the WebSocket frame,
/// after `store=true` and the instructions drop, and only the frame type is set after
/// them; a retry builds the same frame.
#[test]
fn request_frame_runs_the_payload_rules_last() {
    let cfg = Config::parse(
        r#"
payload:
  override:
    - models: [{name: "*"}]
      params: {store: false, instructions: configured}
  filter:
    - models: [{name: "*"}]
      params: ["input.0", "previous_response_id", "type"]
"#,
    )
    .unwrap();
    let body = br#"{"previous_response_id":"previous","input":[{"content":"first"},{"content":"second"}]}"#;
    let req = request(
        &serde_json::json!({"payload": std::str::from_utf8(body).unwrap()}),
        "s",
        Default::default(),
    );
    let finalize = |b| {
        crate::payload_barrier::Barrier {
            executor: crate::xai::PROVIDER,
            model: "grok-4.3",
            protocol: "codex",
            root: "",
            original: body,
        }
        .apply(&cfg, &req, b)
    };
    let first = request_frame(body, finalize);
    assert_eq!(first, request_frame(body, finalize));
    let text = String::from_utf8_lossy(&first);
    assert!(!gj::get(&first, "store").bool(), "{text}");
    assert_eq!(gj::get(&first, "instructions").str(), "configured", "{text}");
    assert!(!gj::get(&first, "previous_response_id").exists(), "{text}");
    assert_eq!(gj::get(&first, "input.#").int(), 1, "{text}");
    assert_eq!(gj::get(&first, "type").str(), "response.create", "{text}");
}

/// #178: a WebSocket turn marks TTFT at the first upstream message with output
/// (`helpers::responses_token_event`), not at its first message: `response.created` and
/// an empty delta are frames only.
#[tokio::test]
async fn ttft_is_marked_at_the_first_output_message() {
    let shared: Shared = Arc::default();
    let app = axum::Router::new().fallback(handler).with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let created = r#"{"type":"response.created","response":{"id":"r1","status":"in_progress","output":[]}}"#;
    let empty = r#"{"type":"response.output_text.delta","item_id":"m","output_index":0,"content_index":0,"delta":""}"#;
    let delta =
        r#"{"type":"response.output_text.delta","item_id":"m","output_index":0,"content_index":0,"delta":"hi"}"#;
    let done = r#"{"type":"response.completed","response":{"id":"r1","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#;
    let turn = serde_json::json!({
        "auth": "auth-a",
        "payload": r#"{"model":"grok-4.3","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],"stream":true}"#,
        "acts": [{"send": [created, empty, delta, done]}],
    });
    shared.lock().unwrap().turn = Some(turn.clone());
    let attrs = serde_json::json!({"api_key": "sk-fake", "base_url": "http://UPSTREAM/v1", "websockets": "true"});
    let credential = credential("auth-a", &attrs, &addr);
    let executor = XaiExecutor::with_client(wreq::Client::new());
    let session = ExecSession {
        id: "sess-ttft".into(),
        continuation: false,
        lease: None,
    };
    let (recorder, _capture, usage) = crate::usage_testkit::sinks();
    let response = executor
        .execute_in_session(
            &credential,
            request(&turn, "sess-ttft", usage),
            &Config::default(),
            &session,
        )
        .await
        .unwrap();
    let ResponseBody::Stream(mut stream) = response.body else {
        panic!("buffered response");
    };
    while let Some(item) = stream.next().await {
        item.unwrap();
    }
    let marks: Vec<&str> = recorder
        .0
        .lock()
        .unwrap()
        .iter()
        .filter_map(|r| match r {
            crate::usage_testkit::Report::RoundTrip => Some("start"),
            crate::usage_testkit::Report::FirstByte => Some("byte"),
            crate::usage_testkit::Report::TokenEvent(false) => Some("frame"),
            crate::usage_testkit::Report::TokenEvent(true) => Some("token"),
            _ => None,
        })
        .collect();
    assert_eq!(marks, ["start", "frame", "frame", "token", "token"]);
    executor.close_session("sess-ttft");
    server.abort();
}
