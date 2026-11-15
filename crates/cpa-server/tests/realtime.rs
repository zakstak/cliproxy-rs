//! Realtime routes against private loopback peers and fake credentials.
//! Historical request corpora supply stimuli only. Assertions specify supported
//! route/auth/session, SDP conversion, credential isolation and RFC 6455 behavior.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::IntoResponse;
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_exec::Executors;
use cpa_exec::codex::CodexExecutor;
use cpa_exec::codex_oauth::CodexOAuth;
use cpa_server::router;
use serde_json::Value;

#[derive(Default)]
struct Mock {
    next: Option<Value>,
    seen: Vec<(axum::http::Method, Uri, HeaderMap, Bytes)>,
}

type Shared = Arc<Mutex<Mock>>;

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

fn credential(id: &str, meta: Value, attrs: &[(&str, &str)]) -> Credential {
    let mut c = Credential::from_file(
        Path::new("/fake"),
        &Path::new("/fake").join(id),
        meta.as_object().unwrap().clone(),
    )
    .unwrap();
    c.attributes = attrs
        .iter()
        .map(|(k, v)| ((*k).into(), (*v).into()))
        .collect::<BTreeMap<_, _>>();
    c
}

/// A proxy whose live upstream is a loopback mock answering with `mock.next`.
async fn proxy(credentials: Vec<Credential>, mock: Shared) -> String {
    let upstream = axum::Router::new()
        .fallback(
            |State(mock): State<Shared>, method: axum::http::Method, uri: Uri, headers: HeaderMap, body: Bytes| async move {
                let mut mock = mock.lock().unwrap();
                mock.seen.push((method, uri, headers, body));
                let Some(next) = mock.next.clone() else {
                    return (StatusCode::from_u16(599).unwrap(), "unexpected upstream call").into_response();
                };
                let mut response = (
                    StatusCode::from_u16(next["status"].as_u64().unwrap() as u16).unwrap(),
                    next["body"].as_str().unwrap_or_default().to_owned(),
                )
                    .into_response();
                response.headers_mut().remove("content-type");
                if let Some(headers) = next["headers"].as_object() {
                    for (name, values) in headers {
                        for value in values.as_array().unwrap() {
                            response.headers_mut().append(
                                name.parse::<axum::http::HeaderName>().unwrap(),
                                value.as_str().unwrap().parse().unwrap(),
                            );
                        }
                    }
                }
                response
            },
        )
        .with_state(mock);
    let upstream_url = serve(upstream).await;
    let executor = CodexExecutor::with_client(wreq::Client::new(), CodexOAuth::new(wreq::Client::new()))
        .with_live_endpoints(
            format!("{upstream_url}/backend-api/codex/realtime/calls?intent=quicksilver&architecture=avas"),
            format!("ws{}/v1", upstream_url.trim_start_matches("http")),
        );
    let rt = Arc::new(cpa_server::testing::runtime(
        Config::parse("api-keys: [good-key, other-key]").unwrap(),
        credentials,
        Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: executor,
            meta: Default::default(),
            xai: Default::default(),
        },
    ));
    serve(router(rt)).await
}

fn test_credentials() -> Vec<Credential> {
    vec![
        credential(
            "a-codex-apikey",
            serde_json::json!({"type":"codex"}),
            &[("api_key", "sk-must-not-be-used")],
        ),
        credential(
            "b-codex-oauth",
            serde_json::json!({"type":"codex","access_token":"oauth-token","account_id":"acct-1","email":"user@example.com"}),
            &[("header:X-Operator", "op-value")],
        ),
    ]
}

/// Protocol metadata accepted at the Realtime boundary, independently specified.
const PROTOCOL_HEADERS: [&str; 9] = [
    "openai-alpha",
    "x-session-id",
    "session-id",
    "thread-id",
    "originator",
    "openai-safety-identifier",
    "openai-organization",
    "openai-project",
    "x-oai-attestation",
];
fn input_header<'a>(input: &'a Value, name: &str) -> Vec<&'a str> {
    input
        .as_object()
        .into_iter()
        .flat_map(|h| h.iter())
        .filter(|(k, _)| k.eq_ignore_ascii_case(name))
        .flat_map(|(_, v)| v.as_array().unwrap().iter().map(|v| v.as_str().unwrap()))
        .collect()
}
fn http_status(name: &str, peer: Option<&Value>) -> Option<u16> {
    if let Some(peer) = peer {
        return Some(peer["status"].as_u64().unwrap() as u16);
    }
    Some(match name {
        "live_missing_key"
        | "live_invalid_key"
        | "calls_missing_key"
        | "calls_invalid_key"
        | "calls_unknown_client_secret"
        | "secrets_reject_client_secret"
        | "ek_hangup_rejected"
        | "sip_without_key" => 401,
        "live_invalid_multipart"
        | "calls_invalid_multipart"
        | "calls_bad_session_json"
        | "secret_invalid_json"
        | "secret_bad_session"
        | "secret_bad_anchor"
        | "secret_bad_seconds"
        | "legacy_session_array"
        | "sideband_invalid_call_id"
        | "sideband_query_invalid_call_id"
        | "hangup_invalid_call_id" => 400,
        // A null session formerly pinned Go's nil-map panic. Only safe refusal is
        // useful here; its accidental recovery status and prose are not contracts.
        "calls_null_session_panics" => return None,
        "ek_direct_model_mismatch" | "sideband_ek_principal_mismatch" | "hangup_other_principal" => 403,
        "sideband_unknown_call" | "hangup_unknown_call" | "hangup_again_not_found" => 404,
        "live_sideband_no_upgrade"
        | "calls_sideband_no_upgrade"
        | "realtime_query_sideband_no_upgrade"
        | "realtime_direct_no_upgrade" => 426,
        "live_no_credentials" | "calls_no_credentials" => 503,
        "secret_create" | "secret_default_session" | "legacy_session" => 200,
        "secret_too_large" => 413,
        "secret_unsupported"
        | "ek_translation"
        | "transcription_sessions"
        | "translations_get"
        | "translations_post"
        | "translations_client_secrets"
        | "sip_accept"
        | "sip_reject"
        | "sip_refer" => 501,
        _ => panic!("unclassified Realtime input: {name}"),
    })
}
fn multipart_payload(body: &str, content_type: &str) -> Value {
    let boundary = content_type.split("boundary=").nth(1).unwrap();
    let mut out = serde_json::Map::new();
    for part in body.split(&format!("--{boundary}")) {
        let Some((head, value)) = part.split_once("\r\n\r\n") else {
            continue;
        };
        let value = value.strip_suffix("\r\n").unwrap();
        if head.contains("name=\"sdp\"") {
            out.insert("sdp".into(), value.into());
        }
        if head.contains("name=\"session\"") {
            out.insert("session".into(), serde_json::from_str(value).unwrap());
        }
    }
    out.into()
}

#[tokio::test]
async fn realtime_http_route_auth_session_and_forwarding_contracts() {
    let corpus: Value = serde_json::from_str(include_str!("fixtures/realtime_http_inputs.json")).unwrap();
    let cases = corpus["cases"].as_array().unwrap();
    let mocks: BTreeMap<&str, Shared> = [("main", Shared::default()), ("empty", Shared::default())].into();
    let mut servers = BTreeMap::new();
    servers.insert("main", proxy(test_credentials(), mocks["main"].clone()).await);
    servers.insert("empty", proxy(vec![], mocks["empty"].clone()).await);
    let client = wreq::Client::new();
    // Masked alias -> the token this run issued.
    let mut secrets: BTreeMap<String, String> = BTreeMap::new();
    let mut issued = std::collections::BTreeSet::new();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let server = case["server"].as_str().unwrap();
        let mock = &mocks[server];
        {
            let mut mock = mock.lock().unwrap();
            mock.next = case.get("upstream").filter(|u| !u.is_null()).cloned();
            mock.seen.clear();
        }
        let request = &case["request"];
        let method: wreq::Method = request["method"].as_str().unwrap().parse().unwrap();
        let mut builder = client.request(
            method,
            format!("{}{}", servers[server], request["path"].as_str().unwrap()),
        );
        if let Some(headers) = request["headers"].as_object() {
            for (header, values) in headers {
                for value in values.as_array().unwrap() {
                    let mut value = value.as_str().unwrap().to_owned();
                    for (alias, real) in &secrets {
                        value = value.replace(alias, real);
                    }
                    builder = builder.header(header.as_str(), value);
                }
            }
        }
        let response = builder
            .body(request["body"].as_str().unwrap_or_default().to_owned())
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let body = response.text().await.unwrap();
        let peer = case.get("upstream").filter(|v| !v.is_null());
        if let Some(want) = http_status(name, peer) {
            assert_eq!(status, want, "{name}");
        } else {
            assert!(status >= 400, "{name}: invalid session must be refused");
        }
        for name in ["set-cookie", "x-live-session"] {
            assert!(!headers.contains_key(name), "{name}: private peer metadata leaked");
        }
        let seen = std::mem::take(&mut mock.lock().unwrap().seen);
        assert_eq!(seen.len(), usize::from(peer.is_some()), "{name}: upstream call count");
        if let Some(peer) = peer {
            assert_eq!(body, peer["body"].as_str().unwrap_or_default(), "{name}: peer bytes");
            for h in ["retry-after", "x-request-id", "openai-request-id"] {
                let actual: Vec<_> = headers.get_all(h).iter().map(|v| v.to_str().unwrap()).collect();
                assert_eq!(actual, input_header(&peer["headers"], h), "{name}: {h}");
            }
            let location = input_header(&peer["headers"], "location");
            let expected_location = if status < 300 {
                location.first().map(|v| {
                    let id = v
                        .split('?')
                        .next()
                        .unwrap()
                        .trim_end_matches('/')
                        .rsplit('/')
                        .next()
                        .unwrap();
                    format!(
                        "{}/{}",
                        if request["path"].as_str().unwrap() == "/v1/live" {
                            "/v1/live"
                        } else {
                            "/v1/realtime/calls"
                        },
                        id
                    )
                })
            } else {
                location.first().map(|v| (*v).to_owned())
            };
            assert_eq!(
                headers.get("location").map(|v| v.to_str().unwrap()),
                expected_location.as_deref(),
                "{name}: local call location"
            );
            let (sent_method, uri, sent_headers, sent_body) = &seen[0];
            assert_eq!(
                sent_method.as_str(),
                request["method"].as_str().unwrap(),
                "{name}: upstream method"
            );
            let path = request["path"].as_str().unwrap();
            let hangup = path.ends_with("/hangup");
            let expected_target = if hangup {
                path.to_owned()
            } else {
                "/backend-api/codex/realtime/calls?intent=quicksilver&architecture=avas".into()
            };
            assert_eq!(uri.to_string(), expected_target, "{name}: target");
            assert_eq!(sent_headers["authorization"], "Bearer oauth-token", "{name}");
            assert_eq!(sent_headers["chatgpt-account-id"], "acct-1", "{name}");
            assert_eq!(sent_headers["x-operator"], "op-value", "{name}");
            for h in PROTOCOL_HEADERS {
                let actual: Vec<_> = sent_headers.get_all(h).iter().map(|v| v.to_str().unwrap()).collect();
                assert_eq!(actual, input_header(&request["headers"], h), "{name}: {h}");
            }
            for h in ["x-not-forwarded", "proxy-authorization", "cookie"] {
                assert!(!sent_headers.contains_key(h), "{name}: {h}");
            }
            let raw = request["body"].as_str().unwrap_or_default();
            let content_type = input_header(&request["headers"], "content-type")
                .first()
                .copied()
                .unwrap_or("application/json");
            if hangup || content_type == "application/sdp" && name != "ek_call_sdp" {
                assert_eq!(sent_body.as_ref(), raw.as_bytes(), "{name}: passthrough");
            } else {
                let mut payload: Value = if name == "ek_call_sdp" {
                    serde_json::json!({"sdp":raw,"session":{"type":"realtime","model":"gpt-realtime","instructions":"<help>","n":1.5}})
                } else if content_type.starts_with("multipart/") {
                    multipart_payload(raw, content_type)
                } else {
                    serde_json::from_str(raw).unwrap()
                };
                let model = if payload["session"].is_object() {
                    &mut payload["session"]["model"]
                } else {
                    &mut payload["model"]
                };
                if model.as_str().is_some_and(|m| m.starts_with("gpt-realtime")) {
                    *model = "gpt-live-1-codex".into();
                }
                // Absent model stays absent. Unknown fields and numeric values survive
                // shape conversion; JSON key order and HTML escaping are incidental.
                if model.is_null() {
                    payload.as_object_mut().unwrap().remove("model");
                }
                assert_eq!(
                    serde_json::from_slice::<Value>(sent_body).unwrap(),
                    payload,
                    "{name}: call payload"
                );
                assert_eq!(sent_headers["content-type"], "application/json", "{name}");
            }
        } else if matches!(name, "secret_create" | "secret_default_session" | "legacy_session") {
            assert_eq!(headers["cache-control"], "no-store", "{name}");
            let value: Value = serde_json::from_str(&body).unwrap();
            let legacy = name == "legacy_session";
            let secret = if legacy { &value["client_secret"] } else { &value };
            let token = secret["value"].as_str().unwrap();
            assert!(
                token.starts_with("ek_") && token.len() > 32,
                "unguessable scoped credential"
            );
            assert!(issued.insert(token.to_owned()), "fresh credentials must differ");
            if name == "secret_create" {
                secrets.insert("ek_<secret-1>".into(), token.to_owned());
            }
            let session = if legacy { &value } else { &value["session"] };
            assert!(session["id"].as_str().unwrap().starts_with("sess_"));
            assert_eq!(session["object"], "realtime.session");
            assert_eq!(session["type"], "realtime");
            let request_body: Value =
                serde_json::from_str(request["body"].as_str().unwrap_or_default()).unwrap_or(serde_json::json!({}));
            let requested = if legacy {
                &request_body
            } else {
                &request_body["session"]
            };
            if let Some(fields) = requested.as_object() {
                for (key, value) in fields {
                    assert_eq!(&session[key], value, "{name}: session {key}");
                }
            } else {
                assert_eq!(session["model"], "gpt-realtime");
            }
            let expiry = secret["expires_at"].as_i64().unwrap();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            let ttl = if name == "secret_create" { 60 } else { 600 };
            assert!(
                (now + ttl - 5..=now + ttl + 1).contains(&expiry),
                "{name}: configured expiry {expiry}"
            );
            assert_eq!(session["expires_at"], expiry);
        } else if name != "calls_null_session_panics" {
            let error: Value =
                serde_json::from_str(&body).unwrap_or_else(|e| panic!("{name}: invalid error envelope {body:?}: {e}"));
            assert!(
                error["error"]["message"]
                    .as_str()
                    .or_else(|| error["error"].as_str())
                    .is_some_and(|s| !s.is_empty()),
                "{name}: actionable error"
            );
            assert!(
                !body.contains("oauth-token") && !body.contains("sk-must-not-be-used"),
                "{name}: credentials leaked"
            );
        }
    }
}

// ---------------------------------------------------------------------------------------
// WebSockets

/// One relayed frame as both harnesses record it.
#[derive(Debug, Clone, PartialEq)]
enum Frame {
    Text(String),
    Binary(Vec<u8>),
    /// A close frame's code; reasons differ only in the automatic close echo (gorilla
    /// echoes an empty reason, tungstenite the received one), so they are not compared.
    Close(u16),
    Gone,
}

fn frame(v: &Value) -> Frame {
    let data = v["data"].as_str().unwrap_or_default();
    match v["kind"].as_str().unwrap() {
        "text" => Frame::Text(data.into()),
        "binary" => Frame::Binary(data.as_bytes().to_vec()),
        "close" => Frame::Close(v["code"].as_u64().unwrap_or(1005) as u16),
        _ => Frame::Gone,
    }
}

#[derive(Default)]
struct WsSeen {
    target: String,
    headers: HeaderMap,
    received: Vec<Frame>,
}

#[derive(Default)]
struct WsMock {
    /// Requests to the calls endpoint: their Authorization and Chatgpt-Account-Id.
    created: Vec<(String, String)>,
    seen: Option<WsSeen>,
    done: Option<Arc<tokio::sync::Notify>>,
}

type WsShared = Arc<Mutex<WsMock>>;

/// The Go harness's upstream: calls answer 201 with the `Thread-Id` as call ID; sockets
/// echo with `echo:`, close with 4001 on `upstream-close`, and reject marked targets.
async fn ws_upstream(
    State(mock): State<WsShared>,
    uri: Uri,
    headers: HeaderMap,
    ws: Result<axum::extract::ws::WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> axum::response::Response {
    use axum::extract::ws::Message;
    let header = |name: &str| {
        headers
            .get(name)
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default()
    };
    let Ok(ws) = ws else {
        mock.lock()
            .unwrap()
            .created
            .push((header("authorization"), header("chatgpt-account-id")));
        let location = format!("/v1/live/{}", header("thread-id"));
        return (
            StatusCode::CREATED,
            [("location", location.as_str()), ("content-type", "application/sdp")],
            "v=0",
        )
            .into_response();
    };
    let target = uri.to_string();
    for (marker, status, content_type, body) in [
        (
            "401",
            401,
            "application/json",
            r#"{"error":{"message":"token expired"}}"#,
        ),
        ("404", 404, "text/plain", "no such call"),
        ("429", 429, "application/json", r#"{"error":"slow"}"#),
    ] {
        if target.contains(&format!("-{marker}")) {
            return (
                StatusCode::from_u16(status).unwrap(),
                [
                    ("content-type", content_type),
                    ("x-request-id", &format!("up-{marker}")),
                    ("retry-after", "9"),
                    ("x-other", "dropped"),
                ],
                body.to_owned(),
            )
                .into_response();
        }
    }
    let offered: Vec<String> = header("sec-websocket-protocol")
        .split(',')
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.is_empty())
        .collect();
    let mut ws = ws;
    if let Some(last) = offered.last().filter(|p| *p != "no-select") {
        ws.set_selected_protocol(last.parse().unwrap());
    }
    ws.on_upgrade(move |mut socket| async move {
        let mut record = WsSeen {
            target,
            headers,
            received: vec![],
        };
        loop {
            let message = match socket.recv().await {
                Some(Ok(message)) => message,
                _ => {
                    record.received.push(Frame::Gone);
                    break;
                }
            };
            let reply = match message {
                Message::Text(text) => {
                    record.received.push(Frame::Text(text.as_str().into()));
                    if text.as_str() == "upstream-close" {
                        let close = axum::extract::ws::CloseFrame {
                            code: 4001,
                            reason: "bye".into(),
                        };
                        let _ = socket.send(Message::Close(Some(close))).await;
                        let next = socket.recv().await;
                        record.received.push(match next {
                            Some(Ok(Message::Close(f))) => Frame::Close(f.map_or(1005, |f| f.code)),
                            _ => Frame::Gone,
                        });
                        break;
                    }
                    Message::Text(format!("echo:{}", text.as_str()).into())
                }
                Message::Binary(data) => {
                    record.received.push(Frame::Binary(data.to_vec()));
                    let mut echo = b"echo:".to_vec();
                    echo.extend_from_slice(&data);
                    Message::Binary(echo.into())
                }
                Message::Close(f) => {
                    record.received.push(Frame::Close(f.map_or(1005, |f| f.code)));
                    break;
                }
                Message::Ping(_) | Message::Pong(_) => continue,
            };
            let _ = socket.send(reply).await;
        }
        let mut mock = mock.lock().unwrap();
        mock.seen = Some(record);
        if let Some(done) = mock.done.take() {
            done.notify_one();
        }
    })
}

#[tokio::test]
async fn realtime_websocket_relay_scope_and_identity_contracts() {
    let corpus: Value = serde_json::from_str(include_str!("fixtures/realtime_ws_inputs.json")).unwrap();
    let cases = corpus["cases"].as_array().unwrap();
    let mock = WsShared::default();
    let upstream_url = serve(axum::Router::new().fallback(ws_upstream).with_state(mock.clone())).await;
    let executor = CodexExecutor::with_client(wreq::Client::new(), CodexOAuth::new(wreq::Client::new()))
        .with_live_endpoints(
            format!("{upstream_url}/backend-api/codex/realtime/calls?intent=quicksilver&architecture=avas"),
            format!("ws{}/v1", upstream_url.trim_start_matches("http")),
        );
    let credentials = vec![
        credential(
            "a-other-oauth",
            serde_json::json!({"type":"codex","access_token":"other-token","account_id":"other-account"}),
            &[],
        ),
        credential(
            "b-pinned-oauth",
            serde_json::json!({"type":"codex","access_token":"pinned-token","account_id":"pinned-account"}),
            &[("header:X-Operator", "op-value")],
        ),
    ];
    let rt = Arc::new(cpa_server::testing::runtime(
        Config::parse("api-keys: [owner-key, other-key]").unwrap(),
        credentials,
        Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: executor,
            meta: Default::default(),
            xai: Default::default(),
        },
    ));
    let proxy = serve(router(rt)).await;
    let client = wreq::Client::new();
    let secret = client
        .post(format!("{proxy}/v1/realtime/client_secrets"))
        .header("authorization", "Bearer owner-key")
        .body(r#"{"session":{"type":"realtime","model":"gpt-realtime","instructions":"<be brief>","voice":"alloy"}}"#)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["value"]
        .as_str()
        .unwrap()
        .to_owned();
    let authorization = |principal: &str| match principal {
        "owner" => "Bearer owner-key".to_owned(),
        "other" => "Bearer other-key".to_owned(),
        _ => format!("Bearer {secret}"),
    };
    // Create calls through the public route. Sidebands must use their creator
    // even when rotation would otherwise select a different upstream account.
    let setup: BTreeMap<&str, (&str, &str)> = [
        ("live_sideband_relays", ("call-live", "owner")),
        ("calls_sideband_client_close", ("call-calls", "owner")),
        ("query_sideband", ("call-query", "owner")),
        ("sideband_other_principal", ("call-scope", "owner")),
        ("sideband_secret_owner", ("call-secret", "secret")),
        ("sideband_secret_wrong_call", ("call-secret2", "owner")),
        ("sideband_upstream_401", ("call-401", "owner")),
        ("live_sideband_upstream_401", ("live-401", "owner")),
        ("sideband_upstream_404", ("call-404", "owner")),
        ("live_sideband_upstream_404", ("live-404", "owner")),
        ("sideband_upstream_429", ("call-429", "owner")),
        ("live_sideband_upstream_429", ("live-429", "owner")),
        ("sideband_upstream_selects_no_protocol", ("call-noselect", "owner")),
    ]
    .into();
    let mut creators: BTreeMap<String, (String, String)> = BTreeMap::new();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        if let Some((call_id, principal)) = setup.get(name) {
            let path = if *principal == "secret" {
                "/v1/realtime/calls"
            } else {
                "/v1/live"
            };
            let created = client
                .post(format!("{proxy}{path}"))
                .header("authorization", authorization(principal))
                .header("content-type", "application/json")
                .header("thread-id", *call_id)
                .body(r#"{"sdp":"v=0"}"#)
                .send()
                .await
                .unwrap();
            assert_eq!(created.status().as_u16(), 201, "{name}: creating {call_id}");
            let creator = mock.lock().unwrap().created.pop().unwrap();
            creators.insert((*call_id).to_owned(), creator);
        }
        let path = case["path"].as_str().unwrap();
        let principal = case["principal"].as_str().unwrap();
        let done = Arc::new(tokio::sync::Notify::new());
        {
            let mut mock = mock.lock().unwrap();
            mock.seen = None;
            mock.done = Some(done.clone());
        }
        let ws_url = format!("ws{}{path}", proxy.trim_start_matches("http"));
        let mut builder = client
            .websocket(&ws_url)
            .header("authorization", authorization(principal));
        if let Some(headers) = case["headers"].as_object() {
            for (header, values) in headers {
                let value = values[0].as_str().unwrap();
                // A raw offer, so wreq's strict negotiation cannot hide what the proxy did.
                builder = builder.header(header.as_str(), value);
            }
        }
        let mut response = builder.send().await.unwrap();
        let status = response.status().as_u16();
        let expected_status = match name {
            "live_sideband_consumed" => 404,
            "sideband_other_principal" | "sideband_secret_wrong_call" => 403,
            _ if name.ends_with("_401") => 401,
            _ if name.ends_with("_404") => {
                if name.starts_with("direct_") {
                    501
                } else {
                    404
                }
            }
            _ if name.ends_with("_429") => 429,
            _ => 101,
        };
        assert_eq!(status, expected_status, "{name}");
        let headers = response.headers().clone();
        for h in ["x-other", "set-cookie"] {
            assert!(!headers.contains_key(h), "{name}: {h}");
        }
        if name.contains("upstream_") && status != 101 {
            let code = name.rsplit('_').next().unwrap();
            assert_eq!(headers["x-request-id"], format!("up-{code}"));
            assert_eq!(headers["retry-after"], "9");
        }
        let mut received = vec![];
        if status == 101 {
            let protocol = response
                .headers_mut()
                .remove("sec-websocket-protocol")
                .map(|p| p.to_str().unwrap().to_owned());
            let mut socket = response.into_websocket().await.unwrap();
            let offered = input_header(&case["headers"], "sec-websocket-protocol");
            let selected = offered
                .first()
                .and_then(|h| h.split(',').next_back())
                .map(str::trim)
                .filter(|p| *p != "no-select");
            assert_eq!(protocol.as_deref(), selected, "{name}: peer-selected protocol");
            use wreq::ws::message::{CloseFrame, Message};
            for step in case["send"].as_array().unwrap() {
                let data = step["data"].as_str().unwrap_or_default().to_owned();
                let message = match step["kind"].as_str().unwrap() {
                    "read" => None,
                    "text" => Some(Message::text(data)),
                    "binary" => Some(Message::binary(data.into_bytes())),
                    "ping" => {
                        socket.send(Message::ping(data.into_bytes())).await.unwrap();
                        continue;
                    }
                    _ => Some(Message::Close(Some(CloseFrame {
                        code: (step["code"].as_u64().unwrap() as u16).into(),
                        reason: data.into(),
                    }))),
                };
                if let Some(message) = message {
                    let _ = socket.send(message).await;
                }
                let got = loop {
                    match tokio::time::timeout(std::time::Duration::from_secs(2), socket.recv()).await {
                        Ok(Some(Ok(Message::Text(t)))) => break Frame::Text(t.as_str().into()),
                        Ok(Some(Ok(Message::Binary(b)))) => break Frame::Binary(b.to_vec()),
                        Ok(Some(Ok(Message::Close(f)))) => break Frame::Close(f.map_or(1005, |f| u16::from(f.code))),
                        Ok(Some(Ok(_))) => continue,
                        _ => break Frame::Gone,
                    }
                };
                let stop = matches!(got, Frame::Close(_) | Frame::Gone);
                received.push(got);
                if stop {
                    break;
                }
            }
            drop(socket);
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), done.notified()).await;
        } else {
            let inner = std::mem::replace(
                &mut *response,
                wreq::Response::from(axum::http::Response::new(Vec::<u8>::new())),
            );
            let body = inner.text().await.unwrap();
            if status == 401 {
                assert_eq!(
                    serde_json::from_str::<Value>(&body).unwrap(),
                    serde_json::json!({"error":{"message":"token expired"}}),
                    "{name}"
                );
            } else {
                let error: Value = serde_json::from_str(&body)
                    .unwrap_or_else(|e| panic!("{name}: invalid error envelope {body:?}: {e}"));
                assert!(
                    error["error"]["message"]
                        .as_str()
                        .or_else(|| error["error"].as_str())
                        .is_some_and(|m| !m.is_empty()),
                    "{name}: actionable refusal"
                );
                assert!(
                    !body.contains("no such call") && !body.contains("pinned-token") && !body.contains("other-token"),
                    "{name}: unsafe peer detail"
                );
            }
        }
        if status == 101 {
            let mut expected = Vec::new();
            for step in case["send"].as_array().unwrap() {
                let data = step["data"].as_str().unwrap_or_default();
                match step["kind"].as_str().unwrap() {
                    "text" if data == "upstream-close" => expected.push(Frame::Close(4001)),
                    "text" => expected.push(Frame::Text(format!("echo:{data}"))),
                    "binary" => expected.push(Frame::Binary([b"echo:".as_slice(), data.as_bytes()].concat())),
                    "close" => expected.push(Frame::Close(step["code"].as_u64().unwrap() as u16)),
                    "read" => {
                        let Frame::Text(update) = &received[expected.len()] else {
                            panic!("{name}: session update missing")
                        };
                        let update: Value = serde_json::from_str(update.strip_prefix("echo:").unwrap()).unwrap();
                        assert_eq!(
                            update,
                            serde_json::json!({"type":"session.update","session":{"type":"realtime","instructions":"<be brief>","voice":"alloy"}}),
                            "{name}: scoped session initialization"
                        );
                        expected.push(received[expected.len()].clone());
                    }
                    "ping" => {}
                    _ => panic!("unknown relay input"),
                }
            }
            assert_eq!(received, expected, "{name}: client relay");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let seen = mock.lock().unwrap().seen.take();
        if status != 101 {
            assert!(seen.is_none(), "{name}: rejected session cannot relay");
            continue;
        }
        let seen = seen.expect("accepted relay must reach peer");
        let sideband = !name.starts_with("direct_");
        let call_id = path.rsplit(['/', '=']).next().unwrap();
        let expected_target = if sideband {
            if path.contains("?call_id=") {
                path.replace("?call_id=", "?intent=quicksilver&call_id=")
            } else {
                path.to_owned()
            }
        } else {
            let model = path.split("model=").nth(1).unwrap_or("gpt-realtime");
            format!("/v1/realtime?model={model}")
        };
        assert_eq!(seen.target, expected_target, "{name}: peer target");
        let token = seen.headers["authorization"].to_str().unwrap();
        let account = seen.headers["chatgpt-account-id"].to_str().unwrap();
        if sideband {
            assert_eq!(
                (token, account),
                creators.get(call_id).map(|(t, a)| (t.as_str(), a.as_str())).unwrap(),
                "{name}: creator pinning"
            );
        } else {
            assert!(
                matches!(
                    (token, account),
                    ("Bearer other-token", "other-account") | ("Bearer pinned-token", "pinned-account")
                ),
                "{name}: OAuth identity pair"
            );
        }
        assert_ne!(token, authorization(principal), "client credential cannot reach peer");
        for h in PROTOCOL_HEADERS {
            let mut expected = input_header(&case["headers"], h);
            if !sideband && h == "openai-alpha" {
                expected.clear();
            }
            if !sideband && h == "originator" && expected.is_empty() {
                expected.push("cliproxy-rs");
            }
            let actual: Vec<_> = seen.headers.get_all(h).iter().map(|v| v.to_str().unwrap()).collect();
            assert_eq!(actual, expected, "{name}: {h}");
        }
        for h in ["x-not-forwarded", "proxy-authorization", "cookie"] {
            assert!(!seen.headers.contains_key(h), "{name}: {h}");
        }
        let mut expected = Vec::new();
        if principal == "secret" && !sideband {
            let Frame::Text(update) = &seen.received[0] else {
                panic!("{name}: initial session")
            };
            let update: Value = serde_json::from_str(update).unwrap();
            assert_eq!(
                update,
                serde_json::json!({"type":"session.update","session":{"type":"realtime","instructions":"<be brief>","voice":"alloy"}})
            );
            expected.push(seen.received[0].clone());
        }
        for step in case["send"].as_array().unwrap() {
            match step["kind"].as_str().unwrap() {
                "read" | "ping" => {}
                "text" => {
                    expected.push(frame(step));
                    if step["data"] == "upstream-close" {
                        expected.push(Frame::Close(4001));
                    }
                }
                _ => expected.push(frame(step)),
            }
        }
        assert_eq!(seen.received, expected, "{name}: upstream relay");
    }
    // Pinning must hold even when rotation would pick the other credential.
    let tokens: std::collections::BTreeSet<&str> = creators.values().map(|(t, _)| t.as_str()).collect();
    assert_eq!(tokens.len(), 2, "calls were created with both credentials");
}

/// Call state across requests (sideband.go `sessionStore`, `HandleHangup`): one sideband
/// at a time, a hangup ends a running sideband and forgets the call, and a call whose
/// credential disappeared cannot borrow another one.
#[tokio::test]
async fn sideband_claims_and_hangup_teardown() {
    use wreq::ws::message::Message;
    let mock = WsShared::default();
    let upstream_url = serve(axum::Router::new().fallback(ws_upstream).with_state(mock.clone())).await;
    let executor = CodexExecutor::with_client(wreq::Client::new(), CodexOAuth::new(wreq::Client::new()))
        .with_live_endpoints(
            format!("{upstream_url}/backend-api/codex/realtime/calls"),
            format!("ws{}/v1", upstream_url.trim_start_matches("http")),
        );
    let rt = Arc::new(cpa_server::testing::runtime(
        Config::parse("api-keys: [owner-key]").unwrap(),
        vec![credential(
            "only-oauth",
            serde_json::json!({"type":"codex","access_token":"only-token"}),
            &[],
        )],
        Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: executor,
            meta: Default::default(),
            xai: Default::default(),
        },
    ));
    let proxy = serve(router(rt.clone())).await;
    let client = wreq::Client::new();
    let create = |call_id: &'static str| {
        client
            .post(format!("{proxy}/v1/live"))
            .header("authorization", "Bearer owner-key")
            .header("thread-id", call_id.to_string())
            .body(r#"{"sdp":"v=0"}"#)
            .send()
    };
    let join = |call_id: &'static str| {
        client
            .websocket(format!(
                "ws{}/v1/realtime/calls/{call_id}",
                proxy.trim_start_matches("http")
            ))
            .header("authorization", "Bearer owner-key")
            .send()
    };
    assert_eq!(create("call-a").await.unwrap().status().as_u16(), 201);
    let first = join("call-a").await.unwrap();
    assert_eq!(first.status().as_u16(), 101);
    let mut first = first.into_websocket().await.unwrap();
    first.send(Message::text("ping")).await.unwrap();
    let echoed = tokio::time::timeout(std::time::Duration::from_secs(2), first.recv()).await;
    assert!(matches!(echoed, Ok(Some(Ok(Message::Text(t)))) if t.as_str() == "echo:ping"));

    let second = join("call-a").await.unwrap();
    assert_eq!(second.status().as_u16(), 409, "one sideband per call");

    let hangup = client
        .post(format!("{proxy}/v1/realtime/calls/call-a/hangup"))
        .header("authorization", "Bearer owner-key")
        .send()
        .await
        .unwrap();
    assert_eq!(hangup.status().as_u16(), 201, "upstream status passes through");
    let ended = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match first.recv().await {
                Some(Ok(Message::Text(_) | Message::Binary(_) | Message::Ping(_) | Message::Pong(_))) => continue,
                other => return other,
            }
        }
    })
    .await;
    assert!(ended.is_ok(), "hangup ends the running sideband");
    let again = join("call-a").await.unwrap();
    assert_eq!(again.status().as_u16(), 404, "the call is forgotten");

    // The call stays pinned: once its credential is gone, no other one stands in.
    assert_eq!(create("call-b").await.unwrap().status().as_u16(), 201);
    rt.store().reconcile(vec![]);
    let orphan = join("call-b").await.unwrap();
    assert_eq!(orphan.status().as_u16(), 503);
}

/// RFC 6455 section 4 handshake validation and local call ownership: a rejected
/// handshake releases its claim; a completed accepted connection consumes the call.
#[tokio::test]
async fn websocket_handshake_validation_releases_or_consumes_call_claims() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let cases = [
        (
            "valid",
            Some("dGhlIHNhbXBsZSBub25jZQ=="),
            "Upgrade",
            "websocket",
            "13",
            101,
        ),
        ("invalid_base64", Some("x"), "Upgrade", "websocket", "13", 400),
        (
            "short_nonce",
            Some("AAAAAAAAAAAAAAAAAAAA"),
            "Upgrade",
            "websocket",
            "13",
            400,
        ),
        ("missing_nonce", None, "Upgrade", "websocket", "13", 400),
        (
            "token_lists",
            Some("dGhlIHNhbXBsZSBub25jZQ=="),
            "keep-alive, Upgrade",
            "h2c, websocket",
            "13",
            101,
        ),
        // Preserve the accepted version-list client form independently of Go.
        (
            "version_list",
            Some("dGhlIHNhbXBsZSBub25jZQ=="),
            "Upgrade",
            "websocket",
            "12, 13",
            101,
        ),
        (
            "unsupported_version",
            Some("dGhlIHNhbXBsZSBub25jZQ=="),
            "Upgrade",
            "websocket",
            "12",
            400,
        ),
    ];
    let mock = WsShared::default();
    let upstream_url = serve(axum::Router::new().fallback(ws_upstream).with_state(mock.clone())).await;
    let executor = CodexExecutor::with_client(wreq::Client::new(), CodexOAuth::new(wreq::Client::new()))
        .with_live_endpoints(
            format!("{upstream_url}/backend-api/codex/realtime/calls"),
            format!("ws{}/v1", upstream_url.trim_start_matches("http")),
        );
    let rt = Arc::new(cpa_server::testing::runtime(
        Config::parse("api-keys: [owner-key]").unwrap(),
        vec![credential(
            "only-oauth",
            serde_json::json!({"type":"codex","access_token":"only-token"}),
            &[],
        )],
        Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: executor,
            meta: Default::default(),
            xai: Default::default(),
        },
    ));
    let proxy = serve(router(rt)).await;
    let client = wreq::Client::new();
    for (name, key, connection, upgrade, version, expected_status) in cases {
        let call_id = format!("handshake-{name}");
        let created = client
            .post(format!("{proxy}/v1/live"))
            .header("authorization", "Bearer owner-key")
            .header("thread-id", call_id.to_string())
            .body(r#"{"sdp":"v=0"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(created.status().as_u16(), 201);
        let mut stream = tokio::net::TcpStream::connect(proxy.trim_start_matches("http://"))
            .await
            .unwrap();
        let nonce = key.map(|k| format!("Sec-WebSocket-Key: {k}\r\n")).unwrap_or_default();
        let request = format!(
            "GET /v1/live/{call_id} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer owner-key\r\nConnection: {connection}\r\nUpgrade: {upgrade}\r\nSec-WebSocket-Version: {version}\r\n{nonce}\r\n"
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut head = vec![0u8; 1024];
        let n = tokio::time::timeout(std::time::Duration::from_secs(2), stream.read(&mut head))
            .await
            .unwrap()
            .unwrap();
        let status: u16 = String::from_utf8_lossy(&head[..n])
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        drop(stream);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        // A kept call can still be joined; a consumed one is gone.
        let rejoin = client
            .websocket(format!("ws{}/v1/live/{call_id}", proxy.trim_start_matches("http")))
            .header("authorization", "Bearer owner-key")
            .send()
            .await
            .unwrap();
        let kept = rejoin.status().as_u16() == 101;
        drop(rejoin);
        assert_eq!(status, expected_status, "{name}");
        assert_eq!(kept, expected_status != 101, "{name}: claim lifetime");
    }
}

/// Local relay policy: protocol/I/O failures close the peer with 1011; a clean
/// downstream EOF sends a normal close. Exact library diagnostic prose is incidental.
#[tokio::test]
async fn relay_failures_close_the_peer_and_release_resources() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let cases = [("rsv1", 1011), ("abrupt", 1000), ("reset", 1011)];
    let mock = WsShared::default();
    let upstream_url = serve(axum::Router::new().fallback(ws_upstream).with_state(mock.clone())).await;
    let executor = CodexExecutor::with_client(wreq::Client::new(), CodexOAuth::new(wreq::Client::new()))
        .with_live_endpoints(
            format!("{upstream_url}/backend-api/codex/realtime/calls"),
            format!("ws{}/v1", upstream_url.trim_start_matches("http")),
        );
    let rt = Arc::new(cpa_server::testing::runtime(
        Config::parse("api-keys: [owner-key]").unwrap(),
        vec![credential(
            "only-oauth",
            serde_json::json!({"type":"codex","access_token":"only-token"}),
            &[],
        )],
        Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: executor,
            meta: Default::default(),
            xai: Default::default(),
        },
    ));
    let proxy = serve(router(rt)).await;
    let client = wreq::Client::new();
    for (action, expected_close) in cases {
        let call_id = format!("call-end-{action}");
        let created = client
            .post(format!("{proxy}/v1/live"))
            .header("authorization", "Bearer owner-key")
            .header("thread-id", call_id.to_string())
            .body(r#"{"sdp":"v=0"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(created.status().as_u16(), 201);
        mock.lock().unwrap().seen = None;
        let stream = tokio::net::TcpStream::connect(proxy.trim_start_matches("http://"))
            .await
            .unwrap();
        let mut stream = stream;
        let request = format!(
            "GET /v1/live/{call_id} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer owner-key\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0u8; 1];
            stream.read_exact(&mut byte).await.unwrap();
            head.push(byte[0]);
        }
        assert!(
            head.starts_with(b"HTTP/1.1 101"),
            "{action}: {}",
            String::from_utf8_lossy(&head)
        );
        match action {
            "rsv1" => {
                let mask = [1u8, 2, 3, 4];
                let mut frame = vec![0x80 | 0x40 | 0x1, 0x80 | 1];
                frame.extend_from_slice(&mask);
                frame.push(b'x' ^ mask[0]);
                stream.write_all(&frame).await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                drop(stream);
            }
            "abrupt" => drop(stream),
            _ => {
                #[allow(deprecated)]
                stream.set_linger(Some(std::time::Duration::ZERO)).unwrap();
                drop(stream);
            }
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let got = loop {
            let seen = mock
                .lock()
                .unwrap()
                .seen
                .as_ref()
                .filter(|s| s.target.contains(&call_id))
                .map(|s| s.received.clone());
            if let Some(got) = seen {
                break got;
            }
            if tokio::time::Instant::now() > deadline {
                break vec![];
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        assert_eq!(got, vec![Frame::Close(expected_close)], "{action}: peer teardown");
        let rejoin = client
            .websocket(format!("ws{}/v1/live/{call_id}", proxy.trim_start_matches("http")))
            .header("authorization", "Bearer owner-key")
            .send()
            .await
            .unwrap();
        assert_eq!(
            rejoin.status().as_u16(),
            404,
            "{action}: completed call cannot be reused"
        );
    }
}

/// A Sign in with ChatGPT credential needs its direct-scope token; the outer
/// credential token cannot substitute for it or reach the upstream.
#[tokio::test]
async fn direct_realtime_refuses_a_siwc_credential_without_its_token_with_401() {
    let mock = Shared::default();
    let siwc = credential(
        "siwc",
        serde_json::json!({"type":"codex","use_siwc":true,"access_token":"outer-token","siwc":{"refresh_token":"rt"}}),
        &[],
    );
    let proxy = proxy(vec![siwc], mock.clone()).await;
    let mut response = wreq::Client::new()
        .websocket(format!(
            "ws{}/v1/realtime?model=gpt-realtime",
            proxy.trim_start_matches("http")
        ))
        .header("authorization", "Bearer good-key")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 401);
    let inner = std::mem::replace(
        &mut *response,
        wreq::Response::from(axum::http::Response::new(Vec::<u8>::new())),
    );
    assert_eq!(
        inner.json::<Value>().await.unwrap(),
        serde_json::json!({"error": {"message": "Codex Realtime WebSocket upstream unavailable",
            "type": "authentication_error", "param": null, "code": "realtime_upstream_unauthorized"}})
    );
    assert!(mock.lock().unwrap().seen.is_empty());
}
