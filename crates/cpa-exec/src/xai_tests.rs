//! Differential tests against `tests/fixtures/xai_go.json`, produced by the real Go
//! XAIExecutor (tests/reference/xai). Scenarios run in order on one executor, as the Go
//! generator runs them in one process, so the reasoning replay cache carries from one
//! turn to the next. Each scenario replays the scripted upstream answer and compares the
//! URL the executor chose, the raw upstream request and the executor's output. Expected
//! values come only from Go.

use std::sync::{Arc, Mutex};

use cpa_core::exec::Caller;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use super::*;
use crate::usage_testkit::{HttpAttempt, Logged, http_capture_problems, published};

const FIXTURE: &str = include_str!("../tests/fixtures/xai_go.json");
/// The scenarios the payload barrier changed, recorded at v8.0.35; they replace their
/// 6fecc6e recordings.
const FIXTURE_V8_0_35: &str = include_str!("../tests/fixtures/xai_go_v8.0.35.json");

/// Recordings whose buffered body ends without a terminal event: v8.0.35 answered 408
/// and cooled the credential; this fork follows v8.0.67 (949b43b3) with a request-scoped
/// 502 (docs/DIFFERENCES-FROM-GO.md, "xAI non-streaming responses"). The rest of each
/// recording (request, usage record) still compares.
const V8_0_67_ENDINGS: &[&str] = &["disconnected_before_completed", "usage_done_ignored_execute"];

/// Shared helpers whose real port has not landed: scenarios that need them are skipped
/// and listed, so integration can remove an entry and see the scenario run.
const PENDING: &[&str] = &[];

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
        let xai: Vec<Credential> = cpa_core::config::credentials::from_config(cfg)
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
fn bound_model(v: &Value) -> Option<cpa_core::exec::ResolvedModel> {
    Some(cpa_core::exec::ResolvedModel {
        info: cpa_core::registry::ModelInfo::from_raw(v.as_object()?.clone()).expect("resolved model info"),
        source: cpa_core::exec::ResolvedSource::ApiKey,
    })
}

fn request(s: &Value, usage: cpa_core::exec::UsageSink) -> ExecRequest {
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
    let meta = cpa_common::session::Meta {
        execution_session: s["execution_session"].as_str(),
        derived: s["derived_session"].as_str(),
    };
    let session = Some(cpa_common::session::extract_session_id(&headers, session_body, &meta))
        .filter(|id| !id.is_empty())
        .map(|id| cpa_common::session::bound_session_identity(&id));
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
async fn go_reference_scenarios() {
    let fixture = crate::payload_barrier::with_v8_0_35(FIXTURE, FIXTURE_V8_0_35, "scenarios");
    let scenarios = fixture["scenarios"].as_array().unwrap();
    assert!(scenarios.len() >= 80, "fixture lost scenarios");
    // One executor for all scenarios: Go's replay cache is process-wide.
    let target: Arc<Mutex<String>> = Arc::default();
    let chosen: Arc<Mutex<Option<String>>> = Arc::default();
    let executor = {
        let (target, chosen) = (target.clone(), chosen.clone());
        XaiExecutor::default().with_url_rewrite(move |url| {
            *chosen.lock().unwrap() = Some(url.to_owned());
            redirect(url, &target.lock().unwrap())
        })
    };
    let mut skipped = Vec::new();
    let mut failures = Vec::new();
    for s in scenarios {
        let name = s["name"].as_str().unwrap();
        let needs: Vec<&str> = s["needs"]
            .as_array()
            .map(|n| n.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if needs.iter().any(|n| PENDING.contains(n)) {
            skipped.push(name);
            continue;
        }
        let mock = match s.get("upstream").filter(|u| !u.is_null()) {
            Some(up) => Some(Mock::start(up).await),
            None => None,
        };
        let addr = mock.as_ref().map_or("127.0.0.1:9".to_owned(), |m| m.addr.clone());
        *target.lock().unwrap() = addr.clone();
        *chosen.lock().unwrap() = None;
        let cfg = Config::parse(&s["config"].as_str().unwrap_or_default().replace("UPSTREAM", &addr)).unwrap();
        let cred = credential(s, &cfg);
        let (recorder, capture, sink) = crate::usage_testkit::sinks();
        let req = request(s, sink);
        let op = s["op"].as_str().unwrap();
        let path = s["request_path"].as_str().unwrap_or_default().to_owned();
        let result = match op {
            "images" => executor.images(&cred, req, &path, &cfg).await,
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
        if let Some(mock) = &mock {
            let url = chosen.lock().unwrap().clone();
            if url.as_deref() != s["url"].as_str() {
                problems.push(format!("url: {url:?} != {:?}", s["url"]));
            }
            let got = mock.request();
            if got.as_deref() != s["request"].as_str() {
                problems.push(format!(
                    "request:\n  rust: {got:?}\n  go:   {:?}",
                    s["request"].as_str()
                ));
            }
        }
        if op != "count"
            && let Some(problem) = crate::usage_testkit::ttft_problem(
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
                // The URL Go chose, as the rewrite sent it to the mock.
                url: &redirect(s["url"].as_str().unwrap_or_default(), &addr),
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
        if output.as_deref() != s["output"].as_str() {
            problems.push(format!(
                "output:\n  rust: {output:?}\n  go:   {:?}",
                s["output"].as_str()
            ));
        }
        let go_chunks: Vec<&str> = s["chunks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let want: Vec<String> = if response_format(s) == Format::OpenAIResponse {
            let mut framer = cpa_translate::stream::ResponsesFramer::default();
            let mut frames: Vec<Bytes> = Vec::new();
            for chunk in &go_chunks {
                frames.extend(framer.write(chunk.as_bytes()));
            }
            frames.extend(framer.flush());
            frames.iter().map(|f| String::from_utf8_lossy(f).into_owned()).collect()
        } else {
            go_chunks
                .iter()
                .filter_map(|c| cpa_translate::stream::frame(response_format(s), c.as_bytes()))
                .map(|f| String::from_utf8_lossy(&f).into_owned())
                .collect()
        };
        if chunks != want {
            problems.push(format!("frames:\n  rust: {chunks:?}\n  go:   {want:?}"));
        }
        // Go publishes at most one record per attempt; the server publishes what the
        // executor's reports say (the rules of cpa-server's usage_record.rs, including
        // `discard` for an error Go raises before its reporter exists), compared with
        // Go's record or its absence. The server tracks no record for CountTokens.
        let want = s.get("usage").filter(|u| !u.is_null()).cloned();
        let reports = recorder.0.lock().unwrap();
        if op != "count" {
            let got = published(&reports, error.is_some());
            if got != want {
                problems.push(format!("usage: {got:?} != {want:?}"));
            }
        }
        drop(reports);
        let mut want_error = s["error"].clone();
        if !want_error.is_null() {
            let plain = want_error["status"] == 0;
            if plain {
                // A plain Go error: the handler answers 500.
                want_error["status"] = 500.into();
            }
            want_error["plain"] = plain.into();
            if V8_0_67_ENDINGS.contains(&name) {
                assert_eq!(want_error["status"], 408, "{name}: recording changed");
                want_error["status"] = 502.into();
                want_error["message"] = crate::xai_sse::INCOMPLETE_STREAM.into();
            }
        }
        let error = error.unwrap_or(Value::Null);
        if error != want_error {
            problems.push(format!("error: {error} != {want_error}"));
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
    assert_eq!(skipped, Vec::<&str>::new());
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
    let cfg = Config::default();
    let cred = credential(s, &cfg);
    let (recorder, _capture, sink) = crate::usage_testkit::sinks();
    let error = executor
        .execute(&cred, request(s, sink), &cfg, false)
        .await
        .err()
        .expect("the 500");
    assert_eq!(error.status, 500);
    let reports = recorder.0.lock().unwrap();
    assert_eq!(crate::usage_testkit::ttft_problem(&reports, true, false), None);
}

/// Go's TestXAISpeechRequestURLStaysOnOfficialAPI and
/// TestXAIExecutorExecuteSpeechUpstreamErrorScope: an OAuth credential on the chat proxy
/// still sends speech to the official `/tts`, every error keeps the upstream body, and
/// only a 404 that names no model problem is request-scoped (an explicit request scope
/// without upstream headers: neither rotated nor cooled).
#[tokio::test]
async fn speech_errors_keep_go_scope() {
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
    let cfg = Config::default();
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
        let (_recorder, _capture, sink) = crate::usage_testkit::sinks();
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
    let cfg =
        Config::parse("payload:\n  override:\n    - models: [{name: \"*\"}]\n      params: {top_k: 10}\n").unwrap();
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
        let cfg = Config::parse("").unwrap();
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
    let cfg = Config::default();
    let (recorder, _capture, sink) = crate::usage_testkit::sinks();
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
            crate::xai_sse::INCOMPLETE_STREAM,
        ),
        // A response.completed frame cut short.
        (
            PLAIN,
            format!("data: {}", &COMPLETED_READY[..60]),
            crate::xai_sse::INCOMPLETE_STREAM,
        ),
        // Empty 200 and a comment-only body.
        (PLAIN, String::new(), crate::xai_sse::EMPTY_STREAM),
        (PLAIN, ": keep-alive\n\n".to_owned(), crate::xai_sse::EMPTY_STREAM),
        // An apply_patch declaration alone is no patch activity.
        (WITH_PATCH, String::new(), crate::xai_sse::EMPTY_STREAM),
        (
            WITH_PATCH,
            format!("data: {CREATED_EVENT}\n\n"),
            crate::xai_sse::INCOMPLETE_STREAM,
        ),
        // A patch call cut short keeps the apply_patch error, request-scoped.
        (
            WITH_PATCH,
            format!(
                "data: {CREATED_EVENT}\n\ndata: {}\n\ndata: {}\n\n",
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"apply_patch","arguments":""}}"#,
                r#"{"type":"response.function_call_arguments.done","output_index":0,"item_id":"a","call_id":"c","arguments":"{\"input\":\"valid patch\"}"}"#
            ),
            cpa_translate::APPLY_PATCH_UPSTREAM_ERROR,
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
    assert_ne!(&e.body[..], crate::xai_sse::INCOMPLETE_STREAM.as_bytes());

    let mock = Mock::start(&serde_json::json!({"status": 408, "headers": [["Content-Type", "application/json"]], "body": r#"{"error":"request timeout"}"#})).await;
    let addr = mock.addr.clone();
    let s = serde_json::json!({
        "config_auth": -1, "attributes": {"api_key": "sk-fake-xai"}, "model": "grok-4.3",
        "payload": PLAIN, "source": "openai-response", "op": "execute",
    });
    let executor = XaiExecutor::default().with_url_rewrite(move |url| redirect(url, &addr));
    let cfg = Config::default();
    let (_recorder, _capture, sink) = crate::usage_testkit::sinks();
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
                let cfg = Config::default();
                let (_recorder, _capture, sink) = crate::usage_testkit::sinks();
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
