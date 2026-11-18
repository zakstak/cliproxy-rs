//! xAI HTTP behavior against local scripted peers. Expectations describe routing,
//! authentication isolation, protocol semantics, tool bridges and usage accounting.
//! Generated header order, JSON spelling and translator frame boundaries are irrelevant.

use std::sync::{Arc, Mutex};

use crate::execution::Caller;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use super::*;
use crate::providers::usage_testkit::{HttpAttempt, Logged, http_capture_problems, published};

// Historical client inputs and scripted peer answers; recorded Go outputs are excluded.
const FIXTURE: &str = include_str!("../../tests/providers/fixtures/xai_http_inputs.json");

/// Caller headers that must never reach xAI, with a header a `$X-Explicit-UA`
/// attribute may reference.
const CALLER_HEADERS: [(&str, &str); 7] = [
    ("authorization", "Bearer caller-secret-FAKE"),
    ("cookie", "session=caller-cookie-FAKE"),
    ("x-api-key", "caller-api-key-FAKE"),
    ("proxy-authorization", "Basic caller-proxy-FAKE"),
    ("x-amz-security-token", "caller-token-FAKE"),
    ("x-unrelated", "caller-extra"),
    ("x-explicit-ua", "referenced/4.0"),
];

/// The caller's first User-Agent value (a second one follows it), an optional
/// `header:User-Agent` attribute and the User-Agent upstream must see outside the Grok
/// CLI chat proxy.
const USER_AGENT_CASES: [(Option<&str>, Option<&str>, &str); 10] = [
    (None, None, "xai-grok-workspace/1.0.44"),
    (Some(""), None, "xai-grok-workspace/1.0.44"),
    (Some(" \t "), None, "xai-grok-workspace/1.0.44"),
    (Some("caller-client/2.0"), None, "caller-client/2.0"),
    (Some("  caller-client/2.0 \t"), None, "caller-client/2.0"),
    // Go's strings.TrimSpace removes Unicode white space such as U+00A0.
    (Some("\u{a0}caller-client/2.0\u{a0}"), None, "caller-client/2.0"),
    (Some("caller-\u{e9}/2.0"), None, "caller-\u{e9}/2.0"),
    (Some("caller-client/2.0"), Some("operator/3.0"), "operator/3.0"),
    (Some("caller-client/2.0"), Some("$X-Explicit-UA"), "referenced/4.0"),
    (Some("caller-client/2.0"), Some("$X-Missing"), "caller-client/2.0"),
];

/// Lexically unusual members the client sends that no rule rewrites.
const OPAQUE_MEMBER: &str = r#""future_field" : {"opaque":"\u0061", "big":12345678901234567890}, "#;

/// #88 on the HTTP paths, over a loopback upstream: only `User-Agent` is taken from the
/// caller, on the API, an OAuth custom base URL, OAuth compact and the native media
/// path, while the chat proxy keeps the fixed Grok CLI identity. A `header:` attribute
/// still wins. The native body is unchanged. This is local policy evidence, not
/// evidence that xAI accepts any of these identities.
#[tokio::test]
async fn caller_user_agent_on_http() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    for name in [
        "api_key_default_base",
        "oauth_custom_base_without_cli_headers",
        "compact_oauth_uses_official_api",
        "videos_create_idempotency",
        "oauth_default_chat_proxy",
    ] {
        let mut scenario = fixture["scenarios"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == name)
            .unwrap()
            .clone();
        let payload = scenario["payload"]
            .as_str()
            .unwrap()
            .replacen('{', &format!("{{{OPAQUE_MEMBER}"), 1);
        scenario["payload"] = Value::String(payload.clone());
        let chat_proxy = name == "oauth_default_chat_proxy";
        for (caller, custom, expected) in USER_AGENT_CASES {
            let mock = Mock::start(&scenario["upstream"]).await;
            let addr = mock.addr.clone();
            let executor = XaiExecutor::default().with_url_rewrite(move |url| redirect(url, &addr));
            // A private auth-dir that is never created: these paths read no logins.
            let cfg = Config {
                auth_dir: std::env::temp_dir().join(format!("cpa-xai-ua-{}", std::process::id())),
                ..Default::default()
            };
            let mut c = credential(&scenario, &cfg);
            if let Some(custom) = custom {
                c.attributes.insert("header:User-Agent".into(), custom.into());
            }
            let mut req = request(&scenario, Default::default());
            for (header, value) in CALLER_HEADERS {
                req.headers.insert(header, value.parse().unwrap());
            }
            if let Some(caller) = caller {
                req.headers.insert("user-agent", caller.parse().unwrap());
                // Go's Header.Get reads the first value, also when it is blank.
                req.headers.append("user-agent", "second-client/9.0".parse().unwrap());
            }
            let result = if scenario["op"] == "videos" {
                let path = scenario["request_path"].as_str().unwrap().to_owned();
                executor.videos(&c, req, &path, &cfg).await
            } else {
                executor.execute(&c, req, &cfg, false).await
            };
            let case = format!("{name}, caller {caller:?}, header:User-Agent {custom:?}");
            assert!(result.is_ok(), "{case}: {:?}", result.err());
            let raw = mock.request().unwrap();
            let (head, body) = raw.split_once("\r\n\r\n").unwrap();
            let lines: Vec<(&str, &str)> = head.lines().skip(1).filter_map(|l| l.split_once(": ")).collect();
            let get = |header: &str| -> Vec<&str> {
                lines
                    .iter()
                    .filter(|(n, _)| n.eq_ignore_ascii_case(header))
                    .map(|(_, v)| *v)
                    .collect()
            };
            // The chat proxy's fixed identity gives way only to a resolved attribute.
            let expected = if chat_proxy && matches!(custom, None | Some("$X-Missing")) {
                "xai-grok-workspace/1.0.44"
            } else {
                expected
            };
            assert_eq!(get("User-Agent"), [expected], "{case}");
            assert_eq!(get("Authorization"), [format!("Bearer {}", creds(&c).0)], "{case}");
            for (header, _) in &CALLER_HEADERS[1..] {
                assert_eq!(get(header), Vec::<&str>::new(), "{case}: {header} reached upstream");
            }
            let cli = if chat_proxy { vec!["xai-grok-cli"] } else { vec![] };
            assert_eq!(get("X-XAI-Token-Auth"), cli, "{case}");
            assert!(body.contains(OPAQUE_MEMBER), "{case}: opaque member preserved");
            if scenario["op"] == "videos" {
                assert_eq!(body, payload, "{case}: native body bytes preserved");
            } else {
                assert!(gj::get(body.as_bytes(), "input").exists(), "{case}: translated input");
            }
        }
    }
}

/// One-shot raw HTTP/1.1 capture server answering with the scripted response.
struct Mock {
    addr: String,
    raw: Arc<Mutex<Option<Vec<u8>>>>,
}

impl Mock {
    async fn start(up: &Value) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let raw: Arc<Mutex<Option<Vec<u8>>>> = Arc::default();
        let sink = raw.clone();
        let status = up["status"].as_u64().unwrap() as u16;
        let headers: Vec<(String, String)> = up["headers"]
            .as_array()
            .map(|hs| {
                hs.iter()
                    .map(|h| (h[0].as_str().unwrap().to_owned(), h[1].as_str().unwrap().to_owned()))
                    .collect()
            })
            .unwrap_or_default();
        let body = up["body"].as_str().unwrap().as_bytes().to_vec();
        tokio::spawn(async move {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let (read, mut write) = socket.into_split();
            let mut reader = BufReader::new(read);
            let mut captured = Vec::new();
            let mut length = 0;
            loop {
                let mut line = Vec::new();
                if reader.read_until(b'\n', &mut line).await.unwrap_or(0) == 0 {
                    break;
                }
                captured.extend_from_slice(&line);
                let text = String::from_utf8_lossy(&line).to_ascii_lowercase();
                if let Some(v) = text.strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
                if line == b"\r\n" {
                    break;
                }
            }
            let mut payload = vec![0; length];
            reader.read_exact(&mut payload).await.unwrap();
            captured.extend_from_slice(&payload);
            *sink.lock().unwrap() = Some(captured);
            let reason = http::StatusCode::from_u16(status)
                .ok()
                .and_then(|s| s.canonical_reason())
                .unwrap_or("");
            let mut out = format!("HTTP/1.1 {status} {reason}\r\n").into_bytes();
            for (n, v) in &headers {
                out.extend_from_slice(format!("{n}: {v}\r\n").as_bytes());
            }
            out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
            out.extend_from_slice(b"Connection: close\r\n\r\n");
            out.extend_from_slice(&body);
            let _ = write.write_all(&out).await;
            let _ = write.shutdown().await;
        });
        Self { addr, raw }
    }

    fn request(&self) -> Option<String> {
        let raw = self.raw.lock().unwrap().clone()?;
        Some(String::from_utf8_lossy(&raw).replace(&self.addr, "UPSTREAM"))
    }
}

fn credential(s: &Value, cfg: &Config) -> Credential {
    let index = s["config_auth"].as_i64().unwrap();
    if index >= 0 {
        let xai: Vec<Credential> = crate::config::credentials::from_config(cfg)
            .into_iter()
            .filter(|c| c.provider == "xai")
            .collect();
        return xai[index as usize].clone();
    }
    let mut metadata = s["metadata"].as_object().cloned().unwrap_or_default();
    metadata.insert("type".into(), "xai".into());
    let mut c = Credential::from_file(
        std::path::Path::new("/fixture"),
        std::path::Path::new("/fixture/xai.json"),
        metadata,
    )
    .unwrap();
    for (k, v) in s["attributes"].as_object().into_iter().flatten() {
        c.attributes.insert(k.clone(), v.as_str().unwrap().to_owned());
    }
    c
}

fn response_format(s: &Value) -> Format {
    let source = Format::parse(s["source"].as_str().unwrap()).unwrap();
    s["response"].as_str().and_then(Format::parse).unwrap_or(source)
}

/// The dispatch loop's model binding, as the generator bound it.
fn bound_model(v: &Value) -> Option<crate::execution::ResolvedModel> {
    Some(crate::execution::ResolvedModel {
        info: crate::catalog::ModelInfo::from_raw(v.as_object()?.clone()).expect("resolved model info"),
        source: crate::execution::ResolvedSource::ApiKey,
    })
}

fn request(s: &Value, usage: crate::execution::UsageSink) -> ExecRequest {
    let op = s["op"].as_str().unwrap();
    let source = Format::parse(s["source"].as_str().unwrap()).unwrap();
    let payload = Bytes::from(s["payload"].as_str().unwrap().to_owned());
    let original = s["original"]
        .as_str()
        .map(|o| Bytes::from(o.to_owned()))
        .unwrap_or_default();
    let mut headers = http::HeaderMap::new();
    for (k, v) in s["headers"].as_object().into_iter().flatten() {
        headers.insert(
            http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            v.as_str().unwrap().parse().unwrap(),
        );
    }
    // Go's executor-side fallback (EnsureSessionContext -> CanonicalSessionID).
    let session_body: &[u8] = if original.is_empty() { &payload } else { &original };
    let meta = crate::protocol::session::Meta {
        execution_session: s["execution_session"].as_str(),
        derived: s["derived_session"].as_str(),
    };
    let session = Some(crate::protocol::session::extract_session_id(
        &headers,
        session_body,
        &meta,
    ))
    .filter(|id| !id.is_empty())
    .map(|id| crate::protocol::session::bound_session_identity(&id));
    let model = s["model"].as_str().unwrap().to_owned();
    ExecRequest {
        operation: if op == "count" {
            Operation::CountTokens
        } else {
            Operation::Generate
        },
        source_format: source,
        response_format: response_format(s),
        requested_model: s["requested_model"].as_str().unwrap_or(&model).to_owned(),
        model,
        original_body: original,
        body: payload,
        stream: op == "stream",
        alt: s["alt"].as_str().map(str::to_owned),
        session,
        execution_session: s["execution_session"].as_str().map(str::to_owned),
        derived_session: s["derived_session"].as_str().map(str::to_owned),
        resolved_model: bound_model(&s["resolved_model"]),
        usage,
        request_path: s["request_path"].as_str().unwrap_or_default().to_owned(),
        headers,
        caller: Caller {
            principal: s["caller_key"].as_str().unwrap_or_default().to_owned(),
            source: "authorization",
            provider: String::new(),
        },
    }
}

fn error_json(e: &ExecError) -> Value {
    serde_json::json!({
        "status": e.status,
        "message": String::from_utf8_lossy(&e.body),
        "retry_after_ms": e.retry_after.map_or(-1, |d| d.as_millis() as i64),
        // The server's reading of IsCredentialScoped (classify::credential_scoped).
        "credential_scoped": e.scope == FailureScope::Credential && e.status == 429,
        // A plain Go error (no status) is a transport-scoped 500 here.
        "plain": e.scope == FailureScope::Transport,
    })
}

/// `scheme://authority` of every upstream URL goes to the capture server.
fn redirect(url: &str, addr: &str) -> String {
    let rest = &url[url.find("://").map_or(0, |i| i + 3)..];
    let path = rest.find('/').map_or("", |i| &rest[i..]);
    format!("http://{addr}{path}")
}

#[tokio::test]
async fn http_scenarios_preserve_routing_translation_and_accounting() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let scenarios = fixture["scenarios"].as_array().unwrap();
    assert!(scenarios.len() >= 80, "fixture lost scenarios");
    // Keep one executor so replay isolation is exercised across turns.
    let target: Arc<Mutex<String>> = Arc::default();
    let chosen: Arc<Mutex<Option<String>>> = Arc::default();
    let executor = {
        let (target, chosen) = (target.clone(), chosen.clone());
        XaiExecutor::default().with_url_rewrite(move |url| {
            *chosen.lock().unwrap() = Some(url.to_owned());
            redirect(url, &target.lock().unwrap())
        })
    };
    let mut failures = Vec::new();
    for s in scenarios {
        let name = s["name"].as_str().unwrap();
        let mock = match s.get("upstream").filter(|u| !u.is_null()) {
            Some(up) => Some(Mock::start(up).await),
            None => None,
        };
        let addr = mock.as_ref().map_or("127.0.0.1:9".to_owned(), |m| m.addr.clone());
        *target.lock().unwrap() = addr.clone();
        *chosen.lock().unwrap() = None;
        let mut cfg = Config::parse(&s["config"].as_str().unwrap_or_default().replace("UPSTREAM", &addr)).unwrap();
        cfg.auth_dir = std::env::temp_dir().join(format!("cpa-xai-http-{}", uuid::Uuid::new_v4()));
        let cred = credential(s, &cfg);
        let (recorder, capture, sink) = crate::providers::usage_testkit::sinks();
        let req = request(s, sink);
        let op = s["op"].as_str().unwrap();
        let path = s["request_path"].as_str().unwrap_or_default().to_owned();
        let result = match op {
            "images" => executor.images(&cred, req, &path, &cfg, None).await,
            "videos" => executor.videos(&cred, req, &path, &cfg).await,
            _ => {
                executor
                    .execute(&cred, req, &cfg, s["websocket"].as_bool() == Some(true))
                    .await
            }
        };
        let mut output = None;
        let mut chunks = Vec::new();
        let mut error = None;
        match result {
            Err(e) => error = Some(error_json(&e)),
            Ok(response) => match response.body {
                ResponseBody::Buffered(bytes) => output = Some(String::from_utf8_lossy(&bytes).into_owned()),
                ResponseBody::Stream(mut stream) => {
                    while let Some(item) = stream.next().await {
                        assert!(error.is_none(), "{name}: stream item after an error");
                        match item {
                            Ok(bytes) => chunks.push(String::from_utf8_lossy(&bytes).into_owned()),
                            Err(e) => error = Some(error_json(&e)),
                        }
                    }
                }
            },
        }
        let mut problems = Vec::new();
        let raw = mock.as_ref().and_then(|m| m.raw.lock().unwrap().clone());
        if let Some(raw) = mock.as_ref().and_then(Mock::request) {
            let url = chosen.lock().unwrap().clone().unwrap();
            assert_http_request(name, s, &cred, &url, &raw);
        }
        if op != "count"
            && let Some(problem) = crate::providers::usage_testkit::ttft_problem(
                &recorder.0.lock().unwrap(),
                raw.is_some(),
                raw.is_some() && !s["upstream"]["body"].as_str().unwrap_or_default().is_empty(),
            )
        {
            problems.push(problem);
        }
        // Capture: one logged attempt per request the upstream received, none otherwise.
        if let Some(raw) = raw {
            let up = &s["upstream"];
            let status = up["status"].as_u64().unwrap() as u16;
            let get = raw.starts_with(b"GET ");
            let attempt = HttpAttempt {
                raw: &raw,
                url: &redirect(chosen.lock().unwrap().as_deref().unwrap(), &addr),
                provider: "xai",
                credential: &cred,
                // The video poll is a GET that logs its payload.
                logged_body: get.then(|| s["payload"].as_str().unwrap()),
                status,
                headers: up["headers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|h| (h[0].as_str().unwrap().to_owned(), h[1].as_str().unwrap().to_owned()))
                    .collect(),
                body: up["body"].as_str().unwrap(),
                logged: if op == "stream" && (200..300).contains(&status) {
                    Logged::Lines
                } else {
                    Logged::Whole
                },
            };
            for problem in http_capture_problems(&capture.take(), &attempt) {
                problems.push(problem);
            }
        } else {
            let events = capture.take();
            if !events.is_empty() {
                problems.push(format!("capture without an upstream attempt: {events:?}"));
            }
        }
        assert_http_outcome(name, s, output.as_deref(), &chunks, error.as_ref());
        if op != "count" {
            let reports = recorder.0.lock().unwrap();
            let usage = published(&reports, error.is_some());
            if let Some(usage) = usage {
                assert_eq!(usage["failed"], error.is_some(), "{name}: accounting outcome");
                if error.is_none()
                    && let Some(peer) = peer_messages(s["upstream"]["body"].as_str().unwrap_or_default())
                        .into_iter()
                        .find(|e| matches!(e["type"].as_str(), Some("response.completed" | "response.incomplete")))
                {
                    let tokens = &peer["response"]["usage"];
                    for (local, upstream) in [
                        ("input", "input_tokens"),
                        ("output", "output_tokens"),
                        ("total", "total_tokens"),
                    ] {
                        if let Some(value) = tokens.get(upstream) {
                            assert_eq!(&usage[local], value, "{name}: {local}");
                        }
                    }
                }
            }
        }
        if !problems.is_empty() {
            failures.push(format!("{name}:\n{}", problems.join("\n")));
        }
    }
    assert!(
        failures.is_empty(),
        "{} scenarios differ:\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

/// Go marks the first response byte only on a body read that returns data
/// (`usageTTFTReadCloser`): an empty error body starts the round trip and never marks it.
#[tokio::test]
async fn empty_error_body_starts_ttft_without_first_byte() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let s = fixture["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "api_key_default_base")
        .unwrap();
    let mock = Mock::start(&serde_json::json!({"status": 500, "headers": [], "body": ""})).await;
    let addr = mock.addr.clone();
    let executor = XaiExecutor::default().with_url_rewrite(move |url| redirect(url, &addr));
    let cfg = Config {
        auth_dir: std::env::temp_dir().join(format!("cpa-xai-private-{}", uuid::Uuid::new_v4())),
        ..Default::default()
    };
    let cred = credential(s, &cfg);
    let (recorder, _capture, sink) = crate::providers::usage_testkit::sinks();
    let error = executor
        .execute(&cred, request(s, sink), &cfg, false)
        .await
        .err()
        .expect("the 500");
    assert_eq!(error.status, 500);
    let reports = recorder.0.lock().unwrap();
    assert_eq!(
        crate::providers::usage_testkit::ttft_problem(&reports, true, false),
        None
    );
}

/// Go's TestXAISpeechRequestURLStaysOnOfficialAPI and
/// TestXAIExecutorExecuteSpeechUpstreamErrorScope: an OAuth credential on the chat proxy
/// still sends speech to the official `/tts`, every error keeps the upstream body, and
/// only a 404 that names no model problem is request-scoped (an explicit request scope
/// without upstream headers: neither rotated nor cooled).
#[tokio::test]
async fn speech_errors_preserve_body_and_classify_voice_vs_model_failures() {
    let cases: [(u16, &str, u16, bool); 15] = [
        (400, r#"{"error":"invalid language"}"#, 400, false),
        (400, r#"{"error":"requested model is not supported"}"#, 400, false),
        (404, r#"{"error":"voice not found"}"#, 404, true),
        (
            404,
            r#"{"error":{"code":"model_not_found","message":"The model grok-tts does not exist"}}"#,
            404,
            false,
        ),
        (
            404,
            r#"{"code":"model_not_found","error":"model unavailable"}"#,
            404,
            false,
        ),
        (
            404,
            r#"{"error":"The model grok-tts is not available for your account"}"#,
            404,
            false,
        ),
        (404, "model is not available", 404, false),
        (
            404,
            r#"{"error":{"message":"Unsupported model: grok-tts"}}"#,
            404,
            false,
        ),
        (422, r#"{"error":"text too long"}"#, 422, false),
        (
            422,
            r#"{"code":"model_not_supported","error":"model is not supported"}"#,
            422,
            false,
        ),
        (401, r#"{"error":"unauthorized"}"#, 401, false),
        (
            403,
            r#"{"code":"bad-credentials","error":"access token could not be validated"}"#,
            401,
            false,
        ),
        (403, r#"{"error":"forbidden"}"#, 403, false),
        (429, r#"{"error":"rate limited"}"#, 429, false),
        (500, r#"{"error":"boom"}"#, 500, false),
    ];
    let s = serde_json::json!({
        "config_auth": -1,
        "metadata": {"access_token": "xai-token"},
        "attributes": {"base_url": CLI_CHAT_PROXY_BASE_URL, "auth_kind": "oauth"},
        "op": "execute",
        "source": "openai",
        "payload": r#"{"text":"hello","voice_id":"nope","language":"auto"}"#,
        "model": "grok-tts",
    });
    let cfg = Config {
        auth_dir: std::env::temp_dir().join(format!("cpa-xai-private-{}", uuid::Uuid::new_v4())),
        ..Default::default()
    };
    for (status, body, want, scoped) in cases {
        let reply =
            serde_json::json!({"status": status, "headers": [["Content-Type", "application/json"]], "body": body});
        let mock = Mock::start(&reply).await;
        let (addr, target) = (mock.addr.clone(), Arc::new(Mutex::new(String::new())));
        let seen = target.clone();
        let executor = XaiExecutor::default().with_url_rewrite(move |url| {
            *seen.lock().unwrap() = url.to_owned();
            redirect(url, &addr)
        });
        let (_recorder, _capture, sink) = crate::providers::usage_testkit::sinks();
        let error = executor
            .speech(&credential(&s, &cfg), request(&s, sink), &cfg)
            .await
            .err()
            .expect("an upstream error");
        assert_eq!(*target.lock().unwrap(), format!("{DEFAULT_API_BASE_URL}/tts"));
        assert_eq!(
            (error.status, String::from_utf8_lossy(&error.body).as_ref()),
            (want, body)
        );
        let request_scoped = error.scope == FailureScope::Request && error.headers.is_empty();
        assert_eq!(request_scoped, scoped, "{status} {body}");
    }
}

/// TestXAIExecutorCompactUsesCompactEndpoint (868e137): compact's deletes run before the
/// payload rules, so a configured `top_k` survives while the deleted fields stay gone.
#[tokio::test]
async fn compact_keeps_a_configured_top_k() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let s = fixture["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "compact_oauth_uses_official_api")
        .unwrap();
    let mock = Mock::start(&s["upstream"]).await;
    let addr = mock.addr.clone();
    let executor = XaiExecutor::default().with_url_rewrite(move |url| redirect(url, &addr));
    let mut cfg =
        Config::parse("payload:\n  override:\n    - models: [{name: \"*\"}]\n      params: {top_k: 10}\n").unwrap();
    cfg.auth_dir = std::env::temp_dir().join(format!("cpa-xai-private-{}", uuid::Uuid::new_v4()));
    let cred = credential(s, &cfg);
    executor
        .execute(&cred, request(s, Default::default()), &cfg, false)
        .await
        .unwrap();
    let wire = mock.request().unwrap();
    let body = wire.split_once("\r\n\r\n").unwrap().1.as_bytes().to_vec();
    assert_eq!(gj::get(&body, "top_k").int(), 10, "{wire}");
    for absent in ["stream", "tools", "max_output_tokens", "temperature", "top_p", "stop"] {
        assert!(!gj::get(&body, absent).exists(), "{absent}: {wire}");
    }
}

/// Go ignores a refusal only the original (baseline) body hits
/// (TranslateRequestPairWithAPIKeyModelCompatibilityAndUpdateIntent): the working body is
/// the one sent, and only its own refusal refuses the request.
#[tokio::test]
async fn a_refused_baseline_alone_does_not_refuse_the_request() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let mut s = fixture["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "claude_execute")
        .unwrap()
        .clone();
    s["original"] = Value::String(
        r#"{"model":"grok-4.3","max_tokens":100,"messages":[{"role":"user","content":[{"type":"container_upload","source":{"type":"file","file_id":"f1"}}]}]}"#.into(),
    );
    let run = |s: Value| async move {
        let mock = Mock::start(&s["upstream"]).await;
        let addr = mock.addr.clone();
        let executor = XaiExecutor::default().with_url_rewrite(move |url| redirect(url, &addr));
        let cfg = Config {
            auth_dir: std::env::temp_dir().join(format!("cpa-xai-private-{}", uuid::Uuid::new_v4())),
            ..Default::default()
        };
        let cred = credential(&s, &cfg);
        let ok = executor
            .execute(&cred, request(&s, Default::default()), &cfg, false)
            .await
            .is_ok();
        (ok, mock.request())
    };
    let (ok, wire) = run(s.clone()).await;
    assert!(ok);
    assert!(wire.unwrap().contains("hello"));
    s["payload"] = s["original"].clone();
    assert_eq!(run(s).await, (false, None));
}

// --- buffered terminal failures (v8.0.67 949b43b3, issue #84) ------------------------------

const CREATED_EVENT: &str =
    r#"{"type":"response.created","response":{"id":"resp_1","status":"in_progress","model":"grok-4.3"}}"#;
const COMPLETED_READY: &str = r#"{"type":"response.completed","response":{"id":"resp_1","object":"response","status":"completed","model":"grok-4.3","output":[{"id":"msg_1","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"ready"}]}],"usage":{"input_tokens":5,"output_tokens":1,"total_tokens":6}}}"#;

/// One loopback upstream connection: reads the request, then writes `pieces` one at a
/// time, flushed and apart, so the client reads them as separate segments. `length`
/// overrides the announced Content-Length (a larger one ends the body early).
async fn segmented_upstream(content_type: &str, pieces: Vec<Vec<u8>>, length: Option<usize>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let total: usize = pieces.iter().map(Vec::len).sum();
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nX-Request-Id: req-1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        length.unwrap_or(total)
    );
    tokio::spawn(async move {
        let Ok((socket, _)) = listener.accept().await else {
            return;
        };
        let (read, mut write) = socket.into_split();
        let mut reader = BufReader::new(read);
        let mut length = 0;
        loop {
            let mut line = Vec::new();
            if reader.read_until(b'\n', &mut line).await.unwrap_or(0) == 0 {
                return;
            }
            let text = String::from_utf8_lossy(&line).to_ascii_lowercase();
            if let Some(v) = text.strip_prefix("content-length:") {
                length = v.trim().parse().unwrap_or(0);
            }
            if line == b"\r\n" {
                break;
            }
        }
        let mut payload = vec![0; length];
        let _ = reader.read_exact(&mut payload).await;
        let _ = write.write_all(head.as_bytes()).await;
        for piece in pieces {
            let _ = write.write_all(&piece).await;
            let _ = write.flush().await;
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let _ = write.shutdown().await;
    });
    addr
}

/// Splits `body` at each `|` (a marker, never part of the body itself).
fn segments(body: &str) -> Vec<Vec<u8>> {
    body.split('|').map(|p| p.as_bytes().to_vec()).collect()
}

struct Outcome {
    result: Result<ExecResponse, ExecError>,
    usage: Option<Value>,
}

/// A non-streaming Responses request with `payload` against an upstream answering 200
/// with `pieces`, through an API-key credential and a private recorder.
async fn buffered_turn(payload: &str, content_type: &str, pieces: Vec<Vec<u8>>, length: Option<usize>) -> Outcome {
    let addr = segmented_upstream(content_type, pieces, length).await;
    let s = serde_json::json!({
        "config_auth": -1,
        "attributes": {"api_key": "sk-fake-xai"},
        "model": "grok-4.3",
        "payload": payload,
        "source": "openai-response",
        "op": "execute",
    });
    let executor = XaiExecutor::default().with_url_rewrite(move |url| redirect(url, &addr));
    let cfg = Config {
        auth_dir: std::env::temp_dir().join(format!("cpa-xai-private-{}", uuid::Uuid::new_v4())),
        ..Default::default()
    };
    let (recorder, _capture, sink) = crate::providers::usage_testkit::sinks();
    let result = executor
        .execute(&credential(&s, &cfg), request(&s, sink), &cfg, false)
        .await;
    let usage = published(&recorder.0.lock().unwrap(), result.is_err());
    Outcome { result, usage }
}

const PLAIN: &str = r#"{"model":"grok-4.3","input":"hi"}"#;
const WITH_PATCH: &str = r#"{"model":"grok-4.3","input":"hi","tools":[{"type":"custom","name":"apply_patch"}]}"#;

fn sse_error(o: Outcome) -> ExecError {
    match o.result {
        Ok(r) => panic!(
            "want an error, got {}",
            match r.body {
                ResponseBody::Buffered(b) => String::from_utf8_lossy(&b).into_owned(),
                ResponseBody::Stream(_) => "a stream".into(),
            }
        ),
        Err(e) => e,
    }
}

/// A body that ends without a terminal event is the provider's failure for this
/// request: 502, request-scoped with no upstream headers (classify's explicit request
/// fault: not retried elsewhere, no credential cooled). v8.0.35 answered 408 and cooled.
#[tokio::test]
async fn unterminated_and_empty_bodies_are_request_scoped_502() {
    let cases: [(&str, String, &str); 7] = [
        // Missing terminal event after a split frame.
        (
            PLAIN,
            format!(
                "data: {CREATED_EVENT}\n|\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"cu|t\"}}\n\n"
            ),
            crate::providers::xai_sse::INCOMPLETE_STREAM,
        ),
        // A response.completed frame cut short.
        (
            PLAIN,
            format!("data: {}", &COMPLETED_READY[..60]),
            crate::providers::xai_sse::INCOMPLETE_STREAM,
        ),
        // Empty 200 and a comment-only body.
        (PLAIN, String::new(), crate::providers::xai_sse::EMPTY_STREAM),
        (
            PLAIN,
            ": keep-alive\n\n".to_owned(),
            crate::providers::xai_sse::EMPTY_STREAM,
        ),
        // An apply_patch declaration alone is no patch activity.
        (WITH_PATCH, String::new(), crate::providers::xai_sse::EMPTY_STREAM),
        (
            WITH_PATCH,
            format!("data: {CREATED_EVENT}\n\n"),
            crate::providers::xai_sse::INCOMPLETE_STREAM,
        ),
        // A patch call cut short keeps the apply_patch error, request-scoped.
        (
            WITH_PATCH,
            format!(
                "data: {CREATED_EVENT}\n\ndata: {}\n\ndata: {}\n\n",
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"apply_patch","arguments":""}}"#,
                r#"{"type":"response.function_call_arguments.done","output_index":0,"item_id":"a","call_id":"c","arguments":"{\"input\":\"valid patch\"}"}"#
            ),
            crate::translation::APPLY_PATCH_UPSTREAM_ERROR,
        ),
    ];
    for (payload, body, want) in cases {
        let o = buffered_turn(payload, "text/event-stream", segments(&body), None).await;
        let usage = o.usage.clone();
        let e = sse_error(o);
        assert_eq!(
            (e.status, e.scope, String::from_utf8_lossy(&e.body).into_owned()),
            (502, FailureScope::Request, want.to_owned()),
            "{body:?}"
        );
        assert!(
            e.headers.is_empty(),
            "{body:?}: a local request fault carries no upstream headers"
        );
        assert_eq!(
            usage.as_ref().map(|u| u["failed"].clone()),
            Some(Value::Bool(true)),
            "{body:?}"
        );
    }
}

type Failure = (&'static str, String, u16, FailureScope, &'static str, Option<u64>);

/// Terminal failures carried by a 200 body take the status they name and stay upstream
/// errors (headers kept), so credential health follows the status: 401 refreshes, 429
/// cools the model with the free-usage window, a provider 502/503 cools the credential.
#[tokio::test]
async fn terminal_failures_in_a_200_body_keep_their_status() {
    // (content type, body, status, scope, client message, retry hint in ms)
    let cases: [Failure; 8] = [
        (
            "text/event-stream",
            "event: error\r\ndata: {\"type\":\"error\",\r\ndata: \"error\":{\"type\":\"invalid_request_error\",\"code\":\"invalid_argument\",\"message\":\"Multiline error.\"}}\r\n\r\n".to_owned(),
            400,
            FailureScope::Request,
            "Multiline error.",
            None,
        ),
        (
            "text/event-stream",
            format!("data: {CREATED_EVENT}\n\ndata: {}\n\n", r#"{"type":"response.failed","sequence_number":3,"response":{"id":"resp_1","status":"failed","error":{"type":"server_error","code":"server_error","message":"Upstream generation failed."}}}"#),
            502,
            FailureScope::Credential,
            "Upstream generation failed.",
            None,
        ),
        (
            "application/json",
            r#"{"code":"permission-denied","error":"Request blocked."}"#.to_owned(),
            403,
            FailureScope::Credential,
            "Request blocked.",
            None,
        ),
        (
            "text/event-stream",
            "data: {\"type\":\"error\",\"status\":403,\"error\":{\"code\":\"unauthenticated:bad-credentials\",\"message\":\"The OAuth2 access token could not be validated.\"}}\n\n".to_owned(),
            401,
            FailureScope::Credential,
            "The OAuth2 access token could not be validated.",
            None,
        ),
        (
            "text/event-stream",
            "data: {\"type\":\"error\",\"error\":{\"code\":\"subscription:free-usage-exhausted\",\"message\":\"You've used all the included free usage for model grok-4.3 for now.\"}}\n\n".to_owned(),
            429,
            FailureScope::Model,
            "You've used all the included free usage for model grok-4.3 for now.",
            Some(24 * 3600 * 1000),
        ),
        (
            "text/event-stream",
            "event: error\ndata: upstream gateway error\n\n".to_owned(),
            502,
            FailureScope::Credential,
            "upstream gateway error",
            None,
        ),
        (
            "text/event-stream",
            "data: {\"code\":\"invalid_argument\",\"message\":\"Invalid input.\"}\nevent: error\n\n".to_owned(),
            400,
            FailureScope::Request,
            "Invalid input.",
            None,
        ),
        (
            "text/event-stream",
            r#"{"error":{"message":"Upstream overloaded","type":"server_error","code":"server_is_overloaded"}}"#.to_owned(),
            503,
            FailureScope::Credential,
            "Upstream overloaded",
            None,
        ),
    ];
    for (content_type, body, status, scope, message, retry_ms) in cases {
        let o = buffered_turn(WITH_PATCH, content_type, segments(&body), None).await;
        let e = sse_error(o);
        let doc: Value = serde_json::from_slice(&e.body).unwrap_or_else(|_| panic!("{body:?}: {:?}", e.body));
        assert_eq!(
            (e.status, e.scope, doc["error"]["message"].as_str()),
            (status, scope, Some(message)),
            "{body:?}"
        );
        assert_eq!(e.retry_after.map(|d| d.as_millis() as u64), retry_ms, "{body:?}");
        assert_eq!(
            e.headers.get("x-request-id").map(|v| v.to_str().unwrap()),
            Some("req-1"),
            "{body:?}: the upstream answered"
        );
    }
}

/// Split segments, CRLF and multi-line data still complete a turn with its usage, and
/// an incomplete terminal event still answers.
#[tokio::test]
async fn split_crlf_and_incomplete_terminals_still_answer() {
    // The data lines split between two members (SSE joins them with `\n`).
    let at = COMPLETED_READY.find(r#""status":"completed""#).unwrap();
    let completed = format!(
        "event: response.created\r\ndata: {CREATED_EVENT}\r\n\r|\nevent: response.completed\r\ndata: {}\r\ndata: {}\r\n\r\n",
        &COMPLETED_READY[..at],
        &COMPLETED_READY[at..]
    );
    let incomplete = format!(
        "data: {CREATED_EVENT}\n\ndata: {}\n\n",
        r#"{"type":"response.incomplete","response":{"id":"resp_1","object":"response","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"model":"grok-4.3","output":[],"usage":{"input_tokens":5,"output_tokens":9,"total_tokens":14}}}"#
    );
    for (body, status, total) in [(completed, "completed", 6), (incomplete, "incomplete", 14)] {
        let o = buffered_turn(PLAIN, "text/event-stream", segments(&body), None).await;
        let usage = o.usage.clone().expect("usage published");
        let out = match o.result {
            Ok(ExecResponse {
                body: ResponseBody::Buffered(b),
                ..
            }) => b,
            Ok(_) => panic!("{body:?}: a stream"),
            Err(e) => panic!("{body:?}: {e:?}"),
        };
        let out: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(out["status"], status, "{out}");
        assert_eq!(
            (usage["failed"].clone(), usage["total"].clone()),
            (Value::Bool(false), total.into()),
            "{usage}"
        );
    }
}

/// Failures of the exchange itself keep their own classification: a body cut by the
/// connection is a transport fault, and an upstream 408 status still cools.
#[tokio::test]
async fn transport_faults_and_upstream_408_are_not_stream_faults() {
    let body = format!("data: {CREATED_EVENT}\n\n");
    let o = buffered_turn(PLAIN, "text/event-stream", segments(&body), Some(body.len() + 100)).await;
    let e = sse_error(o);
    assert_eq!(e.scope, FailureScope::Transport, "{e:?}");
    assert_ne!(&e.body[..], crate::providers::xai_sse::INCOMPLETE_STREAM.as_bytes());

    let mock = Mock::start(&serde_json::json!({"status": 408, "headers": [["Content-Type", "application/json"]], "body": r#"{"error":"request timeout"}"#})).await;
    let addr = mock.addr.clone();
    let s = serde_json::json!({
        "config_auth": -1, "attributes": {"api_key": "sk-fake-xai"}, "model": "grok-4.3",
        "payload": PLAIN, "source": "openai-response", "op": "execute",
    });
    let executor = XaiExecutor::default().with_url_rewrite(move |url| redirect(url, &addr));
    let cfg = Config {
        auth_dir: std::env::temp_dir().join(format!("cpa-xai-private-{}", uuid::Uuid::new_v4())),
        ..Default::default()
    };
    let (_recorder, _capture, sink) = crate::providers::usage_testkit::sinks();
    let e = executor
        .execute(&credential(&s, &cfg), request(&s, sink), &cfg, false)
        .await
        .err()
        .expect("the 408");
    assert_eq!((e.status, e.scope), (408, FailureScope::Credential));
}

// --- historical additional_tools deduplication (v8.0.67 0c61b12c, issue #86) -------------

/// Go TestXAIExecutorDeduplicatesToolsAcrossAdditionalTools: a namespaced tool in the
/// top-level tools and again in a historical `additional_tools` item reaches xAI once,
/// as the top-level declaration, for Responses and Codex clients, buffered and streamed;
/// the buffered answer restores the call to the client's namespace.
#[tokio::test]
async fn historical_additional_tools_are_sent_once() {
    let top = r#"{"type":"namespace","name":"clock","tools":[{"type":"function","name":"sleep","description":"top declaration","parameters":{"type":"object","properties":{"duration_ms":{"type":"string"}},"required":["duration_ms"]}}]}"#;
    let different = r#"{"type":"namespace","name":"clock","tools":[{"type":"function","name":"sleep","description":"history declaration","parameters":{"type":"object","properties":{"seconds":{"type":"string"}},"required":["seconds"]}}]}"#;
    let reply = concat!(
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"name\":\"clock__sleep\",\"call_id\":\"call_1\",\"arguments\":\"{\\\"duration_ms\\\":\\\"10\\\"}\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"grok-4.7\",\"output\":[{\"type\":\"function_call\",\"name\":\"clock__sleep\",\"call_id\":\"call_1\",\"arguments\":\"{\\\"duration_ms\\\":\\\"10\\\"}\"}],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
    );
    for source in ["openai-response", "codex"] {
        for op in ["execute", "stream"] {
            for history in [top, different] {
                let label = format!("{source}/{op}/{}", if history == top { "same" } else { "different" });
                let mock = Mock::start(&serde_json::json!({
                    "status": 200,
                    "headers": [["Content-Type", "text/event-stream"]],
                    "body": reply,
                }))
                .await;
                let addr = mock.addr.clone();
                let executor = XaiExecutor::default().with_url_rewrite(move |url| redirect(url, &addr));
                let s = serde_json::json!({
                    "config_auth": -1,
                    "attributes": {"api_key": "sk-fake-xai"},
                    "model": "grok-4.7",
                    "payload": format!(r#"{{"model":"grok-4.7","tools":[{top}],"input":[{{"type":"additional_tools","role":"developer","tools":[{history}]}},{{"role":"user","content":"say hello"}}]}}"#),
                    "source": source,
                    "response": "openai-response",
                    "op": op,
                });
                let cfg = Config {
                    auth_dir: std::env::temp_dir().join(format!("cpa-xai-private-{}", uuid::Uuid::new_v4())),
                    ..Default::default()
                };
                let (_recorder, _capture, sink) = crate::providers::usage_testkit::sinks();
                let response = executor
                    .execute(&credential(&s, &cfg), request(&s, sink), &cfg, false)
                    .await
                    .unwrap_or_else(|e| panic!("{label}: {e:?}"));
                match response.body {
                    ResponseBody::Buffered(bytes) => {
                        let out: Value = serde_json::from_slice(&bytes).unwrap();
                        assert_eq!(
                            (&out["output"][0]["name"], &out["output"][0]["namespace"]),
                            (&Value::from("sleep"), &Value::from("clock")),
                            "{label}: {out}"
                        );
                    }
                    ResponseBody::Stream(mut stream) => {
                        while let Some(item) = stream.next().await {
                            item.unwrap_or_else(|e| panic!("{label}: {e:?}"));
                        }
                    }
                }
                let raw = mock.request().unwrap();
                let sent: Value = serde_json::from_str(raw.split_once("\r\n\r\n").unwrap().1).unwrap();
                let tools = sent["tools"].as_array().unwrap();
                assert_eq!(
                    tools.iter().filter(|t| t["name"] == "clock__sleep").count(),
                    1,
                    "{label}: {sent}"
                );
                assert_eq!(sent["tools"][0]["description"], "top declaration", "{label}");
                assert!(
                    sent["tools"][0]["parameters"]["properties"]["duration_ms"].is_object(),
                    "{label}"
                );
                assert!(
                    sent["input"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|i| i["type"] != "additional_tools"),
                    "{label}: {sent}"
                );
            }
        }
    }
}

// --- Responses reasoning for Claude clients (#168) ------------------------------------

/// A Claude client of Grok reads the same thinking blocks streamed and buffered: summary
/// parts joined with a blank line, and no thinking block for a whitespace-only summary
/// (a signed one keeps its signature-only block).
#[tokio::test]
async fn claude_clients_read_the_same_reasoning_streamed_and_buffered() {
    for op in ["stream", "execute"] {
        let mock = Mock::start(&serde_json::json!({
            "status": 200,
            "headers": [["Content-Type", "text/event-stream"]],
            "body": crate::providers::reasoning_testkit::answer("grok-4.3"),
        }))
        .await;
        let addr = mock.addr.clone();
        let executor = XaiExecutor::default().with_url_rewrite(move |url| redirect(url, &addr));
        let s = serde_json::json!({
            "config_auth": -1,
            "attributes": {"api_key": "sk-fake-xai"},
            "model": "grok-4.3",
            "payload": format!(r#"{{"model":"grok-4.3","max_tokens":16,"stream":{},"messages":[{{"role":"user","content":"hi"}}]}}"#, op == "stream"),
            "source": "claude",
            "op": op,
        });
        let cfg = Config {
            auth_dir: std::env::temp_dir().join(format!("cpa-xai-private-{}", uuid::Uuid::new_v4())),
            ..Default::default()
        };
        let (_recorder, _capture, sink) = crate::providers::usage_testkit::sinks();
        let response = executor
            .execute(&credential(&s, &cfg), request(&s, sink), &cfg, false)
            .await
            .unwrap_or_else(|e| panic!("{op}: {e:?}"));
        let answer = match response.body {
            ResponseBody::Buffered(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            ResponseBody::Stream(mut stream) => {
                let mut text = String::new();
                while let Some(item) = stream.next().await {
                    text.push_str(&String::from_utf8_lossy(
                        &item.unwrap_or_else(|e| panic!("{op}: {e:?}")),
                    ));
                }
                text
            }
        };
        assert_eq!(
            crate::providers::reasoning_testkit::blocks(&answer),
            crate::providers::reasoning_testkit::want(),
            "{op}: {answer}"
        );
    }
}

/// #178: a streamed answer marks TTFT at the first upstream event that carries output
/// (`helpers::responses_token_event`), not at the first body byte, `response.created`,
/// an announced item or an empty delta. The first body chunk is a frame, TTFT's
/// fallback. A buffered answer is read whole first: its TTFT stays the first body byte.
#[tokio::test]
async fn stream_ttft_is_marked_at_the_first_output_event() {
    let events = [
        CREATED_EVENT,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"m","role":"assistant","status":"in_progress","content":[]}}"#,
        r#"{"type":"response.output_text.delta","output_index":0,"item_id":"m","content_index":0,"delta":""}"#,
        r#"{"type":"response.output_text.delta","output_index":0,"item_id":"m","content_index":0,"delta":"hi"}"#,
        COMPLETED_READY,
    ];
    let body: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
    for op in ["stream", "execute"] {
        let mock = Mock::start(&serde_json::json!({
            "status": 200,
            "headers": [["Content-Type", "text/event-stream"]],
            "body": body,
        }))
        .await;
        let addr = mock.addr.clone();
        let executor = XaiExecutor::default().with_url_rewrite(move |url| redirect(url, &addr));
        let s = serde_json::json!({
            "config_auth": -1,
            "attributes": {"api_key": "sk-fake-xai"},
            "model": "grok-4.3",
            "payload": PLAIN,
            "source": "openai-response",
            "op": op,
        });
        let cfg = Config {
            auth_dir: std::env::temp_dir().join(format!("cpa-xai-private-{}", uuid::Uuid::new_v4())),
            ..Default::default()
        };
        let (recorder, _capture, sink) = crate::providers::usage_testkit::sinks();
        let response = executor
            .execute(&credential(&s, &cfg), request(&s, sink), &cfg, false)
            .await
            .unwrap_or_else(|e| panic!("{op}: {e:?}"));
        if let ResponseBody::Stream(mut stream) = response.body {
            while let Some(item) = stream.next().await {
                item.unwrap_or_else(|e| panic!("{op}: {e:?}"));
            }
        }
        let marks: Vec<&str> = recorder
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|r| match r {
                crate::providers::usage_testkit::Report::RoundTrip => Some("start"),
                crate::providers::usage_testkit::Report::FirstByte => Some("byte"),
                crate::providers::usage_testkit::Report::TokenEvent(false) => Some("frame"),
                crate::providers::usage_testkit::Report::TokenEvent(true) => Some("token"),
                _ => None,
            })
            .collect();
        let want: &[&str] = if op == "stream" {
            &["start", "frame", "frame", "frame", "frame", "token", "token"]
        } else {
            &["start", "byte"]
        };
        assert_eq!(marks, want, "{op}");
    }
}

fn peer_messages(body: &str) -> Vec<Value> {
    if let Ok(value) = serde_json::from_str(body) {
        return vec![value];
    }
    body.lines()
        .filter_map(|line| serde_json::from_str(line.strip_prefix("data:")?.trim()).ok())
        .collect()
}

fn assert_http_request(name: &str, s: &Value, credential: &Credential, url: &str, raw: &str) {
    let parsed = url::Url::parse(url).unwrap();
    assert!(matches!(parsed.scheme(), "http" | "https"), "{name}");
    assert_eq!(parsed.username(), "", "{name}: credentials cannot enter URL");
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    let headers: Vec<_> = head
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(n, v)| (n, v.trim()))
        .collect();
    let get = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| *v)
    };
    let token = creds(credential).0;
    assert_eq!(
        get("authorization"),
        (!token.is_empty()).then(|| format!("Bearer {token}")).as_deref(),
        "{name}"
    );
    let request_path = head.lines().next().unwrap().split_whitespace().nth(1).unwrap();
    assert_eq!(
        request_path,
        parsed[url::Position::BeforePath..].split('#').next().unwrap(),
        "{name}: selected path reaches peer"
    );
    if s["op"] == "videos" {
        if head.starts_with("POST ") {
            assert_eq!(body, s["payload"].as_str().unwrap(), "{name}: native video bytes");
        }
        assert!(parsed.path().contains("/videos"), "{name}: native video endpoint");
        return;
    }
    if s["op"] == "images" {
        assert!(parsed.path().contains("/images"), "{name}: native image endpoint");
        return;
    }
    assert!(
        parsed.path().ends_with("/responses") || parsed.path().ends_with("/responses/compact"),
        "{name}: Responses endpoint"
    );
    let value: Value = serde_json::from_str(body).unwrap_or_else(|e| panic!("{name}: upstream JSON: {e}: {body}"));
    assert!(value.get("input").is_some(), "{name}: input retained");
    if !parsed.path().ends_with("/compact") {
        assert_eq!(value["stream"], true, "{name}: upstream always streams");
    }
    if let Some(tools) = value["tools"].as_array() {
        assert!(tools.len() <= 200, "{name}: provider tool cap");
        assert!(
            !tools.iter().any(|t| t["type"] == "namespace"),
            "{name}: namespaces adapted"
        );
    }
    match name {
        "thinking_suffix_applied" => {
            assert_eq!(value["model"], "grok-4.3");
            assert_eq!(value["reasoning"]["effort"], "high");
        }
        "unsupported_reasoning_effort_omitted" => {
            // An unbound model has no catalog-backed restriction: retain the client's value.
            assert_eq!(value["reasoning"]["effort"], "high");
        }
        "composer_reuses_prompt_cache_key" => assert_eq!(value["prompt_cache_key"], "pck-123"),
        "payload_override_stop_dropped" => {
            assert_eq!(value["temperature"], 0.3);
            assert_eq!(value["stop"], serde_json::json!(["X"]));
        }
        "replay_other_caller_isolated" => assert!(
            !body.contains("encrypted-secret-1"),
            "other caller cannot receive replay"
        ),
        "replay_claude_turn2_injects" => assert!(
            value["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["type"] == "reasoning"),
            "reasoning is replayed for its owner"
        ),
        "custom_tool_history_normalized" => {
            assert!(
                !value["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|i| i["type"] == "custom_tool_call")
            );
            assert!(body.contains("ls -la"), "custom input survives adaptation");
        }
        "compact_oauth_uses_official_api" => assert_eq!(parsed.host_str(), Some("api.x.ai")),
        "oauth_default_chat_proxy" | "oauth_default_api_base_goes_to_chat_proxy" => {
            assert_ne!(parsed.host_str(), Some("api.x.ai"))
        }
        _ => {}
    }
}

fn assert_http_outcome(name: &str, s: &Value, output: Option<&str>, chunks: &[String], error: Option<&Value>) {
    let peer_status = s["upstream"]["status"].as_u64().unwrap_or(200);
    let expected_error = match name {
        "compact_stream_rejected" => Some(400),
        "bad_credentials_403_becomes_401" | "videos_error_bad_credentials" => Some(401),
        "apply_patch_invalid_history_input" => Some(500),
        "disconnected_before_completed"
        | "usage_done_ignored_execute"
        | "apply_patch_invalid_input_execute"
        | "apply_patch_invalid_input_stream"
        | "apply_patch_eof_without_completion_stream"
        | "apply_patch_eof_without_completion_execute" => Some(502),
        _ if peer_status >= 400 => Some(peer_status),
        _ => None,
    };
    assert_eq!(
        error.map(|e| e["status"].as_u64().unwrap()),
        expected_error,
        "{name}: outcome"
    );
    if let Some(error) = error {
        if peer_status >= 400 {
            assert_eq!(error["message"], s["upstream"]["body"], "{name}: peer error body");
        }
        assert!(
            !error["message"].as_str().unwrap().is_empty(),
            "{name}: actionable error"
        );
        return;
    }
    if s["op"] == "count" {
        let count: Value = serde_json::from_str(output.unwrap()).unwrap();
        assert!(
            count["input_tokens"].as_u64().unwrap_or(0) > 0
                || count["usage"]["input_tokens"].as_u64().unwrap_or(0) > 0
                || count["response"]["usage"]["input_tokens"].as_u64().unwrap_or(0) > 0,
            "{name}: positive token estimate"
        );
        return;
    }
    let joined = output.map(str::to_owned).unwrap_or_else(|| chunks.concat());
    let messages = peer_messages(&joined);
    assert!(!messages.is_empty(), "{name}: valid client JSON or SSE");
    if s["op"] == "images" || s["op"] == "videos" {
        assert_eq!(
            joined,
            s["upstream"]["body"].as_str().unwrap(),
            "{name}: native response bytes"
        );
        return;
    }
    if name.contains("apply_patch") && !name.contains("history") {
        assert!(
            joined.contains("custom_tool_call") || joined.contains("tool_use"),
            "{name}: patch call restored"
        );
        assert!(joined.contains("apply_patch"), "{name}: patch name restored");
    }
    if s["op"] == "stream" && response_format(s) == Format::Claude {
        assert_eq!(messages.first().unwrap()["type"], "message_start", "{name}");
        assert!(
            messages.iter().any(|m| m["type"] == "message_stop"),
            "{name}: terminal stop"
        );
    }
    if output.is_some() {
        assert!(messages[0].is_object(), "{name}: client object");
    }
    // Compare semantic text supplied by the peer, independently of translator framing.
    let peer = peer_messages(s["upstream"]["body"].as_str().unwrap_or_default());
    let mut all_client = Vec::new();
    for message in &messages {
        nodes(message, &mut all_client);
    }
    let client_strings: Vec<_> = all_client.iter().filter_map(|v| v.as_str()).collect();
    let client_text = all_client
        .iter()
        .filter_map(|v| v.get("text").and_then(Value::as_str))
        .collect::<String>();
    let text_survives =
        |text: &str| client_strings.iter().any(|actual| actual.contains(text)) || client_text.contains(text);
    if output.is_none() {
        // Streams expose deltas. A terminal response may carry a conflicting snapshot;
        // it does not retroactively rewrite text already delivered to a client.
        for event in &peer {
            if matches!(
                event["type"].as_str(),
                Some(
                    "response.output_text.delta"
                        | "response.reasoning_text.delta"
                        | "response.reasoning_summary_text.delta"
                )
            ) && let Some(text) = event["delta"].as_str().filter(|s| !s.is_empty())
            {
                assert!(text_survives(text), "{name}: peer delta survives: {text:?}");
            }
        }
    } else if let Some(terminal) = peer.iter().find(|event| {
        matches!(
            event["type"].as_str(),
            Some("response.completed" | "response.incomplete")
        )
    }) {
        let mut peer_nodes = Vec::new();
        nodes(&terminal["response"]["output"], &mut peer_nodes);
        for node in peer_nodes {
            if let Some(text) = node.get("text").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                assert!(text_survives(text), "{name}: completed peer text survives: {text:?}");
            }
        }
    }
    let find = |kind: &str, name: &str| all_client.iter().any(|v| v["type"] == kind && v["name"] == name);
    match name {
        "namespaces_flatten_and_restore" => {
            assert!(find("function_call", "read"), "client tool name restored");
            assert!(
                all_client.iter().any(|v| v["namespace"] == "files"),
                "namespace restored"
            );
        }
        "namespaces_fold_when_over_200_tools" | "namespaces_fold_stream" => {
            assert!(find("function_call", "t3"), "folded tool selected by envelope name");
            assert!(
                all_client.iter().any(|v| v["namespace"] == "big"),
                "folded namespace restored"
            );
        }
        "client_web_search_function_aliased" | "client_web_search_alias_restored_in_stream" => assert!(
            find("function_call", "web_search"),
            "client name restored after reserved-name alias"
        ),
        "x_search_internal_calls_filtered" => assert!(
            !find("function_call", "x_keyword_search"),
            "internal search cannot become client tool calls"
        ),
        "x_search_stream_keeps_client_same_name_tool" => {
            assert!(find("function_call", "x_keyword_search"), "declared client tool kept");
            assert!(
                !find("function_call", "x_semantic_search"),
                "undeclared internal search hidden"
            );
        }
        "reasoning_text_events_normalized_stream" | "reasoning_output_normalized_execute" => {
            assert!(
                all_client
                    .iter()
                    .any(|v| v["type"] == "summary_text" || v["type"] == "response.reasoning_summary_text.delta"),
                "reasoning exposes client summary grammar"
            );
            assert!(
                !all_client
                    .iter()
                    .any(|v| v["type"] == "reasoning_text" || v["type"] == "response.reasoning_text.delta")
            );
        }
        _ => {}
    }
}

fn nodes<'a>(value: &'a Value, out: &mut Vec<&'a Value>) {
    out.push(value);
    match value {
        Value::Object(fields) => {
            for child in fields.values() {
                nodes(child, out);
            }
        }
        Value::Array(items) => {
            for child in items {
                nodes(child, out);
            }
        }
        _ => {}
    }
}
