//! Native Responses WebSocket transcripts against local scripted peers.
//! Historical Go recordings supply only client messages and mock reply stimuli;
//! `contract_scenario` specifies expected relay/validation/lifecycle behavior from
//! docs/codex/CONTRACT.md. Recorded output frames and upstream captures are discarded.
//! Credentials are fake and the runtime uses a private auth directory.

use std::collections::VecDeque;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_server::router;
use futures_util::StreamExt;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use wreq::ws::message::Message;

static FIXTURE: LazyLock<Vec<Value>> = LazyLock::new(|| {
    serde_json::from_str::<Value>(include_str!("ws_fixtures/ws_inputs.json")).unwrap()["scenarios"]
        .as_array()
        .unwrap()
        .clone()
});

#[derive(Debug, Clone, PartialEq)]
struct Captured {
    kind: String,
    headers: Vec<(String, String)>,
    body: String,
    /// The exact bytes, for Go's `body_hex`.
    raw: Vec<u8>,
}

#[derive(Default)]
struct Upstream {
    script: Mutex<VecDeque<Value>>,
    captured: Mutex<Vec<Captured>>,
    /// Held SSE responses whose request the proxy canceled.
    canceled: Mutex<u64>,
}

/// Counts a held SSE response as canceled once the server drops its body.
struct Canceled(Arc<Upstream>);

impl Drop for Canceled {
    fn drop(&mut self) {
        *self.0.canceled.lock().unwrap() += 1;
    }
}

fn header_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or_default().to_owned()))
        .collect()
}

async fn upstream_handler(State(up): State<Arc<Upstream>>, mut req: axum::extract::Request) -> Response {
    let headers = req.headers().clone();
    let key = headers.get("sec-websocket-key").filter(|_| {
        headers
            .get("upgrade")
            .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"))
    });
    if let Some(key) = key {
        up.captured.lock().unwrap().push(Captured {
            kind: "ws_dial".into(),
            headers: header_pairs(&headers),
            body: String::new(),
            raw: Vec::new(),
        });
        let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
        let upgrade = hyper::upgrade::on(&mut req);
        tokio::spawn(async move {
            if let Ok(upgraded) = upgrade.await {
                upstream_socket(up, hyper_util::rt::TokioIo::new(upgraded)).await;
            }
        });
        return Response::builder()
            .status(StatusCode::SWITCHING_PROTOCOLS)
            .header("upgrade", "websocket")
            .header("connection", "Upgrade")
            .header("sec-websocket-accept", accept)
            .body(axum::body::Body::empty())
            .unwrap();
    }
    let body = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap();
    up.captured.lock().unwrap().push(Captured {
        kind: "http".into(),
        headers: header_pairs(&headers),
        body: String::from_utf8_lossy(&body).into_owned(),
        raw: body.to_vec(),
    });
    let Some(reply) = up.script.lock().unwrap().pop_front() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let status = reply["status"].as_u64().unwrap_or(0) as u16;
    if status != 0 {
        return (
            StatusCode::from_u16(status).unwrap(),
            [("content-type", "application/json")],
            reply["body"].as_str().unwrap_or_default().to_owned(),
        )
            .into_response();
    }
    let mut sse = String::new();
    for event in reply["events"].as_array().into_iter().flatten() {
        let event = event.as_str().unwrap();
        let kind = cpa_common::rawjson::get(event, "type");
        sse.push_str(&format!("event: {}\ndata: {event}\n\n", kind.str()));
    }
    if reply["then"] == "hold" {
        // The events, then an open stream until the proxy cancels the request.
        let guard = Canceled(up.clone());
        let body = futures_util::stream::once(async move { Ok::<_, std::io::Error>(Bytes::from(sse)) })
            .chain(futures_util::stream::pending())
            .map(move |item| {
                let _ = &guard;
                item
            });
        return (
            [("content-type", "text/event-stream")],
            axum::body::Body::from_stream(body),
        )
            .into_response();
    }
    ([("content-type", "text/event-stream")], sse).into_response()
}

/// One frame from the proxy (a client, so masked): opcode and unmasked payload. The
/// mock reads frames itself, like gorilla, so a text frame need not be UTF-8.
async fn read_frame<S: AsyncRead + Unpin>(io: &mut S) -> Option<(u8, Vec<u8>)> {
    let mut head = [0u8; 2];
    io.read_exact(&mut head).await.ok()?;
    let len = match head[1] & 0x7f {
        126 => u64::from(io.read_u16().await.ok()?),
        127 => io.read_u64().await.ok()?,
        n => u64::from(n),
    };
    let mut mask = [0u8; 4];
    if head[1] & 0x80 != 0 {
        io.read_exact(&mut mask).await.ok()?;
    }
    let mut payload = vec![0u8; usize::try_from(len).ok()?];
    io.read_exact(&mut payload).await.ok()?;
    for (i, b) in payload.iter_mut().enumerate() {
        *b ^= mask[i % 4];
    }
    Some((head[0] & 0x0f, payload))
}

/// One unmasked, final server frame.
async fn write_frame<S: AsyncWrite + Unpin>(io: &mut S, opcode: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut frame = vec![0x80 | opcode];
    match payload.len() {
        n if n < 126 => frame.push(n as u8),
        n if n <= 0xffff => {
            frame.push(126);
            frame.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            frame.push(127);
            frame.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    frame.extend_from_slice(payload);
    io.write_all(&frame).await?;
    io.flush().await
}

async fn upstream_socket<S: AsyncRead + AsyncWrite + Unpin>(up: Arc<Upstream>, mut socket: S) {
    while let Some((opcode, payload)) = read_frame(&mut socket).await {
        match opcode {
            0x1 => {}
            0x8 => {
                let _ = write_frame(&mut socket, 0x8, &payload).await;
                return;
            }
            0x9 => {
                let _ = write_frame(&mut socket, 0xa, &payload).await;
                continue;
            }
            _ => continue,
        }
        up.captured.lock().unwrap().push(Captured {
            kind: "ws_frame".into(),
            headers: Vec::new(),
            body: String::from_utf8_lossy(&payload).into_owned(),
            raw: payload,
        });
        let Some(reply) = up.script.lock().unwrap().pop_front() else {
            continue;
        };
        for event in reply["events"].as_array().into_iter().flatten() {
            let _ = write_frame(&mut socket, 0x1, event.as_str().unwrap().as_bytes()).await;
        }
        let then = reply["then"].as_str().unwrap_or_default();
        // `*_now` ends the socket right after the events, while the proxy may still be
        // delivering them; the others wait first.
        if !then.is_empty() && !then.ends_with("_now") {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        let close = |code: u16, reason: &str| [&code.to_be_bytes()[..], reason.as_bytes()].concat();
        match then {
            "close" | "close_now" => {
                let _ = write_frame(&mut socket, 0x8, &close(1000, "bye")).await;
                return;
            }
            "close1009" | "close1009_now" => {
                let _ = write_frame(&mut socket, 0x8, &close(1009, "too big")).await;
                return;
            }
            "close1009_spaced" => {
                let _ = write_frame(&mut socket, 0x8, &close(1009, " oversized ")).await;
                return;
            }
            "drop" | "drop_now" => return,
            _ => {}
        }
    }
}

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("127.0.0.1:{}", addr.port())
}

/// Go's API-key credentials (those without `metadata`).
fn api_keys(scenario: &Value) -> Vec<&Value> {
    scenario["credentials"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["metadata"].is_null())
        .collect()
}

/// Go's OAuth credentials (`metadata` holds the auth file's fields) as Codex auth files:
/// the Pro plan catalog serves their models, as Go registers them.
fn oauth_credentials(scenario: &Value, upstream: &str) -> Vec<cpa_core::credential::Credential> {
    let creds = scenario["credentials"].as_array().unwrap();
    creds
        .iter()
        .filter(|c| !c["metadata"].is_null())
        .map(|cred| {
            let id = cred["id"].as_str().unwrap();
            let mut metadata = cred["metadata"].as_object().unwrap().clone();
            metadata.insert("type".into(), "codex".into());
            let path = std::path::PathBuf::from(format!("/fake/{id}"));
            let mut credential =
                cpa_core::credential::Credential::from_file(std::path::Path::new("/fake"), &path, metadata).unwrap();
            credential
                .attributes
                .insert("base_url".into(), format!("http://{upstream}"));
            for (k, v) in cred["attributes"].as_object().unwrap() {
                credential.attributes.insert(k.clone(), v.as_str().unwrap().to_owned());
            }
            credential
        })
        .collect()
}

/// Go's API-key credentials as `codex-api-key` config entries, loaded the way the server
/// loads them (`cpa_core::config::credentials::load`).
fn config(scenario: &Value, upstream: &str) -> Config {
    // The scenario's own Go config (top-level keys), then its credentials.
    let mut yaml = scenario["config"].as_str().unwrap_or_default().to_owned();
    let keys = api_keys(scenario);
    if !keys.is_empty() {
        yaml.push_str("codex-api-key:\n");
    }
    for cred in keys {
        let attrs = &cred["attributes"];
        yaml.push_str(&format!(
            "  - api-key: {}\n    base-url: http://{upstream}\n",
            attrs["api_key"].as_str().unwrap()
        ));
        if attrs["websockets"].as_str() == Some("true") {
            yaml.push_str("    websockets: true\n");
        }
        yaml.push_str("    models:\n");
        for model in cred["models"].as_array().unwrap() {
            yaml.push_str(&format!("      - name: {}\n", model.as_str().unwrap()));
        }
    }
    Config::parse(&yaml).unwrap()
}

#[derive(Debug, PartialEq)]
enum Frame {
    Text(String),
    Close(u16, String),
    Semantic(Value),
    LocalError(Value),
    Interrupted(Value),
}

fn expected_frames(step: &Value) -> Vec<Frame> {
    step["frames"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|f| {
            if let Some(text) = f["text"].as_str() {
                Frame::Text(text.to_owned())
            } else if f["semantic"].is_object() {
                Frame::Semantic(f["semantic"].clone())
            } else if f["local_error"].is_object() {
                Frame::LocalError(f["local_error"].clone())
            } else if f["interrupted"].is_object() {
                Frame::Interrupted(f["interrupted"].clone())
            } else {
                Frame::Close(
                    f["close"].as_u64().unwrap() as u16,
                    f["reason"].as_str().unwrap_or_default().to_owned(),
                )
            }
        })
        .collect()
}

fn frames_match(got: &[Frame], expected: &[Frame]) -> bool {
    got.len() == expected.len()
        && got.iter().zip(expected).all(|(got, expected)| match (got, expected) {
            (Frame::Text(raw), Frame::Semantic(want)) => serde_json::from_str::<Value>(raw).ok().as_ref() == Some(want),
            (Frame::Text(raw), Frame::LocalError(want)) => {
                let Ok(event) = serde_json::from_str::<Value>(raw) else {
                    return false;
                };
                event["type"] == "error"
                    && event["status"] == want["status"]
                    && event["error"]["type"] == "invalid_request_error"
                    && event["error"]["message"]
                        .as_str()
                        .is_some_and(|m| m.contains(want["hint"].as_str().unwrap()))
                    && (!want["code"].is_string() || event["error"]["code"] == want["code"])
            }
            (Frame::Text(raw), Frame::Interrupted(want)) => {
                let Ok(event) = serde_json::from_str::<Value>(raw) else {
                    return false;
                };
                event["type"] == "response.incomplete"
                    && event["response"]["id"] == want["id"]
                    && event["response"]["status"] == "incomplete"
                    && event["response"]["incomplete_details"]["reason"] == "interrupted"
                    && event["response"]["output"] == want["output"]
            }
            _ => got == expected,
        })
}

async fn run(name: &str) {
    run_with(name, |_| {}).await;
}

/// Run a historical input scenario after adapting its client/peer stimuli to the
/// current supported contract. Expected outputs are then derived independently.
async fn run_with(name: &str, differ: impl FnOnce(&mut Value)) {
    let mut scenario = FIXTURE
        .iter()
        .find(|s| s["name"] == name)
        .unwrap_or_else(|| panic!("no scenario {name}"))
        .clone();
    differ(&mut scenario);
    contract_scenario(name, &mut scenario);
    run_scenario(name, &scenario).await;
}

/// Independent transcript oracle for the native route. Historical fixtures supply
/// client messages and scripted peer replies only. CONTRACT.md defines byte-preserving
/// relay, connection-local continuation, local validation, and credential isolation.
/// No recorded client frame, captured request, close text, or header is consulted.
fn contract_scenario(name: &str, scenario: &mut Value) {
    use serde_json::json;
    let mut pending = VecDeque::<Value>::new();
    let mut requests = Vec::new();
    let mut bound = false;
    let mut model = String::new();
    let websocket = scenario["credentials"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["attributes"]["websockets"] == "true");
    let mut last_id = String::new();
    let mut output_items = Vec::new();
    for (index, step) in scenario["steps"].as_array_mut().unwrap().iter_mut().enumerate() {
        let sent = step["send"]
            .as_str()
            .unwrap_or_default()
            .replace("{{last_response_id}}", &last_id);
        let parsed: Option<Value> = serde_json::from_str(&sent).ok();
        let local = match (name, index) {
            ("previous_not_found_then_create", 0) => {
                Some((409, "previous_response_id", Some("previous_response_not_found")))
            }
            ("previous_not_found_then_create", 1)
            | ("steer_local_validation", 2)
            | ("steer_rejected_when_disabled", 1 | 2) => Some((400, "unsupported websocket request type", None)),
            ("prewarm_then_followup", 0) => Some((400, "prewarm", None)),
            ("http_incremental", 2) | ("steer_append_inherits", 1) => Some((400, "response.append", None)),
            ("steer_local_validation", 1) => Some((400, "JSON", None)),
            ("steer_wrong_parent_rejected", 2) => Some((400, "continue", None)),
            ("interrupt_in_flight" | "interrupt_in_flight_steering", 0 | 3) => Some((400, "response_id", None)),
            ("interrupt_in_flight" | "interrupt_in_flight_steering", 1) | ("interrupt_late_ws", 1) => {
                Some((400, "no active", None))
            }
            ("interrupt_in_flight" | "interrupt_in_flight_steering", 4) => Some((400, "enabled", None)),
            _ => None,
        };
        if let Some((status, hint, code)) = local {
            pending.push_back(json!({"local_error":{"status":status,"hint":hint,"code":code}}));
        }
        let replay = matches!(
            (name, index),
            ("ws_continuation_needs_replay" | "steer_model_switch_needs_replay", 1) | ("interrupt_http_local", 3)
        );
        let disabled = name == "steer_credential_disabled" && index == 1;
        let local_interrupt = name == "interrupt_http_local" && (index == 1 || index == 2);
        let stale_interrupt = name == "interrupt_late_ws" && (index == 4 || index == 5);
        let forwarded = local.is_none() && !replay && !disabled && !local_interrupt && !stale_interrupt;
        if forwarded && (!sent.is_empty() || step["send_hex"].is_string()) {
            let kind = parsed.as_ref().and_then(|p| p["type"].as_str()).unwrap_or("binary");
            if kind == "response.create" {
                if let Some(next) = parsed.as_ref().and_then(|p| p["model"].as_str()) {
                    model = next.to_owned();
                }
                if websocket {
                    if !bound {
                        requests.push(json!({"kind":"ws_dial"}));
                        bound = true;
                    }
                    requests.push(json!({"kind":"ws_frame","body":native_frame(&sent,&model)}));
                } else {
                    requests.push(json!({"kind":"http","body":http_body(&sent,&model)}));
                }
            } else if let Some(hex) = step["send_hex"].as_str() {
                requests.push(json!({"kind":"ws_frame","body_hex":hex}));
            } else {
                requests.push(json!({"kind":"ws_frame","body":sent}));
            }
        }
        for reply in step["upstream"].as_array().into_iter().flatten() {
            for event in reply["events"].as_array().into_iter().flatten() {
                let raw = event.as_str().unwrap();
                let event: Value = serde_json::from_str(raw).unwrap();
                if event["type"] == "response.output_item.done" {
                    output_items.push(event["item"].clone());
                }
                let account_failure = matches!(
                    event["status"].as_u64().or_else(|| event["error"]["status"].as_u64()),
                    Some(401 | 403 | 429)
                ) || matches!(
                    event["error"]["type"].as_str(),
                    Some("authentication_error" | "rate_limit_error" | "usage_limit_reached")
                ) || matches!((name, index), ("steer_later_quota_failure", 1));
                if !account_failure {
                    pending.push_back(json!({"text":raw}));
                }
            }
            if reply["status"].as_u64().is_some_and(|s| s >= 400) {
                let body: Value = serde_json::from_str(reply["body"].as_str().unwrap()).unwrap();
                pending.push_back(json!({"semantic": {"type":"error","status":reply["status"],"error":body["error"]}}));
            }
            match reply["then"].as_str().unwrap_or_default() {
                "close1009" | "close1009_now" => pending.push_back(json!({"close":1009,"reason":"too big"})),
                "close1009_spaced" => pending.push_back(json!({"close":1009,"reason":" oversized "})),
                "close" | "close_now" | "drop" | "drop_now" => {
                    pending.push_back(json!({"close":1006,"reason":"unexpected EOF"}))
                }
                _ => {}
            }
        }
        if local_interrupt {
            pending.push_back(json!({"text":r#"{"type":"response.interrupt.accepted","response_id":"r-http"}"#}));
            if index == 1 {
                // HTTP cancellation preserves already delivered output, with an explicit
                // interrupted terminal event. These fields are a local lifecycle contract.
                pending.push_back(json!({"interrupted":{"id":"r-http","output":output_items}}));
            }
        }
        if replay {
            pending.push_back(json!({"close":1012,"reason":"upstream requires HTTP replay"}));
        }
        let mode = step["read"].as_str().unwrap();
        if mode == "close" && !pending.iter().any(|f| f["close"].is_number()) {
            pending.push_back(json!({"close":1006,"reason":"unexpected EOF"}));
        }
        let mut frames = Vec::new();
        if mode != "none" {
            while let Some(frame) = pending.pop_front() {
                let event: Option<Value> = frame["text"].as_str().and_then(|s| serde_json::from_str(s).ok());
                let kind =
                    event
                        .as_ref()
                        .and_then(|e| e["type"].as_str())
                        .unwrap_or(if frame["local_error"].is_object() {
                            "error"
                        } else if frame["interrupted"].is_object() {
                            "response.incomplete"
                        } else {
                            ""
                        });
                if matches!(kind, "response.completed" | "response.done") {
                    last_id = event
                        .as_ref()
                        .and_then(|e| e["response"]["id"].as_str())
                        .unwrap()
                        .to_owned();
                }
                let until = mode
                    .strip_prefix("until:")
                    .map(|v| v.split_once('@').unwrap_or((v, "")));
                let end = frame["close"].is_number()
                    || mode == "one"
                    || (mode == "completed"
                        && matches!(kind, "response.completed" | "response.done" | "response.incomplete"))
                    || until.is_some_and(|(k, id)| {
                        kind == k
                            && (id.is_empty() || event.as_ref().and_then(|e| e["response"]["id"].as_str()) == Some(id))
                    });
                frames.push(frame);
                if end {
                    break;
                }
            }
        }
        step["frames"] = json!(frames);
    }
    if name == "steer_queued_create_then_steer" {
        // The third create waits behind the automatic successor. A later steer must
        // overtake it so the active response can finish and release that create.
        requests.swap(3, 4);
    }
    scenario["upstream"] = json!(requests);
    scenario["http_canceled"] = json!(if name == "interrupt_http_local" { 1 } else { 0 });
    scenario["upgrade_turn_state"] = scenario["client_headers"]["x-codex-turn-state"].clone();
}

/// The frame a native `response.create` sends upstream for the client's `sent`: the same
/// bytes with only `model` routed to `model`.
fn native_frame(sent: &str, model: &str) -> String {
    let members: std::collections::BTreeMap<String, &serde_json::value::RawValue> = serde_json::from_str(sent).unwrap();
    if members
        .get("model")
        .and_then(|m| serde_json::from_str::<String>(m.get()).ok())
        .as_deref()
        == Some(model)
    {
        return sent.to_owned();
    }
    edit_member(sent, "model", Some(&serde_json::to_string(model).unwrap()))
}

/// Independent span edits use serde's borrowed RawValue positions, not the proxy's
/// JSON editor. Every byte outside the permitted top-level members stays in place.
fn edit_member(raw: &str, key: &str, replacement: Option<&str>) -> String {
    let members: std::collections::BTreeMap<String, &serde_json::value::RawValue> = serde_json::from_str(raw).unwrap();
    let Some(member) = members.get(key) else {
        let Some(replacement) = replacement else {
            return raw.to_owned();
        };
        let start = raw.find('{').unwrap() + 1;
        let comma = if members.is_empty() { "" } else { "," };
        return format!(
            "{}{}:{replacement}{comma}{}",
            &raw[..start],
            serde_json::to_string(key).unwrap(),
            &raw[start..]
        );
    };
    let start = member.get().as_ptr() as usize - raw.as_ptr() as usize;
    let end = start + member.get().len();
    if let Some(replacement) = replacement {
        return format!("{}{replacement}{}", &raw[..start], &raw[end..]);
    }
    // In these hand-written frames the routing keys are unescaped ASCII. Locate the
    // member's quoted key and its separating comma outside the already parsed value.
    let colon = raw[..start].rfind(':').unwrap();
    let key_end = raw[..colon].trim_end().len();
    let key_start = raw[..key_end - 1].rfind('"').unwrap();
    let following = raw[end..].trim_start();
    if following.starts_with(',') {
        let after_comma = end + raw[end..].find(',').unwrap() + 1;
        format!("{}{}", &raw[..key_start], &raw[after_comma..])
    } else {
        let prefix = raw[..key_start].trim_end();
        let before = prefix.strip_suffix(',').unwrap_or(prefix);
        format!("{before}{}", &raw[end..])
    }
}

/// The fallback HTTP envelope removes the create type and enables SSE. Routing a
/// model is the only other permitted edit (CONTRACT.md, WebSocket HTTP fallback).
fn http_body(sent: &str, model: &str) -> String {
    let routed = native_frame(sent, model);
    let untagged = edit_member(&routed, "type", None);
    let members: std::collections::BTreeMap<String, &serde_json::value::RawValue> =
        serde_json::from_str(&untagged).unwrap();
    if members.get("stream").is_some_and(|v| v.get() == "true") {
        untagged
    } else {
        edit_member(&untagged, "stream", Some("true"))
    }
}

/// Assert identity from client/config input, without a captured-header golden.
fn assert_native_headers(name: &str, scenario: &Value, captured: &Captured) {
    let header = |key: &str| {
        captured
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    };
    let client = |key: &str| {
        scenario["client_headers"]
            .as_object()
            .into_iter()
            .flatten()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .and_then(|(_, v)| v.as_str())
    };
    let credentials = scenario["credentials"].as_array().unwrap();
    let credential = credentials.first().unwrap();
    let token = credential["attributes"]["api_key"]
        .as_str()
        .or_else(|| credential["metadata"]["access_token"].as_str())
        .unwrap();
    assert_eq!(
        header("authorization"),
        Some(format!("Bearer {token}").as_str()),
        "{name}"
    );
    assert_eq!(
        header("user-agent"),
        Some(client("user-agent").unwrap_or(concat!("cliproxy-rs/", env!("CARGO_PKG_VERSION")))),
        "{name}"
    );
    let oauth = credential["metadata"]["access_token"].is_string();
    assert_eq!(
        header("originator"),
        client("originator").or(oauth.then_some("cliproxy-rs")),
        "{name}"
    );
    if let Some(turn_state) = client("x-codex-turn-state") {
        assert_eq!(header("x-codex-turn-state"), Some(turn_state), "{name}");
    }
    if captured.kind == "ws_dial" {
        assert!(
            header("openai-beta").is_some_and(|v| v.contains("responses_websockets=")),
            "{name}"
        );
        let session = header("session_id").expect("upstream socket has session identity");
        assert!(!session.is_empty());
        assert_eq!(
            header("conversation_id"),
            Some(session),
            "{name}: consistent connection identity"
        );
    } else {
        assert_eq!(header("content-type"), Some("application/json"), "{name}");
        assert_eq!(header("accept"), Some("text/event-stream"), "{name}");
    }
}

/// Runs one scenario and returns its runtime for checks after disconnect.
async fn run_scenario(name: &str, scenario: &Value) -> Arc<cpa_server::runtime::Runtime> {
    let up = Arc::new(Upstream::default());
    for step in scenario["steps"].as_array().unwrap() {
        for reply in step["upstream"].as_array().into_iter().flatten() {
            up.script.lock().unwrap().push_back(reply.clone());
        }
    }
    let upstream = serve(axum::Router::new().fallback(upstream_handler).with_state(up.clone())).await;
    let cfg = config(scenario, &upstream);
    let mut credentials = cpa_core::config::credentials::from_config(&cfg);
    assert_eq!(credentials.len(), api_keys(scenario).len());
    // Synthesis carries each key's name-only `models` entries, as Go's registry reads them.
    for (credential, go) in credentials.iter().zip(api_keys(scenario)) {
        let models: Vec<Value> = go["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| serde_json::json!({ "name": m }))
            .collect();
        assert_eq!(credential.metadata["models"], Value::Array(models));
    }
    credentials.extend(oauth_credentials(scenario, &upstream));
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        meta: Default::default(),
        xai: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(cfg, credentials, executors));
    let proxy = serve(router(rt.clone())).await;

    let mut request = wreq::Client::new().websocket(format!("ws://{proxy}/v1/responses"));
    for (k, v) in scenario["client_headers"].as_object().into_iter().flatten() {
        request = request.header(k.as_str(), v.as_str().unwrap());
    }
    let response = request.send().await.unwrap();
    let turn_state = response
        .headers()
        .get("x-codex-turn-state")
        .map(|v| v.to_str().unwrap().to_owned());
    assert_eq!(
        turn_state.as_deref(),
        scenario["upgrade_turn_state"].as_str(),
        "{name}: upgrade echoes x-codex-turn-state"
    );
    let mut socket = response.into_websocket().await.unwrap();

    let mut last_id = String::new();
    let mut closed = false;
    for (i, step) in scenario["steps"].as_array().unwrap().iter().enumerate() {
        let expected = expected_frames(step);
        if closed {
            assert!(expected.is_empty(), "{name} step {i}: frames after close");
            continue;
        }
        // Go `manager.Update` with `Disabled`: config credentials change in memory.
        for (key, disabled) in [("disable", true), ("enable", false)] {
            if step[key].as_bool() == Some(true) {
                for credential in rt.store().snapshot() {
                    let mut changed = cpa_core::credential::Credential::clone(&credential);
                    changed.disabled = disabled;
                    rt.store().replace_config_backed(changed, credential.revision).unwrap();
                }
            }
        }
        if let Some(hex) = step["send_hex"].as_str() {
            let bytes: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect();
            socket.send(Message::binary(bytes)).await.unwrap();
        }
        if let Some(send) = step["send"].as_str() {
            let send = send.replace("{{last_response_id}}", &last_id);
            socket.send(Message::text(send)).await.unwrap();
        }
        // A client that does not read for a while: large frames back up in the proxy's write.
        if let Some(pause) = step["pause_ms"].as_u64() {
            tokio::time::sleep(Duration::from_millis(pause)).await;
        }
        let mode = step["read"].as_str().unwrap();
        if mode == "none" {
            assert!(expected.is_empty(), "{name} step {i}: read mode is none");
            continue;
        }
        // "until:<type>[@<response id>]".
        let until = mode
            .strip_prefix("until:")
            .map(|target| target.split_once('@').unwrap_or((target, "")));
        let mut got = Vec::new();
        loop {
            let next = tokio::time::timeout(Duration::from_secs(5), socket.next())
                .await
                .unwrap_or_else(|_| panic!("{name} step {i}: read timed out after {got:?}"));
            let text = match next {
                Some(Ok(Message::Text(text))) => text.as_str().to_owned(),
                Some(Ok(Message::Close(frame))) => {
                    let (code, reason) = frame.map_or((1005, String::new()), |f| {
                        (u16::from(f.code), f.reason.as_str().to_owned())
                    });
                    got.push(Frame::Close(code, reason));
                    closed = true;
                    break;
                }
                Some(Ok(_)) => continue,
                // TCP closed without a close frame: gorilla reports 1006.
                Some(Err(_)) | None => {
                    got.push(Frame::Close(1006, "unexpected EOF".into()));
                    closed = true;
                    break;
                }
            };
            let kind = cpa_common::rawjson::get(&text, "type").str().to_owned();
            if kind == "response.completed" || kind == "response.done" {
                last_id = cpa_common::rawjson::get(&text, "response.id").str().to_owned();
            }
            got.push(Frame::Text(text.clone()));
            if let Some((want_kind, want_id)) = until
                && kind == want_kind
                && (want_id.is_empty() || cpa_common::rawjson::get(&text, "response.id").str() == want_id)
            {
                break;
            }
            if mode == "one"
                || (mode == "completed"
                    && matches!(
                        kind.as_str(),
                        "response.completed" | "response.done" | "response.incomplete"
                    ))
            {
                break;
            }
        }
        if !frames_match(&got, &expected) {
            // Frames can be megabytes long: show their start and length.
            let short = |frames: &[Frame]| {
                frames
                    .iter()
                    .map(|f| match f {
                        Frame::Text(t) if t.len() > 300 => {
                            format!("Text({}... {} bytes)", t.chars().take(300).collect::<String>(), t.len())
                        }
                        f => format!("{f:?}"),
                    })
                    .collect::<Vec<_>>()
            };
            panic!(
                "{name} step {i}: client frames\n got: {:?}\nwant: {:?}",
                short(&got),
                short(&expected)
            );
        }
    }

    tokio::time::sleep(Duration::from_millis(200)).await;
    let captured = up.captured.lock().unwrap().clone();
    if let Some(expected) = scenario["upstream"].as_array() {
        assert_eq!(captured.len(), expected.len(), "{name}: exact upstream operation count");
        for (index, (actual, want)) in captured.iter().zip(expected).enumerate() {
            assert_eq!(want["kind"], actual.kind, "{name}: operation #{index}");
            if let Some(hex) = want["body_hex"].as_str() {
                let actual_hex = actual.raw.iter().map(|b| format!("{b:02x}")).collect::<String>();
                assert_eq!(actual_hex, hex, "{name}: raw frame #{index}");
            } else {
                assert_eq!(
                    want["body"].as_str().unwrap_or_default(),
                    actual.body,
                    "{name}: exact upstream bytes #{index}"
                );
            }
            if actual.kind != "ws_frame" {
                assert_native_headers(name, scenario, actual);
            }
        }
    }

    assert_eq!(
        *up.canceled.lock().unwrap(),
        scenario["http_canceled"].as_u64().unwrap_or(0),
        "{name}: held HTTP responses the proxy canceled"
    );
    rt
}

#[tokio::test]
async fn ws_two_turns_reuse_one_upstream_socket() {
    run("ws_two_turns").await;
}

/// Go's HTTP mode merged the connection's transcript into every turn and turned a
/// `response.append` into a create. A Codex credential without `websockets` now sends
/// each `response.create` as the client wrote it, without `type` (a create without
/// `previous_response_id` already carries the client's full input), and refuses the
/// append: the client gets the error and the connection closes (#28,
/// docs/codex/CONTRACT.md "WebSocket HTTP fallback").
#[tokio::test]
async fn http_mode_sends_each_create_as_sent_and_refuses_append() {
    run_with("http_incremental", |scenario| {
        let steps = scenario["steps"].as_array_mut().unwrap();
        steps[2]["upstream"] = serde_json::json!([]);
        steps[2]["read"] = "close".into();
    })
    .await;
}

/// Go's step that sent a string `input` is left out: the HTTP fallback refused it
/// locally, and a Codex turn now carries the client's frame natively, so the provider
/// judges it (#28; `codex_native_ws.rs` covers that frame).
#[tokio::test]
async fn request_shape_errors_keep_the_connection_open() {
    run_with("previous_not_found_then_create", |scenario| {
        scenario["steps"].as_array_mut().unwrap().remove(2);
    })
    .await;
}

/// Go answers the prewarm of a Codex credential without `websockets` locally, with a
/// made-up `resp_prewarm_` response, and merges the warm-up input into the follow-up.
/// cliproxy-rs refuses that prewarm with an actionable error and sends nothing upstream
/// (#24 forbids synthesized output; docs/DIFFERENCES-FROM-GO.md, "Codex native WebSocket
/// route"). The connection stays open; the Go follow-up step no longer applies.
#[tokio::test]
async fn a_codex_fallback_prewarm_is_refused_instead_of_answered_locally() {
    run_with("prewarm_then_followup", |scenario| {
        let steps = scenario["steps"].as_array_mut().unwrap();
        steps.truncate(1);
        steps[0]["read"] = Value::String("one".into());
    })
    .await;
}

#[tokio::test]
async fn upstream_request_fault_is_shown_then_closed() {
    run("ws_request_fault_exposed").await;
}

/// v8.0.35: the error frame's `headers` come back on the error event, canonicalized.
#[tokio::test]
async fn upstream_request_fault_keeps_its_frame_headers() {
    run("ws_request_fault_headers").await;
}

#[tokio::test]
async fn quota_failure_closes_silently() {
    run("ws_quota_silent_close").await;
}

#[tokio::test]
async fn continuation_on_another_transport_closes_1012() {
    run("ws_continuation_needs_replay").await;
}

#[tokio::test]
async fn upstream_loss_between_turns_closes_the_client() {
    run("ws_upstream_closes_between_turns").await;
}

#[tokio::test]
async fn upstream_1009_reaches_the_client_as_1009() {
    run("ws_upstream_message_too_big").await;
}

/// FP4 #118: an upstream close 1009 before any event reaches the client as close 1009 with
/// no text before it (handshake metadata goes out only with the turn's first event), the
/// one reading first-party Codex takes as `WebsocketMessageTooLarge` and answers by
/// switching to HTTP itself. The proxy writes the request once and sends it nowhere
/// else.
#[tokio::test]
async fn upstream_1009_before_any_event_reaches_the_client_before_any_text() {
    run_with("ws_upstream_message_too_big", |scenario| {
        let step = &mut scenario["steps"][0];
        step["upstream"][0]["events"] = serde_json::json!([]);
    })
    .await;
}

/// gorilla's close text reaches the client verbatim; only a mapped 413 is trimmed.
#[tokio::test]
async fn upstream_1009_reason_reaches_the_client_verbatim() {
    run("ws_upstream_message_too_big_spaced_reason").await;
}

#[tokio::test]
async fn stream_ending_before_completion_closes_silently() {
    run("http_stream_ends_early").await;
}

#[tokio::test]
/// Go renames the `collaboration` namespace upstream and restores it for the client. The
/// native WebSocket route sends the client's frame unrenamed (checked against the client's
/// frame in [`run_scenario`]), so nothing is restored either: the client gets the scripted
/// upstream event as it arrived (docs/DIFFERENCES-FROM-GO.md, "Codex native WebSocket
/// route").
async fn ws_multi_agent_v2_is_neither_renamed_nor_restored() {
    run("ws_multi_agent_v2").await;
}

/// Go's multi-agent v2 and orphan-delegation rewrites renamed the namespace upstream and
/// restored it for the client. No Codex attempt receives them (#28): the client's create
/// goes upstream without its tag, and the upstream's events reach the client unchanged.
#[tokio::test]
async fn http_multi_agent_v2_is_not_applied_to_codex() {
    run("http_multi_agent_v2").await;
}

#[tokio::test]
async fn http_upstream_400_is_shown_then_closed() {
    run("http_upstream_400_exposed").await;
}

// Response steering (Go openai_responses_steering*_test.go, codex_websockets_duplex*_test.go).

/// M2-0147, M2-0148: in-flight steers go upstream raw; acknowledgements, the steered
/// response's `response.incomplete` and the automatic successor reach the client.
#[tokio::test]
async fn steering_forwards_steers_and_the_automatic_successor() {
    run("steer_successor").await;
}

/// A pending steer waits for the tool result; the explicit continuation is shaped like a
/// turn and keeps `previous_response_id`.
#[tokio::test]
async fn steering_waits_for_required_tool_input() {
    run("steer_tool_pending").await;
}

#[tokio::test]
async fn steering_upstream_close_after_accepted_steers_closes_the_client() {
    run("steer_disconnect_accepted").await;
}

/// M2-0149: invalid JSON and unknown types are rejected locally; the socket stays usable.
#[tokio::test]
async fn steering_rejects_bad_frames_locally_and_recovers() {
    // The native route reads client frames through the request boundary, which names the
    // first invalid byte (docs/DIFFERENCES-FROM-GO.md, "Codex native WebSocket route").
    run("steer_local_validation").await;
}

/// M2-0146: a later upstream error is an event the client can correct on the socket.
#[tokio::test]
async fn steering_later_error_recovers_with_a_corrected_create() {
    run("steer_later_error_recovers").await;
}

#[tokio::test]
async fn steering_initial_error_stays_terminal() {
    run("steer_initial_error_terminal").await;
}

#[tokio::test]
async fn steering_later_error_then_upstream_close_closes_the_client() {
    run("steer_later_error_then_upstream_close").await;
}

/// The historical `oauth.providers.codex` spelling is the shared `upstream.codex`
/// setting since Go 52d5507d, so it steers API keys too.
#[tokio::test]
async fn steering_historical_oauth_path_steers_api_keys() {
    run("steer_historical_oauth_path_steers_api_key").await;
}

#[tokio::test]
async fn steering_idle_upstream_close_closes_the_client() {
    run("steer_idle_upstream_close").await;
}

/// An append inherits `previous_response_id`, `model` and the parent's instructions.
/// Go turned a `response.append` on a steering connection into a create with the
/// parent's context. A Codex credential refuses it with a local error event, and the
/// connection stays open; nothing reaches the upstream (#28).
#[tokio::test]
async fn steering_append_is_refused_on_the_open_connection() {
    run_with("steer_append_inherits", |scenario| {
        let steps = scenario["steps"].as_array_mut().unwrap();
        steps[1]["upstream"] = serde_json::json!([]);
        steps[1]["read"] = "one".into();
    })
    .await;
}

/// A create queued behind an automatic successor does not hold back a later steer.
#[tokio::test]
async fn steering_queued_create_does_not_block_a_later_steer() {
    run("steer_queued_create_then_steer").await;
}

/// A failure of the running response leaves the pending create to start next.
#[tokio::test]
async fn steering_active_failure_keeps_the_queued_create() {
    run("steer_active_failure_keeps_queued_create").await;
}

/// A failure without a response ID while a create is pending ends the socket.
#[tokio::test]
async fn steering_ambiguous_failure_ends_the_socket() {
    run("steer_ambiguous_failure_ends_the_socket").await;
}

/// 401/403/429 after the first response end the stream with the original classification.
#[tokio::test]
async fn steering_later_credential_failure_ends_the_socket() {
    run("steer_later_credential_failure").await;
}

#[tokio::test]
async fn steering_later_quota_failure_ends_the_socket() {
    run("steer_later_quota_failure").await;
}

/// While a steer waits for tool input, a create must continue that response.
#[tokio::test]
async fn steering_rejects_a_create_for_another_parent() {
    run("steer_wrong_parent_rejected").await;
}

/// Another model cannot run on the bound socket: the client replays (1012).
#[tokio::test]
async fn steering_model_switch_needs_replay() {
    run("steer_model_switch_needs_replay").await;
}

/// M2-0145: a disabled credential sends no further frame; the connection ends.
#[tokio::test]
async fn steering_disabled_credential_cannot_send_another_frame() {
    run("steer_credential_disabled").await;
}

/// M2-0148: with steering off, `response.steer` is an unsupported request type.
#[tokio::test]
async fn steer_is_rejected_when_steering_is_off() {
    run("steer_rejected_when_disabled").await;
}

/// Go `TestResponsesWebsocketClosesOnIdleCodexDisconnect/legacy_disabled_api_key`.
#[tokio::test]
async fn steering_off_idle_upstream_close_closes_the_client() {
    run("steer_idle_upstream_close_legacy_disabled").await;
}

/// `.../v8_enabled_api_key`: the historical spelling, an API key.
#[tokio::test]
async fn steering_historical_idle_upstream_close_closes_an_api_key_client() {
    run("steer_idle_upstream_close_v8_api_key").await;
}

/// `.../v8_enabled_oauth`: the historical spelling, an OAuth account.
#[tokio::test]
async fn steering_historical_idle_upstream_close_closes_an_oauth_client() {
    run("steer_idle_upstream_close_v8_oauth").await;
}

/// `.../upstream_enabled_api_key`.
#[tokio::test]
async fn steering_upstream_idle_upstream_close_closes_an_api_key_client() {
    run("steer_idle_upstream_close_upstream_api_key").await;
}

/// `.../upstream_enabled_oauth`.
#[tokio::test]
async fn steering_upstream_idle_upstream_close_closes_an_oauth_client() {
    run("steer_idle_upstream_close_upstream_oauth").await;
}

/// Go `TestResponsesSteeringErrorRecoveryIntegration/disabled_error_remains_terminal`.
#[tokio::test]
async fn steering_off_later_error_stays_terminal() {
    run("steer_disabled_error_terminal").await;
}

/// Go `TestResponsesSteerInFlightWebSocket`.
#[tokio::test]
async fn steering_in_flight_steer_reaches_upstream() {
    run("steer_in_flight").await;
}

/// Go `TestResponsesSteeringFullDuplexIntegration/disconnect_pending`.
#[tokio::test]
async fn steering_upstream_drop_while_pending_closes_the_client() {
    run("steer_disconnect_pending").await;
}

/// Go `TestResponsesInterruptInFlight`: idle, malformed and disabled-credential
/// interrupts fail locally without touching upstream; an in-flight one reaches the
/// socket unchanged, its `response.incomplete` ends the turn, and the socket continues.
#[tokio::test]
async fn interrupt_reaches_the_active_socket_unchanged() {
    run("interrupt_in_flight").await;
}

#[tokio::test]
async fn interrupt_reaches_the_active_socket_unchanged_with_steering() {
    run("interrupt_in_flight_steering").await;
}

/// Go `TestResponsesInterruptStopsHTTPUpstream`: an HTTP turn ends locally with
/// `response.incomplete` (reason interrupted, the collected output), its upstream request
/// is canceled, and the next turn continues from that transcript. Deliberate difference:
/// current Codex waits for `response.interrupt.accepted`, which comes before the
/// incomplete event, and an interrupt for the finished response is acknowledged the same
/// way instead of rejected.
#[tokio::test]
async fn interrupt_cancels_an_http_turn_locally() {
    run_with("interrupt_http_local", |scenario| {
        let steps = scenario["steps"].as_array_mut().unwrap();
        // The continuation needs upstream state a credential without `websockets` does
        // not keep: the replay close, where Go answered from its transcript (#28).
        steps[3]["upstream"] = serde_json::json!([]);
        steps[3]["read"] = "close".into();
    })
    .await;
}

/// A binary `response.interrupt` holding a non-UTF-8 byte in a string is valid JSON
/// to Go and reaches upstream byte for byte.
#[tokio::test]
async fn binary_interrupt_reaches_upstream_byte_for_byte() {
    run("interrupt_binary_frame").await;
}

/// Go `TestResponsesLateInterruptDoesNotPoisonNextCreate`: an interrupt for a finished
/// response never reaches the next turn. Deliberate differences, for current Codex's
/// acknowledgement handshake: while no turn reads, the interrupt goes upstream and
/// upstream's acknowledgement reaches the client (Go: a silent no-op); an acknowledgement
/// after the terminal event of an interrupted turn still reaches the client. During the
/// next turn the late interrupt is a silent no-op, as in Go.
#[tokio::test]
async fn late_interrupt_does_not_poison_the_next_turn() {
    run_with("interrupt_late_ws", |scenario| {
        let ack = |id: &str| format!(r#"{{"type":"response.interrupt.accepted","response_id":"{id}"}}"#);
        let steps = scenario["steps"].as_array_mut().unwrap();
        // The late r1 interrupt on the idle socket.
        steps[2]["upstream"] = serde_json::json!([{"events": [ack("r1")]}]);
        steps[2]["read"] = "one".into();
        // The interrupted r2 acknowledges after its terminal event.
        steps[6]["upstream"][0]["events"]
            .as_array_mut()
            .unwrap()
            .push(ack("r2").into());
        steps[6]["read"] = "until:response.interrupt.accepted".into();
        // The late r2 interrupt on the idle socket.
        steps[7]["upstream"] = serde_json::json!([{"events": [ack("r2")]}]);
        steps[7]["read"] = "one".into();
    })
    .await;
}

/// zakstak/cliproxy-rs#5, not recorded from Go: an upstream `response.failed` with
/// `context_length_exceeded` reaches a Codex client byte for byte, then the connection
/// closes as after any exposed request fault. Codex classifies only that event as
/// `ContextWindowExceeded` (codex-api `parse_failed_response`) and then compacts; the
/// generic `{"type":"error","status":400}` frame Go v8.0.35 sends instead maps to an
/// invalid request (docs/DIFFERENCES-FROM-GO.md); other clients keep it. The scheduler
/// still sees the classified request-scoped error: one failure, and no cooldown.
mod context_window {
    use super::*;
    use serde_json::json;

    /// Codex's own `ContextWindowExceeded` fixture (codex-api sse/responses.rs).
    const FAILED: &str = r#"{"type":"response.failed","sequence_number":3,"response":{"id":"resp_5c66275b97b9baef1ed95550adb3b7ec13b17aafd1d2f11b","object":"response","created_at":1759510079,"status":"failed","background":false,"error":{"code":"context_length_exceeded","message":"Your input exceeds the context window of this model. Please adjust your input and try again."},"usage":null,"user":null,"metadata":{}}}"#;
    const CREATED: &str = r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_5c66275b97b9baef1ed95550adb3b7ec13b17aafd1d2f11b","object":"response","status":"in_progress","output":[]}}"#;
    /// A turn served over HTTP is native too (#28): `response.created` arrives as the
    /// upstream sent it, where Go added the response model.
    const CREATED_HTTP: &str = CREATED;
    const DELTA: &str = r#"{"type":"response.output_text.delta","sequence_number":1,"item_id":"msg_1","output_index":0,"content_index":0,"delta":"Partial answer"}"#;
    const SEND: &str = r#"{"type":"response.create","model":"gpt-fixture","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"huge"}]}]}"#;

    #[derive(Clone, Copy)]
    enum Transport {
        Websocket,
        Steering,
        Http,
    }

    /// Codex CLI's identity (Go `isCodexResponsesClientRequest`).
    const CODEX_UA: &str = "codex_cli_rs/0.200.0 (Linux; x86_64) xterm";

    /// One turn of a Codex client: `events` from upstream, `read` mode, and the frames the
    /// client must see.
    async fn turn(name: &str, transport: Transport, events: &[&str], read: &str, frames: Vec<Value>) {
        turn_as(name, Some(CODEX_UA), transport, events, read, frames).await;
    }

    async fn turn_paused(name: &str, events: &[&str], frames: Vec<Value>, pause_ms: u64, then: Option<&str>) {
        turn_with(
            name,
            Some(CODEX_UA),
            Transport::Websocket,
            events,
            "close",
            frames,
            Some(pause_ms),
            then,
        )
        .await;
    }

    async fn turn_as(
        name: &str,
        user_agent: Option<&str>,
        transport: Transport,
        events: &[&str],
        read: &str,
        frames: Vec<Value>,
    ) {
        turn_with(name, user_agent, transport, events, read, frames, None, None).await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn turn_with(
        name: &str,
        user_agent: Option<&str>,
        transport: Transport,
        events: &[&str],
        read: &str,
        frames: Vec<Value>,
        pause_ms: Option<u64>,
        then: Option<&str>,
    ) {
        let (config, websockets) = match transport {
            Transport::Websocket => ("host: \"\"\n", true),
            Transport::Steering => ("host: \"\"\ncodex:\n  response-steering: true\n", true),
            Transport::Http => ("host: \"\"\n", false),
        };
        let auth_dir = AuthDir::new();
        let config = format!("auth-dir: {}\n{config}", auth_dir.0.display());
        let mut attributes = json!({ "api_key": "sk-fake-ctx" });
        if websockets {
            attributes["websockets"] = json!("true");
        }
        let scenario = json!({
            "name": name,
            "config": config,
            "credentials": [{ "id": "codex-ctx.json", "attributes": attributes, "models": ["gpt-fixture"] }],
            "client_headers": user_agent.map(|ua| json!({ "user-agent": ua })),
            "steps": [{
                "send": SEND,
                "read": read,
                "pause_ms": pause_ms,
                "upstream": [{ "events": events, "then": then }],
                "frames": frames,
            }],
        });
        let rt = run_scenario(name, &scenario).await;
        let credential = rt.store().snapshot().into_iter().next().unwrap();
        assert!(
            !rt.store().blocked(&credential, "gpt-fixture"),
            "{name}: a context-window overflow is the request's fault, not the credential's"
        );
        assert!(
            rt.store().cooldowns(&credential.id).is_empty(),
            "{name}: {:?}",
            rt.store().cooldowns(&credential.id)
        );
        // A steering failure after `response.created` leaves the attempt open.
        if read == "close" && (then.is_none() || !matches!(transport, Transport::Steering)) {
            assert_eq!(
                (rt.store().stats().success, rt.store().stats().failure),
                (0, 1),
                "{name}: one failed attempt"
            );
        }
    }

    /// A private `auth-dir`, removed even when an assertion fails.
    struct AuthDir(std::path::PathBuf);

    impl AuthDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("cpa-it-auth-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for AuthDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn text(payload: &str) -> Value {
        json!({ "text": payload })
    }

    fn closed() -> Value {
        json!({ "close": 1006, "reason": "unexpected EOF" })
    }

    #[tokio::test]
    async fn ws_before_output() {
        let frames = vec![text(CREATED), text(FAILED), closed()];
        turn(
            "ws_before_output",
            Transport::Websocket,
            &[CREATED, FAILED],
            "close",
            frames,
        )
        .await;
    }

    #[tokio::test]
    async fn ws_as_the_first_event() {
        let frames = vec![text(FAILED), closed()];
        turn("ws_first_event", Transport::Websocket, &[FAILED], "close", frames).await;
    }

    #[tokio::test]
    async fn ws_after_streamed_output() {
        let frames = vec![text(CREATED), text(DELTA), text(FAILED), closed()];
        turn(
            "ws_after_output",
            Transport::Websocket,
            &[CREATED, DELTA, FAILED],
            "close",
            frames,
        )
        .await;
    }

    /// Before `response.created` a steering failure still ends the turn, as in Go, now
    /// after the event.
    #[tokio::test]
    async fn steering_as_the_first_event() {
        let frames = vec![text(FAILED), closed()];
        turn("steering_first_event", Transport::Steering, &[FAILED], "close", frames).await;
    }

    /// After `response.created` a steering failure was already an event the client may
    /// correct on the same socket; it stays one.
    #[tokio::test]
    async fn steering_after_streamed_output() {
        let frames = vec![text(CREATED), text(DELTA), text(FAILED)];
        let events = [CREATED, DELTA, FAILED];
        turn(
            "steering_after_output",
            Transport::Steering,
            &events,
            "until:response.failed",
            frames,
        )
        .await;
    }

    /// The HTTP transport for a credential without WebSockets (the proxy's fallback).
    #[tokio::test]
    async fn http_before_output() {
        let frames = vec![text(CREATED_HTTP), text(FAILED), closed()];
        turn(
            "http_before_output",
            Transport::Http,
            &[CREATED, FAILED],
            "close",
            frames,
        )
        .await;
    }

    #[tokio::test]
    async fn http_after_streamed_output() {
        let frames = vec![text(CREATED_HTTP), text(DELTA), text(FAILED), closed()];
        turn(
            "http_after_output",
            Transport::Http,
            &[CREATED, DELTA, FAILED],
            "close",
            frames,
        )
        .await;
    }

    /// The event is written to the client before the session's loss is reported: a
    /// client that reads slowly still gets the whole event, and no generic error after
    /// it. The event is larger than the socket buffers, so its write waits for the
    /// client while the session's loss would already close the connection.
    async fn slow_client(name: &str, events: &[&str], then: Option<&str>) {
        let message = format!(
            "Your input exceeds the context window of this model. {}",
            "x".repeat(8 << 20)
        );
        let failed = FAILED.replace(
            "Your input exceeds the context window of this model. Please adjust your input and try again.",
            &message,
        );
        let events: Vec<&str> = events
            .iter()
            .map(|e| if *e == FAILED { failed.as_str() } else { e })
            .collect();
        let mut frames: Vec<Value> = events.iter().map(|e| text(e)).collect();
        frames.push(closed());
        turn_paused(name, &events, frames, 300, then).await;
    }

    /// Inside the bootstrap window, where `response.created` is held.
    #[tokio::test]
    async fn ws_slow_client_in_bootstrap() {
        slow_client("ws_slow_client_bootstrap", &[CREATED, FAILED], None).await;
    }

    /// After the bootstrap window.
    #[tokio::test]
    async fn ws_slow_client_after_output() {
        slow_client("ws_slow_client_stream", &[CREATED, DELTA, FAILED], None).await;
    }

    /// The upstream ends its socket right after the event (a normal close, a 1009 close,
    /// or no close frame): the reader sees the end while the event is still queued for
    /// the turn or being written to the client. The event still arrives whole, followed
    /// by no error of the socket's end, and the scheduler still sees the classified
    /// failure.
    #[tokio::test]
    async fn ws_slow_client_upstream_ends_in_bootstrap() {
        let events = [CREATED, FAILED];
        slow_client("ws_slow_client_bootstrap_1009", &events, Some("close1009_now")).await;
        slow_client("ws_slow_client_bootstrap_close", &events, Some("close_now")).await;
        slow_client("ws_slow_client_bootstrap_drop", &events, Some("drop_now")).await;
    }

    #[tokio::test]
    async fn ws_slow_client_upstream_ends_after_output() {
        let events = [CREATED, DELTA, FAILED];
        slow_client("ws_slow_client_stream_1009", &events, Some("close1009_now")).await;
        slow_client("ws_slow_client_stream_close", &events, Some("close_now")).await;
        slow_client("ws_slow_client_stream_drop", &events, Some("drop_now")).await;
    }

    /// The turn is native on both transports (#24, #28): any client gets the upstream's
    /// `response.failed` as it arrived, where Go sent its generic error.
    #[tokio::test]
    async fn other_clients_keep_the_classified_error() {
        let frames = vec![text(CREATED), text(FAILED), closed()];
        turn_as(
            "ws_other_client",
            None,
            Transport::Websocket,
            &[CREATED, FAILED],
            "close",
            frames,
        )
        .await;
        let frames = vec![text(CREATED_HTTP), text(FAILED), closed()];
        turn_as(
            "http_other_client",
            None,
            Transport::Http,
            &[CREATED, FAILED],
            "close",
            frames,
        )
        .await;
    }

    /// A steering failure after `response.created` leaves the socket open; when the
    /// upstream later closes it for an unrelated reason, the client gets that close as
    /// it would without the earlier failure. The forwarded event stands only for its own
    /// classified error, not for anything after it.
    async fn steering_failure_then(name: &str, user_agent: Option<&str>, failed: &str, then: &str, reason: &str) {
        let frames = vec![text(CREATED), text(failed), json!({ "close": 1009, "reason": reason })];
        turn_with(
            name,
            user_agent,
            Transport::Steering,
            &[CREATED, failed],
            "close",
            frames,
            None,
            Some(then),
        )
        .await;
    }

    /// Another recoverable failure code (`invalid_prompt` is request-scoped too).
    const INVALID_PROMPT: &str = r#"{"type":"response.failed","sequence_number":3,"response":{"id":"resp_5c66275b97b9baef1ed95550adb3b7ec13b17aafd1d2f11b","object":"response","status":"failed","error":{"code":"invalid_prompt","message":"Invalid prompt."}}}"#;

    #[tokio::test]
    async fn steering_context_failure_then_upstream_close1009() {
        steering_failure_then("steering_ctx_close1009", Some(CODEX_UA), FAILED, "close1009", "too big").await;
        let name = "steering_ctx_close1009_spaced";
        steering_failure_then(name, Some(CODEX_UA), FAILED, "close1009_spaced", " oversized ").await;
    }

    #[tokio::test]
    async fn steering_other_failure_then_upstream_close1009() {
        let name = "steering_invalid_prompt_close1009";
        steering_failure_then(name, Some(CODEX_UA), INVALID_PROMPT, "close1009", "too big").await;
        let name = "steering_other_client_invalid_prompt_close1009";
        steering_failure_then(name, None, INVALID_PROMPT, "close1009", "too big").await;
    }

    #[tokio::test]
    async fn steering_other_client_context_failure_then_upstream_close1009() {
        let name = "steering_other_client_ctx_close1009";
        steering_failure_then(name, None, FAILED, "close1009_spaced", " oversized ").await;
    }

    /// The ChatGPT Codex backend's rejection of an input larger than the window, recorded
    /// from a live subscription request: an `error` event without a status and before
    /// `response.created`. Codex ignores such an event, so a Codex client receives it as
    /// the `response.failed` Codex compacts on, with the original error object, response
    /// ID and sequence number.
    mod live_error {
        use super::*;

        fn live() -> &'static str {
            include_str!("../../cpa-common/tests/fixtures/codex_context_window_error_live.json").trim()
        }

        fn normalized(live: &str) -> String {
            let error = &live[live.find(r#"{"message""#).unwrap()..live.find(r#","response_id""#).unwrap()];
            format!(
                r#"{{"type":"response.failed","sequence_number":3,"response":{{"id":"resp_0565bda653e9b5da016aee80a2d70487d18b1c1858bc049dcf","object":"response","status":"failed","error":{error}}}}}"#
            )
        }

        #[tokio::test]
        async fn ws_as_the_first_event() {
            let frames = vec![text(&normalized(live())), closed()];
            turn("live_ws_first_event", Transport::Websocket, &[live()], "close", frames).await;
        }

        #[tokio::test]
        async fn ws_then_upstream_close() {
            for then in ["close_now", "close1009_now", "drop_now"] {
                let frames = vec![text(&normalized(live())), closed()];
                let name = format!("live_ws_first_event_{then}");
                turn_with(
                    &name,
                    Some(CODEX_UA),
                    Transport::Websocket,
                    &[live()],
                    "close",
                    frames,
                    None,
                    Some(then),
                )
                .await;
            }
        }

        #[tokio::test]
        async fn steering_as_the_first_event() {
            let frames = vec![text(&normalized(live())), closed()];
            turn(
                "live_steering_first_event",
                Transport::Steering,
                &[live()],
                "close",
                frames,
            )
            .await;
        }

        #[tokio::test]
        async fn http_as_the_first_event() {
            let frames = vec![text(&normalized(live())), closed()];
            turn("live_http_first_event", Transport::Http, &[live()], "close", frames).await;
        }

        /// A slow client still gets the whole event before the connection closes, when the
        /// upstream also ends its socket right after it.
        #[tokio::test]
        async fn ws_slow_client_upstream_ends() {
            let message = format!(
                "Your input exceeds the context window of this model. {}",
                "x".repeat(8 << 20)
            );
            let live = live().replace(
                "Your input exceeds the context window of this model. Please adjust your input and try again.",
                &message,
            );
            let frames = vec![text(&normalized(&live)), closed()];
            turn_paused("live_ws_slow_client_close", &[&live], frames, 300, Some("close_now")).await;
        }

        /// A native turn delivers the upstream's `error` event as it arrived, over an upstream
        /// WebSocket and over HTTP alike (#28); Go sent its generic error.
        #[tokio::test]
        async fn other_clients_keep_the_classified_error() {
            let frames = vec![text(live()), closed()];
            turn_as(
                "live_other_client",
                None,
                Transport::Websocket,
                &[live()],
                "close",
                frames,
            )
            .await;
            let frames = vec![text(live()), closed()];
            turn_as(
                "live_other_client_http",
                None,
                Transport::Http,
                &[live()],
                "close",
                frames,
            )
            .await;
        }
    }
}
