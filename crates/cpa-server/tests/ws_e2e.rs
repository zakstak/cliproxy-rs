//! Responses WebSocket (`GET /v1/responses`) end to end, replaying
//! `tests/ws_fixtures/ws_e2e.json`. Each scenario ran through Go's real handler, auth
//! manager and `CodexAutoExecutor` against a scripted loopback upstream
//! (`tests/ws_fixtures/go/zz_rsfix_ws_test.go`). Here the same client frames and upstream
//! script run through the Rust router and Codex executor; the frames the client sees,
//! close codes, and what upstream received must match. Fake credentials only.

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
    serde_json::from_str::<Value>(include_str!("ws_fixtures/ws_e2e.json"))
        .unwrap()
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
    /// Held SSE responses whose request the proxy canceled (Go `http_canceled`).
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
        let kind = gjson::get(event, "type");
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

/// Random values Go and Rust both generate: UUIDs and the prewarm timestamp.
fn mask(s: &str) -> String {
    static UUID: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap());
    static CREATED: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r#""created_at":\d+"#).unwrap());
    let s = UUID.replace_all(s, "<uuid>");
    CREATED.replace_all(&s, r#""created_at":0"#).into_owned()
}

#[derive(Debug, PartialEq)]
enum Frame {
    Text(String),
    Close(u16, String),
}

/// Go leaves `frames` null for steps after the connection closed.
fn go_frames(step: &Value) -> Vec<Frame> {
    step["frames"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|f| match f["text"].as_str() {
            Some(text) => Frame::Text(mask(text)),
            None => Frame::Close(
                f["close"].as_u64().unwrap() as u16,
                f["reason"].as_str().unwrap_or_default().to_owned(),
            ),
        })
        .collect()
}

/// Headers both implementations must agree on, the permessage-deflate offer included.
/// Transport headers (WebSocket key, connection, length, encoding) legitimately differ
/// between gorilla/net http and wreq.
const COMPARED_HEADERS: [&str; 11] = [
    "sec-websocket-extensions",
    "authorization",
    "openai-beta",
    "originator",
    "user-agent",
    "x-codex-turn-state",
    "session_id",
    "session-id",
    "conversation_id",
    "accept",
    "content-type",
];

fn compared(headers: impl Iterator<Item = (String, String)>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = headers
        .filter(|(k, _)| COMPARED_HEADERS.contains(&k.as_str()))
        .map(|(k, v)| (k, mask(&v)))
        .collect();
    out.sort();
    out
}

async fn run(name: &str) {
    let scenario = FIXTURE
        .iter()
        .find(|s| s["name"] == name)
        .unwrap_or_else(|| panic!("no scenario {name}"));
    run_scenario(name, scenario).await;
}

/// Runs one scenario and returns its runtime, for checks after the connection. A
/// scenario without `upstream` (one not recorded from Go) skips the comparison of what
/// upstream received.
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
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(cfg, credentials, executors));
    // Like main.rs (and Go's global registry): translators and the Codex client rewrites
    // read this runtime's models. One scenario at a time owns the process-wide overlay.
    static REGISTRY: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _registry = REGISTRY.lock().await;
    cpa_server::install_registry(&rt);
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
        let expected = go_frames(step);
        if closed {
            assert!(expected.is_empty(), "{name} step {i}: Go read after close");
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
            assert!(expected.is_empty(), "{name} step {i}: Go read nothing");
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
            let kind = gjson::get(&text, "type").str().to_owned();
            if kind == "response.completed" || kind == "response.done" {
                last_id = gjson::get(&text, "response.id").str().to_owned();
            }
            got.push(Frame::Text(mask(&text)));
            if let Some((want_kind, want_id)) = until
                && kind == want_kind
                && (want_id.is_empty() || gjson::get(&text, "response.id").str() == want_id)
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
        if got != expected {
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
    let Some(go) = scenario["upstream"].as_array() else {
        return rt;
    };
    let go: Vec<&Value> = go.iter().collect();
    let kinds = |c: &[Captured]| c.iter().map(|c| c.kind.clone()).collect::<Vec<_>>();
    let go_kinds: Vec<String> = go.iter().map(|c| c["kind"].as_str().unwrap().to_owned()).collect();
    assert_eq!(
        kinds(&captured),
        go_kinds,
        "{name}: upstream dials, frames and requests"
    );
    for (i, (rust, go)) in captured.iter().zip(&go).enumerate() {
        assert_eq!(
            mask(&rust.body),
            mask(go["body"].as_str().unwrap_or_default()),
            "{name}: upstream body #{i}"
        );
        if let Some(hex) = go["body_hex"].as_str() {
            let rust_hex: String = rust.raw.iter().map(|b| format!("{b:02x}")).collect();
            assert_eq!(rust_hex, hex, "{name}: upstream bytes #{i}");
        }
        let go_headers = go["headers"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()));
        assert_eq!(
            compared(rust.headers.clone().into_iter()),
            compared(go_headers),
            "{name}: upstream headers #{i}"
        );
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

#[tokio::test]
async fn http_mode_merges_the_transcript_each_turn() {
    run("http_incremental").await;
}

#[tokio::test]
async fn request_shape_errors_keep_the_connection_open() {
    run("previous_not_found_then_create").await;
}

#[tokio::test]
async fn local_prewarm_then_followup_sends_the_warmup_input() {
    run("prewarm_then_followup").await;
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
async fn ws_multi_agent_v2_renames_upstream_and_restores_for_the_client() {
    run("ws_multi_agent_v2").await;
}

#[tokio::test]
async fn http_multi_agent_v2_renames_upstream_and_restores_for_the_client() {
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
#[tokio::test]
async fn steering_append_inherits_parent_context() {
    run("steer_append_inherits").await;
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
/// is canceled, and the next turn continues from that transcript.
#[tokio::test]
async fn interrupt_cancels_an_http_turn_locally() {
    run("interrupt_http_local").await;
}

/// A binary `response.interrupt` holding a non-UTF-8 byte in a string is valid JSON
/// to Go and reaches upstream byte for byte.
#[tokio::test]
async fn binary_interrupt_reaches_upstream_byte_for_byte() {
    run("interrupt_binary_frame").await;
}

/// Go `TestResponsesLateInterruptDoesNotPoisonNextCreate`: an interrupt for a finished
/// response is a silent no-op, before and during the next turn, and never reaches upstream.
#[tokio::test]
async fn late_interrupt_does_not_poison_the_next_turn() {
    run("interrupt_late_ws").await;
}

#[test]
fn every_go_scenario_has_a_test() {
    let names: Vec<&str> = FIXTURE.iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "ws_two_turns",
            "http_incremental",
            "previous_not_found_then_create",
            "prewarm_then_followup",
            "ws_request_fault_exposed",
            "ws_request_fault_headers",
            "ws_quota_silent_close",
            "ws_continuation_needs_replay",
            "ws_upstream_closes_between_turns",
            "ws_upstream_message_too_big",
            "ws_upstream_message_too_big_spaced_reason",
            "http_stream_ends_early",
            "ws_multi_agent_v2",
            "http_multi_agent_v2",
            "steer_successor",
            "steer_tool_pending",
            "steer_disconnect_accepted",
            "steer_local_validation",
            "steer_later_error_recovers",
            "steer_initial_error_terminal",
            "steer_later_error_then_upstream_close",
            "steer_historical_oauth_path_steers_api_key",
            "steer_idle_upstream_close",
            "steer_idle_upstream_close_legacy_disabled",
            "steer_idle_upstream_close_v8_api_key",
            "steer_idle_upstream_close_v8_oauth",
            "steer_idle_upstream_close_upstream_api_key",
            "steer_idle_upstream_close_upstream_oauth",
            "steer_disabled_error_terminal",
            "steer_in_flight",
            "steer_disconnect_pending",
            "steer_append_inherits",
            "steer_queued_create_then_steer",
            "steer_active_failure_keeps_queued_create",
            "steer_ambiguous_failure_ends_the_socket",
            "steer_later_credential_failure",
            "steer_later_quota_failure",
            "steer_wrong_parent_rejected",
            "steer_model_switch_needs_replay",
            "steer_credential_disabled",
            "steer_rejected_when_disabled",
            "http_upstream_400_exposed",
            "interrupt_in_flight",
            "interrupt_in_flight_steering",
            "interrupt_http_local",
            "interrupt_binary_frame",
            "interrupt_late_ws",
        ]
    );
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
    /// `response.created` as the HTTP transport delivers it (with the response model).
    const CREATED_HTTP: &str = r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_5c66275b97b9baef1ed95550adb3b7ec13b17aafd1d2f11b","object":"response","status":"in_progress","output":[],"model":"gpt-fixture"}}"#;
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
        let auth_dir = std::env::temp_dir().join(format!("cpa-it-auth-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&auth_dir).unwrap();
        let config = format!("auth-dir: {}\n{config}", auth_dir.display());
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
        let _ = std::fs::remove_dir_all(&auth_dir);
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

    /// Go's generic error frame for a client that is not Codex, on both transports.
    const GENERIC: &str = r#"{"type":"error","status":400,"error":{"message":"Your input exceeds the context window of this model. Please adjust your input and try again.","type":"invalid_request_error","code":"context_too_large"}}"#;

    #[tokio::test]
    async fn other_clients_keep_the_classified_error() {
        let frames = vec![text(CREATED), text(GENERIC), closed()];
        turn_as(
            "ws_other_client",
            None,
            Transport::Websocket,
            &[CREATED, FAILED],
            "close",
            frames,
        )
        .await;
        let frames = vec![text(CREATED_HTTP), text(GENERIC), closed()];
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

        #[tokio::test]
        async fn other_clients_keep_the_classified_error() {
            for transport in [Transport::Websocket, Transport::Http] {
                let frames = vec![text(GENERIC), closed()];
                turn_as("live_other_client", None, transport, &[live()], "close", frames).await;
            }
        }
    }
}
