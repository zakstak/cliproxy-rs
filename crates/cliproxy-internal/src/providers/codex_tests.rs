//! Executor contract checks with local upstreams and synthetic credentials. Historical
//! client requests and mock replies remain input-only regression stimuli; recorded Go
//! output is not an expected value. Images and SIWC inputs are authored in Rust.

use super::*;
use crate::execution::Caller;
use crate::providers::codex_testkit::{Captured, INPUTS, Mock, Reply, Tap, Wiretap};
use serde_json::Value;
use std::path::Path;

fn credential(case: &Value, base_url: &str) -> Credential {
    let mut credential = Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/codex-fixture.json"),
        case["metadata"].as_object().cloned().unwrap_or_default(),
    )
    .unwrap_or_else(|| Credential {
        id: "codex-fixture.json".into(),
        provider: "codex".into(),
        source: crate::credential::Source::File("/fake/codex-fixture.json".into()),
        disabled: false,
        label: "codex".into(),
        attributes: Default::default(),
        metadata: Default::default(),
        revision: 0,
    });
    credential.provider = "codex".into();
    credential.attributes.insert("base_url".into(), base_url.into());
    for (k, v) in case["attributes"].as_object().into_iter().flatten() {
        credential.attributes.insert(k.clone(), v.as_str().unwrap().into());
    }
    credential
}

fn request(case: &Value) -> ExecRequest {
    let source = Format::parse(case["source"].as_str().unwrap()).unwrap();
    let response = case["response"].as_str().and_then(Format::parse).unwrap_or(source);
    let mut headers = HeaderMap::new();
    for (k, v) in case["headers"].as_object().into_iter().flatten() {
        headers.insert(
            http::HeaderName::try_from(k.as_str()).unwrap(),
            v.as_str().unwrap().parse().unwrap(),
        );
    }
    let mut body = Bytes::from(case["payload"].as_str().unwrap().to_owned());
    // A WebSocket turn reaches the executor as the client's `response.create` frame; Go's
    // recorded turn body is the frame without its tag.
    if case["exec_metadata"]["execution_session_id"].as_str().is_some()
        && matches!(source, Format::Codex | Format::OpenAIResponse)
        && let Ok(frame) = crate::protocol::codex_body::CodexBody::parse(&body)
        && frame.get("type").is_none()
    {
        body = Bytes::from(frame.with_member("type", r#""response.create""#));
    }
    // Go's executor-side fallback (EnsureSessionContext -> CanonicalSessionID) for a
    // request without conductor metadata, which is how the fixtures were generated: the
    // canonical session from headers and body, with no derived identity.
    let session = Some(crate::protocol::session::extract_session_id(
        &headers,
        &body,
        &Default::default(),
    ))
    .filter(|s| !s.is_empty())
    .map(|s| crate::protocol::session::bound_session_identity(&s));
    ExecRequest {
        operation: Operation::Generate,
        source_format: source,
        response_format: response,
        requested_model: case["model"].as_str().unwrap().into(),
        model: case["model"].as_str().unwrap().into(),
        original_body: body.clone(),
        body,
        stream: case["stream"].as_bool().unwrap_or(false),
        alt: case["alt"].as_str().map(str::to_owned),
        session,
        headers,
        execution_session: None,
        derived_session: None,
        resolved_model: None,
        usage: Default::default(),
        request_path: String::new(),
        caller: Caller {
            principal: "client-key-FAKE".into(),
            source: "authorization",
            provider: String::new(),
        },
    }
}

fn reply(case: &Value) -> Reply {
    Reply {
        status: case["upstream_status"].as_u64().unwrap() as u16,
        headers: vec![
            ("content-type".into(), case["upstream_type"].as_str().unwrap().into()),
            ("x-codex-primary-used-percent".into(), "42".into()),
        ],
        body: case["upstream_body"].as_str().unwrap().into(),
    }
}

/// The proxy's own identity, spelled out here rather than taken from the implementation.
const PROXY_UA: &str = concat!("cliproxy-rs/", env!("CARGO_PKG_VERSION"));

async fn run(
    executor: &CodexExecutor,
    case: &Value,
    mock: &Mock,
) -> (Result<ExecResponse, ExecError>, Vec<Result<Bytes, ExecError>>) {
    let credential = credential(case, &mock.url);
    let config_text = case["config"].as_str().filter(|s| !s.trim().is_empty()).unwrap_or("{}");
    let cfg = Config::parse(&config_text.replace("{{base_url}}", &mock.url)).unwrap();
    let mut req = request(case);
    if case["resolved_compat"] == true {
        // Go's conductor binds the configured model (`resolved_api_key_model_info`).
        let raw = serde_json::json!({"id": req.model, "is_compat": true});
        req.resolved_model = Some(crate::execution::ResolvedModel {
            info: crate::catalog::ModelInfo::from_raw(raw.as_object().unwrap().clone()).unwrap(),
            source: crate::execution::ResolvedSource::ApiKey,
        });
    }
    let ws_session = case["exec_metadata"]["execution_session_id"].as_str();
    let result = match ws_session {
        Some(session) => ws_http(executor, &credential, &cfg, req, session).await,
        None => executor.execute(&credential, req, &cfg).await,
    };
    let mut items = Vec::new();
    let result = match result {
        Ok(ExecResponse {
            status,
            headers,
            body: ResponseBody::Stream(mut stream),
            ..
        }) => {
            while let Some(item) = stream.next().await {
                items.push(item);
            }
            Ok(ExecResponse {
                status,
                headers,
                body: ResponseBody::Buffered(Bytes::new()),
                omitted: Default::default(),
            })
        }
        other => other,
    };
    (result, items)
}

fn executor() -> CodexExecutor {
    CodexExecutor::with_client(wreq::Client::new(), CodexOAuth::new(wreq::Client::new()))
}

/// The load error of a recorded case's config: a `codex-api-key` model marked
/// `is-compat` is refused at load (#28), so the case never reaches the executor.
fn config_refusal(case: &Value) -> Option<String> {
    let text = case["config"].as_str().filter(|s| !s.trim().is_empty()).unwrap_or("{}");
    Config::parse(&text.replace("{{base_url}}", "http://127.0.0.1:1"))
        .err()
        .map(|e| e.to_string())
}

/// `execute_in_session` on a credential without an upstream socket (#28): the frame as an
/// HTTP stream, or its refusal.
async fn ws_http(
    executor: &CodexExecutor,
    credential: &Credential,
    cfg: &Config,
    req: ExecRequest,
    session: &str,
) -> Result<ExecResponse, ExecError> {
    let view = View::new(credential);
    let settings = Settings::scoped(cfg, &view);
    settings.refuse_compat(&req, credential)?;
    match super::native::route_ws(&view, &req) {
        Some(profile) => executor.native_ws_http(&view, &settings, req, profile, session).await,
        None => Err(super::native::refused_frame(&req.body)),
    }
}

/// A recorded case that now takes the native HTTP route (#23): Go's bytes no longer
/// apply to it, the contract does (`docs/codex/CONTRACT.md` "Native HTTP route").
/// No request was recorded: the attempt ended before the send.
fn result_refused(taps: &[Tap]) -> bool {
    !taps.iter().any(|t| matches!(t, Tap::Request { .. }))
}

/// A recorded case the translated route serves (#27): a Claude or Chat Completions
/// request on a native profile.
fn translated_case(case: &Value, base_url: &str) -> bool {
    case["resolved_compat"] != true
        && case["exec_metadata"]["execution_session_id"].as_str().is_none()
        && super::translated::route(&View::new(&credential(case, base_url)), &request(case))
}

/// Translated route assertions inspect the provider request independently of the
/// converter. Historical client inputs remain useful without their recorded outputs.
fn assert_translated_contract(
    name: &str,
    case: &Value,
    result: &Result<ExecResponse, ExecError>,
    captured: &[Captured],
) {
    let req = request(case);
    let source: Value = serde_json::from_slice(&req.body).unwrap();
    let has_unrepresentable_thinking = source["messages"].as_array().into_iter().flatten().any(|message| {
        message["content"].as_array().is_some_and(|blocks| {
            blocks.iter().any(|block| {
                block["type"] == "thinking" && block["thinking"].as_str().is_some_and(|text| !text.is_empty())
            })
        })
    });
    if has_unrepresentable_thinking {
        let error = result
            .as_ref()
            .err()
            .expect("reasoning text has no lossless Codex request representation");
        assert_eq!((error.status, error.scope), (400, FailureScope::Request), "{name}");
        assert!(captured.is_empty(), "{name}: refusal sends nothing");
        return;
    }
    let sent = captured
        .first()
        .unwrap_or_else(|| panic!("{name}: valid translation must be sent"));
    let body: Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(sent.path, "/responses", "{name}");
    assert_eq!(body["model"], req.model, "{name}: routed model");
    assert_eq!(body["stream"], true, "{name}: provider SSE transport");
    assert_eq!(body["store"], false, "{name}: translated requests disable persistence");
    let input = body["input"].as_array().expect("translated message array");
    assert!(!input.is_empty(), "{name}");
    let source: Value = serde_json::from_slice(&req.body).unwrap();
    for message in source["messages"].as_array().into_iter().flatten() {
        if let Some(text) = message["content"].as_str() {
            assert!(
                input.iter().any(|item| item["content"]
                    .as_array()
                    .is_some_and(|parts| parts.iter().any(|part| part["text"].as_str() == Some(text)))),
                "{name}: translated text {text:?} survives: {body}"
            );
        }
    }
    for key in ["prompt_cache_key", "instructions", "reasoning"] {
        assert!(body.get(key).is_none(), "{name}: no synthesized {key}");
    }
    assert!(
        body["tools"].as_array().is_none_or(Vec::is_empty),
        "{name}: no injected tools"
    );
    assert!(!String::from_utf8_lossy(&sent.body).contains("cpa_codex_replay_turn"));
}

fn native_case(case: &Value, base_url: &str) -> bool {
    case["resolved_compat"] != true && CodexExecutor::native_http(&credential(case, base_url), &request(case))
}

/// The native contract for a recorded case: the upstream body is the client's with
/// only the permitted edits, a model suffix is refused before any upstream request, and
/// a successful stream reaches the client as the upstream sent it.
fn assert_native_contract(
    name: &str,
    case: &Value,
    result: &Result<ExecResponse, ExecError>,
    items: &[Result<Bytes, ExecError>],
    captured: &[Captured],
) {
    let req = request(case);
    if crate::protocol::thinking::parse_suffix(&req.model).has_suffix {
        let error = result
            .as_ref()
            .err()
            .unwrap_or_else(|| panic!("{name}: a suffix is refused"));
        assert_eq!(error.status, 400, "{name}");
        assert!(captured.is_empty(), "{name}: nothing is sent");
        return;
    }
    let ws_session = case["exec_metadata"]["execution_session_id"].as_str().is_some();
    use crate::protocol::codex_body::{CodexBody, Kind};
    if ws_session {
        let frame = CodexBody::parse(&req.body).unwrap();
        let continuation = frame.get("previous_response_id").is_some_and(|m| m.kind != Kind::Null);
        let prewarm = frame.get("generate").is_some_and(|m| m.kind == Kind::False);
        if continuation || prewarm {
            let error = result
                .as_ref()
                .err()
                .unwrap_or_else(|| panic!("{name}: a continuation or prewarm without a socket is refused"));
            assert_eq!(error.is_replay_required(), continuation, "{name}");
            assert!(captured.is_empty(), "{name}: nothing is sent");
            return;
        }
    }
    // The first hop: a redirect's later hops follow the HTTP client's method rules.
    let sent = captured
        .first()
        .unwrap_or_else(|| panic!("{name}: an upstream request"));
    let mut expected = req.body.to_vec();
    if ws_session {
        // The WebSocket frame's HTTP body: the frame without its `type` tag, asking for
        // the event stream.
        expected = CodexBody::parse(&req.body).unwrap().without_member("type");
        let untagged = CodexBody::parse(&expected).unwrap();
        if untagged.get("stream").map(|m| m.kind) != Some(Kind::True) {
            expected = untagged.with_member("stream", "true");
        }
    }
    let body = CodexBody::parse(&expected).unwrap();
    if body.get("model").and_then(|m| m.text()).as_deref() != Some(req.model.trim()) {
        expected = body.with_member("model", &serde_json::to_string(req.model.trim()).unwrap());
    }
    let oauth = !case["attributes"]["api_key"].as_str().is_some_and(|k| !k.is_empty());
    if oauth && !req.stream && req.alt.is_none() && !ws_session {
        let edited = expected.clone();
        let reread = CodexBody::parse(&edited).unwrap();
        if reread.get("stream").map(|m| m.raw) != Some(&b"true"[..]) {
            expected = reread.with_member("stream", "true");
        }
    }
    assert_eq!(
        String::from_utf8_lossy(&sent.body),
        String::from_utf8_lossy(&expected),
        "{name}: native upstream body"
    );
    let reply = reply(case);
    let sse = reply
        .headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("content-type") && v.starts_with("text/event-stream"));
    if req.stream && sse && reply.status == 200 && items.iter().all(Result::is_ok) && result.is_ok() {
        let out: Vec<u8> = items.iter().flat_map(|i| i.as_ref().unwrap().to_vec()).collect();
        // A WebSocket turn over HTTP gets each event's `data` payload as one frame (#28).
        let want = if ws_session {
            reply
                .body
                .split("\n\n")
                .filter_map(|event| {
                    event
                        .lines()
                        .find_map(|l| l.strip_prefix("data:"))
                        .map(|d| d.strip_prefix(' ').unwrap_or(d).to_owned())
                })
                .collect::<String>()
        } else {
            reply.body.clone()
        };
        assert_eq!(String::from_utf8_lossy(&out), want, "{name}: native events");
    }
}

#[tokio::test]
async fn historical_client_inputs_obey_native_translation_and_route_refusal_contracts() {
    let executor = executor();
    for case in INPUTS["executor"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let mock = Mock::start().await;
        let path = if case["alt"] == "responses/compact" {
            "/responses/compact"
        } else {
            "/responses"
        };
        if let Some(redirect) = case["redirect"].as_u64() {
            let moved = format!("/moved{path}");
            mock.script(
                path,
                vec![Reply {
                    status: redirect as u16,
                    headers: vec![("location".into(), moved.clone())],
                    body: String::new(),
                }],
            );
            mock.script(&moved, vec![reply(case)]);
        } else {
            mock.script(path, vec![reply(case)]);
        }
        if let Some(error) = config_refusal(case) {
            assert!(error.contains("is-compat is no longer supported"), "{name}: {error}");
            continue;
        }
        let (result, items) = run(&executor, case, &mock).await;
        let captured = mock.take();
        if case["resolved_compat"] == true {
            // `is-compat` chose Go's compatibility rewrites; no Codex route honours it, so
            // the request is refused for the credential before anything is sent (#28).
            let error = result
                .as_ref()
                .err()
                .unwrap_or_else(|| panic!("{name}: is-compat is refused"));
            assert_eq!(
                (error.status, error.scope),
                (500, crate::execution::FailureScope::Credential),
                "{name}"
            );
            assert!(String::from_utf8_lossy(&error.body).contains("is-compat"), "{name}");
            assert!(captured.is_empty(), "{name}: nothing is sent");
            continue;
        }
        if native_case(case, &mock.url) {
            assert_native_contract(name, case, &result, &items, &captured);
            continue;
        }
        if translated_case(case, &mock.url) {
            assert_translated_contract(name, case, &result, &captured);
            continue;
        }
        // Every other recorded request has no Codex route (#28): no legacy rewrite stands
        // in, and nothing reaches the provider.
        let error = result
            .as_ref()
            .err()
            .unwrap_or_else(|| panic!("{name}: a request without a Codex route is refused"));
        assert_eq!(error.scope, crate::execution::FailureScope::Request, "{name}");
        assert!(captured.is_empty(), "{name}: nothing is sent");
    }
}

/// Go refreshes through `NewCodexAuthWithProxyURL(cfg, auth.ProxyURL)`: the credential's
/// proxy wins over the global one. A loopback HTTP proxy answers the token request, so
/// the unresolvable token host proves the request went through it.
#[tokio::test]
async fn refresh_uses_the_credential_proxy_before_the_global_one() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    async fn proxy_once(listener: tokio::net::TcpListener) -> String {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = vec![0; 8192];
        let n = socket.read(&mut buf).await.unwrap();
        let body = r#"{"access_token":"at-proxied","refresh_token":"rt-proxied","id_token":"","expires_in":3600}"#;
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(reply.as_bytes()).await.unwrap();
        String::from_utf8_lossy(&buf[..n]).lines().next().unwrap().to_owned()
    }
    let own = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let own_addr = own.local_addr().unwrap();
    let oauth = CodexOAuth::with_endpoints(wreq::Client::new(), "http://token.invalid/oauth/token", "", "");
    let executor =
        CodexExecutor::with_transport(crate::providers::codex_tls::Transport::new(Default::default()), oauth);
    // The global proxy is a closed port: using it would fail the refresh.
    let cfg = Config::parse("proxy-url: http://127.0.0.1:9\n").unwrap();
    let gone = (Utc::now() - chrono::TimeDelta::hours(2)).to_rfc3339();
    let credential = Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/proxied.json"),
        serde_json::json!({
            "type": "codex",
            "refresh_token": "rt-proxy-test",
            "access_token": "opaque",
            "expired": gone,
            "proxy_url": format!("http://{own_addr}"),
        })
        .as_object()
        .unwrap()
        .clone(),
    )
    .unwrap();
    let seen = tokio::spawn(proxy_once(own));
    let patch = executor.prepare(&credential, &cfg).await.unwrap();
    assert_eq!(patch.set["access_token"], "at-proxied");
    assert_eq!(seen.await.unwrap(), "POST http://token.invalid/oauth/token HTTP/1.1");
}

#[tokio::test]
async fn prepare_refreshes_when_due_and_keeps_a_valid_token_on_failure() {
    use crate::providers::codex_testkit::json;
    let mock = Mock::start().await;
    let oauth = CodexOAuth::with_endpoints(wreq::Client::new(), &format!("{}/oauth/token", mock.url), "", "")
        .with_retry_delay(std::time::Duration::from_millis(1));
    let executor = CodexExecutor::with_client(wreq::Client::new(), oauth);
    let cfg = Config::default();
    let soon = (Utc::now() + chrono::TimeDelta::hours(2)).to_rfc3339();
    let gone = (Utc::now() - chrono::TimeDelta::hours(2)).to_rfc3339();
    let cred = |refresh: &str, expired: &str| {
        Credential::from_file(
            Path::new("/fake"),
            Path::new("/fake/c.json"),
            serde_json::json!({"type":"codex","refresh_token":refresh,"access_token":"opaque","expired":expired})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap()
    };
    let due = cred("rt-1", &soon);
    assert!(executor.needs_prepare(&due, &cfg));
    mock.script("/oauth/token", vec![json(500, "{}"); 3]);
    let patch = executor.prepare(&due, &cfg).await.unwrap();
    assert!(patch.set.is_empty(), "valid token stays in use when refresh fails");
    assert_eq!(mock.take().len(), 3, "Go retries three times");

    let expired = cred("rt-2", &gone);
    mock.script("/oauth/token", vec![json(400, r#"{"error":"invalid_grant"}"#)]);
    let error = executor.prepare(&expired, &cfg).await.unwrap_err();
    assert_eq!((error.status, error.scope), (401, FailureScope::Credential));
    // #92 (first-party codex-rs): a refusal for good is not retried.
    assert_eq!(mock.take().len(), 1, "a refused refresh token is sent once");

    mock.script(
        "/oauth/token",
        vec![json(
            200,
            r#"{"access_token":"at-new","refresh_token":"rt-new","id_token":"","expires_in":3600}"#,
        )],
    );
    let patch = executor.prepare(&cred("rt-3", &gone), &cfg).await.unwrap();
    assert_eq!(patch.set["access_token"], "at-new");
    assert_eq!(patch.set["refresh_token"], "rt-new");
    assert!(!executor.needs_prepare(
        &cred("rt-4", &(Utc::now() + chrono::TimeDelta::days(5)).to_rfc3339()),
        &cfg
    ));
}

#[test]
fn alpha_search_edits_only_cache_controls_and_an_existing_model() {
    let input=br#"{ "model":"client", "prompt_cache_key":"key", "prompt_cache_retention":"24h", "unknown":{"keep":true},"n":9007199254740993 }"#;
    let sanitized = sanitize_alpha_search(input);
    let parsed: Value = serde_json::from_slice(&sanitized).unwrap();
    assert_eq!(
        parsed,
        serde_json::json!({"model":"client","unknown":{"keep":true},"n":9007199254740993_u64})
    );
    let rewritten = rewrite_alpha_search_model(sanitized, "routed");
    let parsed: Value = serde_json::from_slice(&rewritten).unwrap();
    assert_eq!(
        parsed,
        serde_json::json!({"model":"routed","unknown":{"keep":true},"n":9007199254740993_u64})
    );
    for raw in [b"not json".as_slice(), b"[]", b"null", b"{ \"unknown\": 42 }"] {
        assert_eq!(sanitize_alpha_search(raw), raw);
        assert_eq!(
            rewrite_alpha_search_model(raw.to_vec(), "routed"),
            raw,
            "missing model is not inserted"
        );
    }
}

/// Quota normalization is a local observation policy, not an undocumented claim
/// about the provider. Inputs are hand-authored rate-limit and error envelopes.
#[test]
fn quota_events_normalize_known_fields_and_signal_snapshots_are_bounded() {
    let payload = serde_json::json!({"type":"codex.rate_limits",
        "rate_limits":{"allowed":false,"limit_reached":true,"primary":{"used_percent":42,"window_minutes":300,"reset_after_seconds":30}},
        "additional_rate_limits":[{"limit_name":"tool runs","rate_limit":{"allowed":true}}],
        "code_review_rate_limits":{"allowed":true},
        "credits":{"has_credits":true,"unlimited":false,"balance":"12.5"},
        "plan_type":" pro ","metered_limit_name":"primary"});
    let normalized = |payload: &str| -> std::collections::BTreeMap<String, String> {
        crate::providers::codex_quota::event_headers(payload)
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v))
            .collect()
    };
    let expected = std::collections::BTreeMap::from(
        [
            ("x-codex-allowed", "false"),
            ("x-codex-limit-reached", "true"),
            ("x-codex-primary-used-percent", "42"),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-primary-reset-after-seconds", "30"),
            ("x-codex-additional-tool-runs-allowed", "true"),
            ("x-codex-additional-tool-runs-limit-name", "tool runs"),
            ("x-codex-code-review-allowed", "true"),
            ("x-codex-credits-has-credits", "true"),
            ("x-codex-credits-unlimited", "false"),
            ("x-codex-credits-balance", "12.5"),
            ("x-codex-plan-type", "pro"),
            ("x-codex-active-limit", "primary"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned())),
    );
    assert_eq!(normalized(&payload.to_string()), expected);
    assert_eq!(
        normalized(
            r#"{"type":"error","headers":{"retry-after":30,"X-Codex-Primary-Used-Percent":42,"unrelated":"drop","x-codex-plan-type":[],"x-codex-unknown":"drop"}}"#
        ),
        std::collections::BTreeMap::from([
            ("retry-after".into(), "30".into()),
            ("x-codex-primary-used-percent".into(), "42".into())
        ])
    );
    for payload in [
        "invalid",
        r#"{"type":"response.completed"}"#,
        r#"{"type":"codex.rate_limits","rate_limits":{"primary":{"used_percent":101,"window_minutes":300,"reset_after_seconds":30}}}"#,
    ] {
        assert!(normalized(payload).is_empty(), "{payload}");
    }
    let mut headers = HeaderMap::new();
    for (k, v) in [
        ("retry-after", "30"),
        ("x-codex-primary-used-percent", "42"),
        ("x-codex-plan-type", "pro"),
        ("x-ratelimit-limit-requests", "100"),
        ("x-codex-additional-foo-allowed", "true"),
        ("x-codex-unknown", "dropped"),
        ("anthropic-ratelimit-unified-status", "allowed"),
    ] {
        headers.insert(k, v.parse().unwrap());
    }
    headers.insert("x-codex-credits-balance", "9".repeat(600).parse().unwrap());
    headers.append("x-codex-secondary-used-percent", "1".parse().unwrap());
    headers.append("x-codex-secondary-used-percent", "2".parse().unwrap());
    let expected = std::collections::BTreeMap::from(
        [
            ("Retry-After", "30"),
            ("X-Codex-Primary-Used-Percent", "42"),
            ("X-Codex-Plan-Type", "pro"),
            ("X-Ratelimit-Limit-Requests", "100"),
            ("X-Codex-Additional-Foo-Allowed", "true"),
            ("X-Codex-Secondary-Used-Percent", "2"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned())),
    );
    assert_eq!(crate::providers::codex_quota::collect_signals(&headers), expected);
    for n in 0..100 {
        headers.insert(
            http::HeaderName::from_bytes(format!("x-ratelimit-test-{n}").as_bytes()).unwrap(),
            "1".parse().unwrap(),
        );
    }
    let signals = crate::providers::codex_quota::collect_signals(&headers);
    assert_eq!(signals.len(), 64, "configured signal resource cap");
    assert_eq!(signals["Retry-After"], "30", "priority signals survive the cap");
}

fn oauth_credential(base_url: &str) -> Credential {
    let mut c = Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/codex-o.json"),
        serde_json::json!({"type":"codex","access_token":"at-FAKE","account_id":"acct"})
            .as_object()
            .unwrap()
            .clone(),
    )
    .unwrap();
    c.attributes.insert("base_url".into(), base_url.into());
    c
}

fn plain_request(stream: bool, alt: Option<&str>) -> ExecRequest {
    let body = Bytes::from_static(br#"{"model":"gpt-5.4","input":[]}"#);
    ExecRequest {
        operation: Operation::Generate,
        source_format: Format::Codex,
        response_format: Format::Codex,
        requested_model: "gpt-5.4".into(),
        model: "gpt-5.4".into(),
        original_body: body.clone(),
        body,
        stream,
        alt: alt.map(str::to_owned),
        session: None,
        headers: HeaderMap::new(),
        execution_session: None,
        derived_session: None,
        resolved_model: None,
        usage: Default::default(),
        request_path: String::new(),
        caller: Caller {
            principal: String::new(),
            source: "authorization",
            provider: String::new(),
        },
    }
}

#[tokio::test]
async fn compact_failures_preserve_availability_and_classify_request_auth_quota_and_transport() {
    // Go: compact 404/405/501 and request faults stop the request without cooling;
    // other failures fail over without cooling; auth and quota keep the normal policy.
    use crate::providers::codex_testkit::json;
    let mock = Mock::start().await;
    let executor = executor();
    let cfg = Config::default();
    for (status, body, scope) in [
        (404, "not here", FailureScope::Request),
        (405, "", FailureScope::Request),
        (501, "", FailureScope::Request),
        (500, "boom", FailureScope::Transport),
        (503, "busy", FailureScope::Transport),
        (401, "", FailureScope::Credential),
        (
            429,
            r#"{"error":{"type":"usage_limit_reached"}}"#,
            FailureScope::Credential,
        ),
    ] {
        mock.script("/responses/compact", vec![json(status, body)]);
        let error = executor
            .execute(
                &oauth_credential(&mock.url),
                plain_request(false, Some("responses/compact")),
                &cfg,
            )
            .await
            .err()
            .expect("compact error");
        assert_eq!((error.status, error.scope), (status, scope), "compact {status}");
    }
    // The same 404 on the ordinary Responses path is a model-support failure.
    mock.script("/responses", vec![json(404, "not here")]);
    let error = executor
        .execute(&oauth_credential(&mock.url), plain_request(false, None), &cfg)
        .await
        .err()
        .expect("responses error");
    assert_eq!(error.scope, FailureScope::Model);
}

#[tokio::test]
async fn truncated_error_body_is_a_transport_failure_not_an_auth_failure() {
    use axum::response::IntoResponse;
    let app = axum::Router::new().fallback(|| async {
        let broken = futures_util::stream::iter([
            Ok::<_, std::io::Error>(Bytes::from_static(b"{\"error\":")),
            Err(std::io::Error::other("mock reset mid-body")),
        ]);
        (
            axum::http::StatusCode::UNAUTHORIZED,
            [("content-type", "application/json")],
            axum::body::Body::from_stream(broken),
        )
            .into_response()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    let error = executor()
        .execute(&oauth_credential(&url), plain_request(true, None), &Config::default())
        .await
        .err()
        .expect("transport error");
    // Never an auth failure. The mock's reset usually lands before the 401 head leaves,
    // so the request was sent and got no answer: an uncertain, request-scoped failure
    // that never fails over (N1 #111). A head that did arrive would leave a transport
    // failure of the error body instead.
    assert!(
        matches!(error.scope, FailureScope::Transport | FailureScope::Request),
        "{error}"
    );
    assert_ne!(error.status, 401);
}

#[tokio::test]
async fn alpha_search_reads_at_most_32_mib_whatever_the_status() {
    use crate::providers::codex_testkit::Reply;
    let mock = Mock::start().await;
    let big = "x".repeat(ALPHA_SEARCH_MAX_RESPONSE + 4096);
    let error_body = "e".repeat(100 * 1024);
    mock.script(
        "/alpha/search",
        vec![
            Reply {
                status: 200,
                headers: vec![("content-type".into(), "application/json".into())],
                body: big,
            },
            Reply {
                status: 500,
                headers: vec![("content-type".into(), "application/json".into())],
                body: error_body.clone(),
            },
        ],
    );
    let executor = executor().with_alpha_base_url(&mock.url);
    let cred = oauth_credential(&mock.url);
    for (status, expected) in [(200, ALPHA_SEARCH_MAX_RESPONSE), (500, error_body.len())] {
        let response = executor
            .alpha_search(
                &cred,
                b"{}",
                &HeaderMap::new(),
                "",
                &Config::default(),
                &Default::default(),
            )
            .await
            .unwrap();
        assert_eq!(response.status, status);
        let ResponseBody::Buffered(body) = response.body else {
            unreachable!()
        };
        assert_eq!(body.len(), expected, "status {status}");
    }
}

/// Records what the executor reports to the attempt's usage record.
#[derive(Default)]
struct Recorded(std::sync::Mutex<Vec<(&'static str, Format, String)>>);

impl crate::execution::UsageObserver for Recorded {
    fn response_body(&self, format: Format, body: &[u8]) {
        self.0
            .lock()
            .unwrap()
            .push(("body", format, String::from_utf8_lossy(body).into_owned()));
    }
    fn response_line(&self, format: Format, line: &[u8]) {
        self.0
            .lock()
            .unwrap()
            .push(("line", format, String::from_utf8_lossy(line).into_owned()));
    }
    fn request(&self, format: Format, payload: &[u8]) {
        self.0
            .lock()
            .unwrap()
            .push(("request", format, String::from_utf8_lossy(payload).into_owned()));
    }
}

/// Usage records see the upstream side, as Go's reporter does: the finalized request
/// (`SetTranslatedReasoningEffort` on the upstream body, c7b4d573) and every
/// upstream event in Codex format, or the compaction body in OpenAI Responses format,
/// whatever the client speaks. Expected requests come from the mock wire capture;
/// response bodies and lines come from the synthetic upstream stimulus.
#[tokio::test]
async fn usage_records_see_upstream_payloads() {
    let executor = executor();
    for name in [
        "apikey_claude_not_compat",
        "oauth_nonstream_no_instructions_free_plan",
        "oauth_compact",
    ] {
        let case = INPUTS["executor"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .unwrap();
        let mock = Mock::start().await;
        let compact = case["alt"] == "responses/compact";
        mock.script(
            if compact { "/responses/compact" } else { "/responses" },
            vec![reply(case)],
        );
        let credential = credential(case, &mock.url);
        let cfg = Config::parse(case["config"].as_str().filter(|s| !s.trim().is_empty()).unwrap_or("{}")).unwrap();
        let recorded = Arc::new(Recorded::default());
        let mut req = request(case);
        req.usage = crate::execution::UsageSink::new(recorded.clone());
        let response = match executor.execute(&credential, req, &cfg).await {
            Ok(response) => response,
            // A conversion the translated route refuses sends and records nothing (#27).
            Err(error) if translated_case(case, &mock.url) => {
                assert_eq!(error.status, 400, "{name}");
                assert!(
                    mock.take().is_empty() && recorded.0.lock().unwrap().is_empty(),
                    "{name}"
                );
                continue;
            }
            Err(error) => panic!("{name}: {error:?}"),
        };
        match response.body {
            ResponseBody::Stream(mut stream) => while stream.next().await.is_some() {},
            ResponseBody::Buffered(_) => {}
        }
        let reports = recorded.0.lock().unwrap().clone();
        let upstream_format = if compact { Format::OpenAIResponse } else { Format::Codex };
        // A native case records the body it sent: the client's, with only the permitted
        // edits (`historical_client_inputs_obey_native_translation_and_route_refusal_contracts` checks those bytes).
        let sent = String::from_utf8(mock.take().pop().expect("an upstream request").body.to_vec()).unwrap();
        assert_eq!(
            reports[0],
            ("request", upstream_format, sent),
            "{name}: translated request"
        );
        let upstream = case["upstream_body"].as_str().unwrap();
        let expected: Vec<(&str, Format, String)> = if compact {
            vec![("body", Format::OpenAIResponse, upstream.to_owned())]
        } else {
            upstream
                .lines()
                .filter_map(|l| l.strip_prefix("data:"))
                .map(|d| ("line", Format::Codex, d.trim().to_owned()))
                .collect()
        };
        assert_eq!(reports[1..], expected[..], "{name}: upstream payloads");
    }
}

/// Go `Test*RecordsFinalizedPayloadOverrideReasoningEffort` (c7b4d573): a payload
/// override of `reasoning.effort` reaches both the upstream body and the usage record,
/// on the stream, non-stream and compact paths. The record reads the finalized body, the
/// bytes sent upstream, so an input ID that sanitation prefixes is recorded prefixed.
#[tokio::test]
async fn usage_records_the_payload_override_reasoning_effort() {
    let executor = executor();
    for (name, protocol) in [
        ("apikey_claude_not_compat", "codex"),
        ("oauth_nonstream_no_instructions_free_plan", "codex"),
        ("oauth_compact", "openai-response"),
    ] {
        let case = INPUTS["executor"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .unwrap();
        let mock = Mock::start().await;
        let compact = case["alt"] == "responses/compact";
        mock.script(
            if compact { "/responses/compact" } else { "/responses" },
            vec![reply(case)],
        );
        let credential = credential(case, &mock.url);
        let cfg = Config::parse(&format!(
            "payload:\n  override:\n    - models:\n        - name: gpt-5.4\n          protocol: {protocol}\n      params:\n        reasoning.effort: high\n"
        ))
        .unwrap();
        let recorded = Arc::new(Recorded::default());
        let mut req = request(case);
        let body = String::from_utf8(req.body.to_vec()).unwrap().replace(
            r#"{"type":"message","role":"user""#,
            r#"{"type":"message","id":"abc","role":"user""#,
        );
        req.body = Bytes::from(body);
        req.usage = crate::execution::UsageSink::new(recorded.clone());
        let (native, translated) = (native_case(case, &mock.url), translated_case(case, &mock.url));
        let response = match executor.execute(&credential, req, &cfg).await {
            Ok(response) => response,
            // The translated Claude case carries a thinking block with reasoning text and
            // no Codex signature, which the translated route refuses before the send (#27).
            Err(error) if translated => {
                assert_eq!(error.status, 400, "{name}");
                assert!(mock.take().is_empty(), "{name}: nothing is sent");
                continue;
            }
            Err(error) => panic!("{name}: {error:?}"),
        };
        match response.body {
            ResponseBody::Stream(mut stream) => while stream.next().await.is_some() {},
            ResponseBody::Buffered(_) => {}
        }
        let sent = String::from_utf8(mock.take().pop().unwrap().body.to_vec()).unwrap();
        // Payload rules never reach a native (#23) or translated (#27) body; other paths
        // keep Go's override. The translated Claude case keeps the client's own thinking
        // budget (2048 tokens, `medium`).
        let effort = match (native, translated) {
            (true, _) => "",
            (_, true) => "medium",
            _ => "high",
        };
        assert_eq!(
            crate::protocol::rawjson::get(&sent, "reasoning.effort").str(),
            effort,
            "{name}: upstream"
        );
        let reports = recorded.0.lock().unwrap().clone();
        assert_eq!(reports[0].0, "request", "{name}");
        assert_eq!(reports[0].2, sent, "{name}: the usage record reads the sent body");
    }
}

/// Go `TestCodexImageResolvedModelOnWire` ("custom upstream", "routing and thinking
/// suffix" and the two "final payload" rows, 51bde0ff and 868e137) through the executor:
/// a configured, prefixed upstream name reaches the direct Images endpoint as resolved,
/// for a JSON generation and a multipart edit, and the payload rules run once, last.
#[tokio::test]
async fn direct_images_send_the_configured_upstream_model() {
    let executor = executor();
    let form = "--XB\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nREQ\r\n--XB\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\ndiagnostic\r\n--XB--\r\n";
    let models = "[{name: openai/gpt-image-2-cpr, protocol: openai, match: [{model: openai/gpt-image-2-cpr}]}]";
    let override_rules = format!(
        "payload:\n  override:\n    - models: {models}\n      params: {{model: policy/image-model}}\n    - models: [{{name: \"*\"}}]\n      params: {{\"audit.-1\": once}}\n"
    );
    let filter_rules = format!("payload:\n  filter:\n    - models: {models}\n      params: [model]\n");
    for (requested, resolved, configured, want, rules) in [
        (
            "gpt-image-2",
            "openai/gpt-image-2-cpr",
            "openai/gpt-image-2-cpr",
            "openai/gpt-image-2-cpr",
            "",
        ),
        (
            "tenant/gpt-image-2(high)",
            "OpenAI/GPT-Image-2(high)",
            "OpenAI/GPT-Image-2",
            "OpenAI/GPT-Image-2",
            "",
        ),
        (
            "gpt-image-2",
            "openai/gpt-image-2-cpr",
            "openai/gpt-image-2-cpr",
            // Payload rules no longer reach the Images adapter (#27).
            "openai/gpt-image-2-cpr",
            override_rules.as_str(),
        ),
        (
            "gpt-image-2",
            "openai/gpt-image-2-cpr",
            "openai/gpt-image-2-cpr",
            "openai/gpt-image-2-cpr",
            filter_rules.as_str(),
        ),
    ] {
        for (request_path, upstream_path, content_type, body) in [
            (
                "/v1/images/generations",
                "/images/generations",
                "application/json",
                format!(r#"{{"model":"{requested}","prompt":"diagnostic"}}"#),
            ),
            (
                "/v1/images/edits",
                "/images/edits",
                "multipart/form-data; boundary=XB",
                form.replace("REQ", requested),
            ),
        ] {
            let mock = Mock::start().await;
            mock.script(
                upstream_path,
                vec![crate::providers::codex_testkit::json(
                    200,
                    r#"{"created":1713833628,"data":[{"b64_json":"AA=="}]}"#,
                )],
            );
            let cfg = Config::parse(&format!(
                "codex-api-key:\n  - api-key: codex-token\n    base-url: {}\n    prefix: tenant\n    models:\n      - name: {configured}\n        alias: gpt-image-2\n{rules}",
                mock.url
            ))
            .unwrap();
            let credential = crate::config::credentials::from_config(&cfg)
                .into_iter()
                .find(|c| c.provider == "codex")
                .unwrap();
            let mut headers = HeaderMap::new();
            headers.insert(http::header::CONTENT_TYPE, content_type.parse().unwrap());
            let body = Bytes::from(body);
            let req = ExecRequest {
                operation: Operation::Generate,
                source_format: Format::OpenAI,
                response_format: Format::OpenAI,
                requested_model: requested.into(),
                model: resolved.into(),
                original_body: body.clone(),
                body,
                stream: false,
                alt: None,
                session: None,
                headers,
                execution_session: None,
                derived_session: None,
                resolved_model: None,
                usage: Default::default(),
                request_path: request_path.into(),
                caller: Caller {
                    principal: "client-key-FAKE".into(),
                    source: "authorization",
                    provider: String::new(),
                },
            };
            executor.images(&credential, req, request_path, &cfg).await.unwrap();
            let captured = mock.take();
            assert_eq!(captured.len(), 1, "{resolved} {request_path}");
            assert_eq!(captured[0].path, upstream_path);
            let sent: Value = serde_json::from_slice(&captured[0].body).unwrap();
            assert_eq!(
                sent["model"].as_str().unwrap_or_default(),
                want,
                "{resolved} {request_path}"
            );
            assert_eq!(sent["prompt"], "diagnostic", "{request_path}");
            assert!(sent.get("audit").is_none(), "{request_path}: no payload rule ran");
        }
    }
}

/// Build an Images call with synthetic input and no client or saved credentials.
fn image_request(body: Vec<u8>, model: &str, stream: bool, path: &str) -> ExecRequest {
    let mut req = plain_request(stream, None);
    req.source_format = Format::OpenAI;
    req.response_format = Format::OpenAI;
    req.model = model.into();
    req.requested_model = model.into();
    req.body = Bytes::from(body);
    req.original_body = req.body.clone();
    req.request_path = path.into();
    req.headers.insert("user-agent", "caller-identity".parse().unwrap());
    req
}

fn image_form(partial: &str) -> (Vec<u8>, String) {
    let mut form = crate::providers::multipart::Writer::with_boundary("image-contract".into());
    for (key, value) in [
        ("prompt", "paint a contract"),
        ("partial_images", partial),
        ("size", "1024x1024"),
        ("quality", "high"),
    ] {
        form.field(key, value.as_bytes());
    }
    form.part(
        &[
            (
                "Content-Disposition".into(),
                b"form-data; name=\"image\"; filename=\"input.png\"".to_vec(),
            ),
            ("Content-Type".into(), b"image/png".to_vec()),
        ],
        b"IMG",
    );
    form.part(
        &[
            (
                "Content-Disposition".into(),
                b"form-data; name=\"mask\"; filename=\"mask.png\"".to_vec(),
            ),
            ("Content-Type".into(), b"image/png".to_vec()),
        ],
        b"MASK",
    );
    form.finish()
}

/// Direct image endpoints preserve JSON semantics (including unknown values),
/// multipart file bytes, routed identity and upstream response bytes in both modes.
#[tokio::test]
async fn direct_images_preserve_json_form_files_and_buffered_or_streamed_upstream_bytes() {
    use crate::providers::codex_testkit::json;
    for edit in [false, true] {
        for multipart in [false, true] {
            if multipart && !edit {
                continue;
            }
            for stream in [false, true] {
                let mock = Mock::start().await;
                let endpoint = if edit { "/images/edits" } else { "/images/generations" };
                let path = if edit {
                    "/v1/images/edits"
                } else {
                    "/v1/images/generations"
                };
                let upstream = if stream {
                    "event: image_generation.completed\ndata: {\"type\":\"image_generation.completed\",\"b64_json\":\"SU1H\"}\n\n"
                } else {
                    r#"{ "created":17,"data":[{"b64_json":"SU1H"}],"unknown":9007199254740993 }"#
                };
                let mut reply = json(200, upstream);
                if stream {
                    reply.headers = vec![("content-type".into(), "text/event-stream".into())];
                }
                mock.script(endpoint, vec![reply]);
                let (body, ct) = if multipart {
                    image_form("2")
                } else {
                    (
                        br#"{ "model":"client-image", "prompt":"paint a contract", "unknown":9007199254740993 }"#
                            .to_vec(),
                        "application/json".into(),
                    )
                };
                let mut req = image_request(body, "gpt-image-2", stream, path);
                req.headers.insert("content-type", ct.parse().unwrap());
                let response = executor()
                    .images(&oauth_credential(&mock.url), req, path, &Config::default())
                    .await
                    .unwrap();
                let sent = mock.take();
                assert_eq!(sent.len(), 1);
                assert_eq!(sent[0].path, endpoint);
                assert_eq!(sent[0].headers["user-agent"], PROXY_UA);
                let body: Value = serde_json::from_slice(&sent[0].body).unwrap();
                assert_eq!(body["model"], "gpt-image-2");
                assert_eq!(body["stream"].as_bool().unwrap_or(false), stream);
                assert_eq!(body["prompt"], "paint a contract");
                if multipart {
                    assert_eq!(body["images"][0]["image_url"], "data:image/png;base64,SU1H");
                    assert_eq!(body["mask"]["image_url"], "data:image/png;base64,TUFTSw==");
                    assert_eq!(body["partial_images"], 2);
                    assert_eq!(body["size"], "1024x1024");
                } else {
                    assert_eq!(body["unknown"], 9007199254740993_u64);
                }
                let bytes = match response.body {
                    ResponseBody::Buffered(body) => body.to_vec(),
                    ResponseBody::Stream(mut events) => {
                        let mut bytes = Vec::new();
                        while let Some(event) = events.next().await {
                            bytes.extend(event.unwrap());
                        }
                        bytes
                    }
                };
                assert_eq!(bytes, upstream.as_bytes());
            }
        }
    }
    let mock = Mock::start().await;
    let upstream = r#"{"error":{"message":"bad size","type":"invalid_request_error"}}"#;
    mock.script("/images/generations", vec![json(400, upstream)]);
    let error = executor()
        .images(
            &oauth_credential(&mock.url),
            image_request(
                br#"{"prompt":"paint"}"#.to_vec(),
                "gpt-image-2",
                false,
                "/v1/images/generations",
            ),
            "/v1/images/generations",
            &Config::default(),
        )
        .await
        .err()
        .unwrap();
    assert_eq!((error.status, error.scope), (400, FailureScope::Request));
    assert_eq!(error.body.as_ref(), upstream.as_bytes());
}

/// The image tool is an explicit conversion with a chosen upstream model and
/// image-generation action. Results and partial frames read synthetic provider events.
#[tokio::test]
async fn image_tool_preserves_prompt_settings_files_and_image_results_in_both_response_formats() {
    let item = serde_json::json!({"type":"image_generation_call","id":"image-contract","status":"completed","result":"SU1H","revised_prompt":"revised contract","output_format":"webp","size":"1024x1024","quality":"high","background":"transparent"});
    let usage = serde_json::json!({"input_tokens":3,"output_tokens":5,"total_tokens":8});
    let completed = serde_json::json!({"type":"response.completed","response":{"id":"response-contract","created_at":17,"status":"completed","output":[item],"tool_usage":{"image_gen":usage}}});
    let partial = serde_json::json!({"type":"response.image_generation_call.partial_image","partial_image_index":2,"partial_image_b64":"UEFSVA==","output_format":"webp"});
    let upstream = format!("data: {partial}\n\ndata: {completed}\n\n");
    for edit in [false, true] {
        for multipart in [false, true] {
            if multipart && !edit {
                continue;
            }
            for stream in [false, true] {
                for url in [false, true] {
                    let mock = Mock::start().await;
                    mock.script(
                        "/responses",
                        vec![Reply {
                            status: 200,
                            headers: vec![("content-type".into(), "text/event-stream".into())],
                            body: upstream.clone(),
                        }],
                    );
                    let path = if edit {
                        "/v1/images/edits"
                    } else {
                        "/v1/images/generations"
                    };
                    let (body, ct) = if multipart {
                        let (body, ct) = image_form("2");
                        // Add response_format before the closing multipart delimiter.
                        let tail = b"--image-contract--\r\n";
                        let mut raw = body[..body.len() - tail.len()].to_vec();
                        raw.extend(format!("--image-contract\r\nContent-Disposition: form-data; name=\"response_format\"\r\n\r\n{}\r\n",if url {"url"} else {"b64_json"}).as_bytes());
                        raw.extend(tail);
                        (raw, ct)
                    } else {
                        let mut value = serde_json::json!({"prompt":"paint a contract","size":"1024x1024","quality":"high","partial_images":2,"response_format":if url {"url"} else {"b64_json"}});
                        if edit {
                            value["images"] = serde_json::json!([{"image_url":"data:image/png;base64,SU1H"}]);
                            value["mask"] = serde_json::json!({"image_url":"data:image/png;base64,TUFTSw=="});
                        }
                        (value.to_string().into_bytes(), "application/json".into())
                    };
                    let mut req = image_request(body, "gpt-5.4", stream, path);
                    req.headers.insert("content-type", ct.parse().unwrap());
                    let response = executor()
                        .images(&oauth_credential(&mock.url), req, path, &Config::default())
                        .await
                        .unwrap();
                    let sent = mock.take();
                    assert_eq!(sent.len(), 1);
                    assert_eq!(sent[0].path, "/responses");
                    assert_eq!(sent[0].headers["user-agent"], PROXY_UA);
                    let body: Value = serde_json::from_slice(&sent[0].body).unwrap();
                    assert_eq!(body["model"], "gpt-5.4-mini");
                    assert_eq!(body["stream"], true);
                    assert_eq!(body["store"], false);
                    assert_eq!(body["tool_choice"], serde_json::json!({"type":"image_generation"}));
                    let tool = &body["tools"][0];
                    assert_eq!(tool["action"], if edit { "edit" } else { "generate" });
                    assert_eq!(tool["partial_images"], 2);
                    assert_eq!(tool["size"], "1024x1024");
                    assert_eq!(tool["quality"], "high");
                    assert_eq!(body["input"][0]["content"][0]["text"], "paint a contract");
                    if edit {
                        assert_eq!(
                            body["input"][0]["content"][1]["image_url"],
                            "data:image/png;base64,SU1H"
                        );
                        assert_eq!(tool["input_image_mask"]["image_url"], "data:image/png;base64,TUFTSw==");
                    }
                    for key in [
                        "instructions",
                        "reasoning",
                        "parallel_tool_calls",
                        "include",
                        "prompt_cache_key",
                    ] {
                        assert!(body.get(key).is_none(), "no invented {key}");
                    }
                    let (field, result, partial_result) = if url {
                        ("url", "data:image/webp;base64,SU1H", "data:image/webp;base64,UEFSVA==")
                    } else {
                        ("b64_json", "SU1H", "UEFSVA==")
                    };
                    match response.body {
                        ResponseBody::Buffered(body) => {
                            assert!(!stream);
                            let body: Value = serde_json::from_slice(&body).unwrap();
                            assert_eq!(body["created"], 17);
                            assert_eq!(body["data"][0][field], result);
                            assert_eq!(body["data"][0]["revised_prompt"], "revised contract");
                            assert_eq!(body["output_format"], "webp");
                            assert_eq!(body["usage"], usage);
                        }
                        ResponseBody::Stream(mut frames) => {
                            assert!(stream);
                            let mut events = String::new();
                            while let Some(frame) = frames.next().await {
                                events.push_str(std::str::from_utf8(&frame.unwrap()).unwrap());
                            }
                            let data: Vec<Value> = events
                                .lines()
                                .filter_map(|line| line.strip_prefix("data: "))
                                .map(|line| serde_json::from_str(line).unwrap())
                                .collect();
                            assert_eq!(data.len(), 2, "{events}");
                            let prefix = if edit { "image_edit" } else { "image_generation" };
                            assert_eq!(data[0]["type"], format!("{prefix}.partial_image"));
                            assert_eq!(data[0]["partial_image_index"], 2);
                            assert_eq!(data[0][field], partial_result);
                            assert_eq!(data[1]["type"], format!("{prefix}.completed"));
                            assert_eq!(data[1][field], result);
                            assert_eq!(data[1]["usage"], usage);
                        }
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn image_tool_refuses_unrepresentable_inputs_and_reports_upstream_or_incomplete_output() {
    use crate::providers::codex_testkit::json;
    let path = "/v1/images/generations";
    for input in [
        r#"{"prompt":"paint","n":2}"#,
        r#"{"prompt":"paint","style":"vivid"}"#,
        r#"{"prompt":"paint","images":[{"image_url":" "}]}"#,
        r#"{"prompt":"paint","partial_images":"nope"}"#,
        "{bad",
    ] {
        let mock = Mock::start().await;
        let error = executor()
            .images(
                &oauth_credential(&mock.url),
                image_request(input.as_bytes().to_vec(), "gpt-5.4", false, path),
                path,
                &Config::default(),
            )
            .await
            .err()
            .unwrap();
        assert_eq!((error.status, error.scope), (400, FailureScope::Request), "{input}");
        assert!(mock.take().is_empty());
    }
    let mock = Mock::start().await;
    let (body, ct) = image_form("nope");
    let mut req = image_request(body, "gpt-5.4", true, "/v1/images/edits");
    req.headers.insert("content-type", ct.parse().unwrap());
    let error = executor()
        .images(
            &oauth_credential(&mock.url),
            req,
            "/v1/images/edits",
            &Config::default(),
        )
        .await
        .err()
        .unwrap();
    assert_eq!((error.status, error.scope), (400, FailureScope::Request));
    assert!(mock.take().is_empty());
    for (status, upstream, expected) in [
        (
            429,
            r#"{"error":{"type":"usage_limit_reached","message":"limit","resets_in_seconds":60}}"#,
            429,
        ),
        (
            200,
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"status\":\"completed\",\"output\":[]}}\n\n",
            502,
        ),
        (
            200,
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"image_generation_call\",\"result\":\"SU1H\"}}\n\n",
            504,
        ),
    ] {
        let mock = Mock::start().await;
        let mut reply = json(status, upstream);
        reply.headers = vec![("content-type".into(), "text/event-stream".into())];
        mock.script("/responses", vec![reply]);
        let error = executor()
            .images(
                &oauth_credential(&mock.url),
                image_request(br#"{"prompt":"paint"}"#.to_vec(), "gpt-5.4", false, path),
                path,
                &Config::default(),
            )
            .await
            .err()
            .unwrap();
        assert_eq!(error.status, expected, "{upstream}");
        if status == 429 {
            assert_eq!(error.scope, FailureScope::Credential);
            assert_eq!(error.retry_after, Some(std::time::Duration::from_secs(60)));
        }
        assert_eq!(mock.take().len(), 1);
    }
}

/// Go `Auth.AccountInfo` for the fixture credentials.
fn capture_account(credential: &Credential) -> [String; 5] {
    let (kind, value) = match credential.attributes.get("api_key") {
        Some(key) => ("api_key", key.clone()),
        None => ("oauth", credential.str("email").unwrap_or_default().to_owned()),
    };
    [
        "codex".into(),
        credential.id.clone(),
        credential.label.clone(),
        kind.into(),
        value,
    ]
}

/// Upstream capture (M4-0250) at Go's Codex HTTP sites (`codex_executor_execute.go:93,264`,
/// `codex_executor_stream.go:101`). Go records the request header map and body as sent,
/// the response status, the error body or, on success, the whole body (Execute, compact)
/// or each scanned line (streams), and every stream failure it reports. So for each case
/// of Go's executor fixture: the recorded request is what the upstream received (less
/// the transport's own headers), the chunks are the upstream body, and the recorded
/// stream error is Go's.
#[tokio::test]
async fn capture_matches_mock_wire_bytes_metadata_and_delivered_failures() {
    let executor = executor();
    for case in INPUTS["executor"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let mock = Mock::start().await;
        let path = if case["alt"] == "responses/compact" {
            "/responses/compact"
        } else {
            "/responses"
        };
        if let Some(redirect) = case["redirect"].as_u64() {
            let moved = format!("/moved{path}");
            mock.script(
                path,
                vec![Reply {
                    status: redirect as u16,
                    headers: vec![("location".into(), moved.clone())],
                    body: String::new(),
                }],
            );
            mock.script(&moved, vec![reply(case)]);
        } else {
            mock.script(path, vec![reply(case)]);
        }
        if config_refusal(case).is_some() {
            continue;
        }
        let credential = credential(case, &mock.url);
        let cfg = Config::parse(case["config"].as_str().filter(|s| !s.trim().is_empty()).unwrap_or("{}")).unwrap();
        let tap = Arc::new(Wiretap::default());
        let mut req = request(case);
        req.usage =
            crate::execution::UsageSink::default().with_capture(crate::execution::CaptureSink::new(tap.clone()));
        let ws_session = case["exec_metadata"]["execution_session_id"].as_str();
        let result = match ws_session {
            Some(session) => ws_http(&executor, &credential, &cfg, req, session).await,
            None => executor.execute(&credential, req, &cfg).await,
        };
        let mut delivered_errors = Vec::new();
        match result {
            Ok(ExecResponse {
                body: ResponseBody::Stream(mut stream),
                ..
            }) => {
                while let Some(item) = stream.next().await {
                    if let Err(error) = item {
                        delivered_errors.push(error);
                    }
                }
            }
            Err(error) => delivered_errors.push(error),
            _ => {}
        }
        let taps = tap.taps();
        let all = mock.take();
        if all.is_empty() {
            // A refused request (a translated conversion, a suffix, a continuation without
            // a socket) sends nothing and records no request (#27, #28).
            assert!(result_refused(&taps), "{name}");
            continue;
        }
        let sent = &all[0];
        // The request: Go's header map is what went out, less what the transport adds.
        let Tap::Request {
            url,
            method,
            headers,
            body,
            account,
        } = &taps[0]
        else {
            panic!("{name}: first event {:?}", taps[0]);
        };
        assert_eq!(
            (url.as_str(), method.as_str()),
            (format!("{}{path}", mock.url).as_str(), "POST"),
            "{name}"
        );
        assert_eq!(body.as_bytes(), sent.body.as_ref(), "{name}: request body");
        assert_eq!(*account, capture_account(&credential), "{name}: account");
        let mut recorded: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
            .collect();
        recorded.sort();
        let mut wire: Vec<(String, String)> = sent
            .headers
            .iter()
            .filter(|(k, _)| !matches!(k.as_str(), "host" | "content-length" | "accept-encoding"))
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_owned()))
            .collect();
        wire.sort();
        assert_eq!(recorded, wire, "{name}: request headers");
        let status = case["upstream_status"].as_u64().unwrap() as u16;
        assert!(
            matches!(&taps[1], Tap::Metadata(s, _) if *s == status),
            "{name}: {:?}",
            taps[1]
        );
        let upstream = case["upstream_body"].as_str().unwrap();
        let rest = &taps[2..];
        let chunks: Vec<&str> = rest
            .iter()
            .filter_map(|t| match t {
                Tap::Chunk(c) if !c.trim().is_empty() => Some(c.as_str()),
                _ => None,
            })
            .collect();
        let errors: Vec<&str> = rest
            .iter()
            .filter_map(|t| match t {
                Tap::Error(e) => Some(e.as_str()),
                _ => None,
            })
            .collect();
        if !(200..300).contains(&status) || case["stream"] == false {
            // The error body, or Execute's and compact's whole body, recorded once.
            assert_eq!(chunks, [upstream], "{name}: whole body");
            assert!(
                errors.is_empty(),
                "{name}: Execute records no parse failure: {errors:?}"
            );
            continue;
        }
        // Streams record every scanned line up to where Go stops reading.
        let lines: Vec<&str> = upstream.split('\n').filter(|l| !l.trim().is_empty()).collect();
        assert!(!chunks.is_empty(), "{name}: stream lines");
        assert_eq!(chunks, lines[..chunks.len()], "{name}: stream lines");
        assert_eq!(
            errors.len(),
            delivered_errors.len(),
            "{name}: every delivered failure is captured"
        );
        for (captured, error) in errors.iter().zip(&delivered_errors) {
            assert!(
                captured.contains(String::from_utf8_lossy(&error.body).as_ref()),
                "{name}: capture {captured:?}, delivered {error:?}"
            );
        }
        if errors.is_empty() {
            assert_eq!(chunks.len(), lines.len(), "{name}: every successful line");
        }
    }
}

/// Go's transport failure path: the request, then `RecordAPIResponseError`.
#[tokio::test]
async fn capture_records_a_transport_failure() {
    let tap = Arc::new(Wiretap::default());
    let mut req = plain_request(true, None);
    req.usage = crate::execution::UsageSink::default().with_capture(crate::execution::CaptureSink::new(tap.clone()));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let error = executor()
        .execute(&oauth_credential(&url), req, &Config::default())
        .await
        .err()
        .expect("nothing listens");
    let taps = tap.taps();
    assert!(matches!(&taps[0], Tap::Request { .. }), "{taps:?}");
    assert_eq!(
        taps[1..],
        [Tap::Error(String::from_utf8_lossy(&error.body).into_owned())]
    );
}

/// A loopback upstream answering every request with `status`, `content_type` and these
/// body pieces, the last one an error when `reset` (the connection breaks mid-body).
async fn broken_upstream(status: u16, content_type: &'static str, pieces: Vec<&'static str>, reset: bool) -> String {
    use axum::response::IntoResponse;
    let app = axum::Router::new().fallback(move || {
        let pieces = pieces.clone();
        async move {
            let items: Vec<Result<Bytes, std::io::Error>> = pieces
                .into_iter()
                .map(|p| Ok(Bytes::from_static(p.as_bytes())))
                .collect();
            // The reset comes after the bytes were flushed, as a peer dying mid-body.
            let reset = futures_util::stream::iter(reset.then_some(())).then(|()| async {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                Err::<Bytes, _>(std::io::Error::other("mock reset mid-body"))
            });
            (
                axum::http::StatusCode::from_u16(status).unwrap(),
                [("content-type", content_type)],
                axum::body::Body::from_stream(futures_util::stream::iter(items).chain(reset)),
            )
                .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    url
}

fn tapped_request(stream: bool, alt: Option<&str>) -> (ExecRequest, Arc<Wiretap>) {
    let tap = Arc::new(Wiretap::default());
    let mut req = plain_request(stream, alt);
    req.usage = crate::execution::UsageSink::default().with_capture(crate::execution::CaptureSink::new(tap.clone()));
    (req, tap)
}

/// Go's stream loop records each line as it scans it and returns at the terminal event,
/// so a line after `response.completed` in the same SSE event is never recorded.
#[tokio::test]
async fn capture_stops_at_the_terminal_line() {
    const COMPLETED: &str = r#"data: {"type":"response.completed","response":{"id":"r","output":[]}}"#;
    let url = broken_upstream(
        200,
        "text/event-stream",
        vec![
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"output\":[]}}\nid: after-terminal\n\n",
        ],
        false,
    )
    .await;
    let (req, tap) = tapped_request(true, None);
    let response = executor()
        .execute(&oauth_credential(&url), req, &Config::default())
        .await
        .unwrap();
    let ResponseBody::Stream(mut stream) = response.body else {
        panic!("a stream");
    };
    while stream.next().await.is_some() {}
    let chunks: Vec<String> = tap
        .taps()
        .into_iter()
        .filter_map(|t| match t {
            Tap::Chunk(c) if !c.is_empty() => Some(c),
            _ => None,
        })
        .collect();
    assert_eq!(chunks.last().map(String::as_str), Some(COMPLETED), "{chunks:?}");
    assert!(!chunks.iter().any(|c| c.contains("after-terminal")), "{chunks:?}");
}

/// Body read failures follow each Go call site: Execute (`data, errRead := io.ReadAll`)
/// records every byte that arrived, the unfinished tail included, then the error; Alpha
/// Search records the partial body, then the error; the stream path's error response
/// records only the read error and returns it.
#[tokio::test]
async fn capture_keeps_each_sites_read_error_policy() {
    // Execute: the whole partial body, then the error.
    let url = broken_upstream(
        200,
        "text/event-stream",
        vec!["data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\ndata: {\"type\":\"resp"],
        true,
    )
    .await;
    let (req, tap) = tapped_request(false, None);
    let result = executor()
        .execute(&oauth_credential(&url), req, &Config::default())
        .await;
    let taps = tap.taps();
    assert!(taps.len() > 2, "{taps:?} {:?}", result.err());
    assert_eq!(
        taps[2],
        Tap::Chunk(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\ndata: {\"type\":\"resp".into()
        ),
        "{taps:?}"
    );
    assert!(matches!(&taps[3], Tap::Error(_)), "{taps:?}");
    assert_eq!(taps.len(), 4, "{taps:?}");

    // Alpha Search: the prefix, then the error.
    let url = broken_upstream(200, "application/json", vec!["{\"results\":["], true).await;
    let tap = Arc::new(Wiretap::default());
    let mut cred = oauth_credential(&url);
    cred.attributes.insert("api_key".into(), "sk-FAKE".into());
    let _ = executor()
        .alpha_search(
            &cred,
            b"{}",
            &HeaderMap::new(),
            "",
            &Config::default(),
            &crate::execution::CaptureSink::new(tap.clone()),
        )
        .await;
    let taps = tap.taps();
    assert_eq!(taps[2], Tap::Chunk("{\"results\":[".into()), "{taps:?}");
    assert!(matches!(&taps[3], Tap::Error(_)), "{taps:?}");

    // Stream: an error body that breaks is the read error, not a status error.
    let url = broken_upstream(401, "application/json", vec!["{\"error\":"], true).await;
    let (req, tap) = tapped_request(true, None);
    let error = executor()
        .execute(&oauth_credential(&url), req, &Config::default())
        .await
        .err()
        .expect("read error");
    assert_ne!(error.status, 401, "{error}");
    let taps = tap.taps();
    assert_eq!(
        taps[2..],
        [Tap::Error(String::from_utf8_lossy(&error.body).into_owned())],
        "{taps:?}"
    );
}

/// The direct Images endpoints (`executeDirectOpenAIImage` and its stream form) read a
/// non-2xx body with `io.ReadAll` and, when it breaks, record the read error and return
/// it in place of the status error.
#[tokio::test]
async fn direct_images_return_the_read_error_of_a_broken_error_body() {
    for stream in [false, true] {
        let url = broken_upstream(401, "application/json", vec!["{\"error\":"], true).await;
        let (mut req, tap) = tapped_request(stream, None);
        let body = Bytes::from_static(br#"{"model":"gpt-image-2","prompt":"a lighthouse"}"#);
        req.source_format = Format::OpenAI;
        req.response_format = Format::OpenAI;
        req.requested_model = "gpt-image-2".into();
        req.model = "gpt-image-2".into();
        req.original_body = body.clone();
        req.body = body;
        let error = executor()
            .images(
                &oauth_credential(&url),
                req,
                "/v1/images/generations",
                &Config::default(),
            )
            .await
            .err()
            .expect("read error");
        assert_ne!(error.status, 401, "stream={stream}: {error}");
        let taps = tap.taps();
        assert!(matches!(&taps[0], Tap::Request { .. }), "stream={stream}: {taps:?}");
        assert_eq!(
            taps[2..],
            [Tap::Error(String::from_utf8_lossy(&error.body).into_owned())],
            "stream={stream}: {taps:?}"
        );
    }
}

/// The translated route's event processing (`Processor`, with the bootstrap gate when
/// `buffering`) over an upstream that sends `sse`: the frames and the in-stream error, or
/// the attempt's error; buffered (`stream: false`), the terminal `response`. These Go
/// cases pin its line scanner. The native route forwards events unchanged instead
/// (`codex_native_tests.rs`).
async fn through_sse(sse: &str, stream: bool, buffering: bool) -> Result<(Vec<String>, Option<ExecError>), ExecError> {
    use crate::providers::codex_response::{Bootstrap, Processor, Step, bootstrap, processed};
    let processor = Processor::new(false, false);
    if !stream {
        let mut processor = processor;
        let completed = processor.buffered(sse.as_bytes())?.expect("a terminal event");
        return Ok((
            vec![crate::protocol::rawjson::get(&completed, "response").json().to_owned()],
            None,
        ));
    }
    let upstream = crate::providers::codex::events(ResponseBody::Stream(crate::providers::upstream::framed(
        futures_util::stream::iter([Ok::<_, ExecError>(Bytes::from(sse.to_owned()))]).boxed(),
    )));
    let mut body = if buffering {
        match bootstrap(upstream, processor, None, Instant::now()).await {
            Bootstrap::Reject(error) => return Err(error),
            Bootstrap::Stream { held, rest } => futures_util::stream::iter(held)
                .chain(futures_util::stream::iter(rest).flatten())
                .boxed(),
        }
    } else {
        processed(upstream, processor, 0)
    };
    let (mut frames, mut failure) = (Vec::new(), None);
    while let Some(step) = body.next().await {
        let out = match step {
            Step::Event(out) | Step::Terminal(out) => Some(out),
            Step::Fail { error, before, .. } => {
                failure = Some(error);
                before
            }
        };
        frames.extend(out.map(|out| String::from_utf8(out.to_vec()).unwrap()));
    }
    Ok((frames, failure))
}

const SPLIT_CREATED: &str = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_split\",\"model\":\"gpt-5.4\"}}\n\n";
/// Go's `codexOverloadEvent`: an overload that fails the attempt over during bootstrap.
const SPLIT_OVERLOAD: &str = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"service_unavailable_error\",\"code\":\"server_is_overloaded\",\"message\":\"Our servers are currently overloaded. Please try again later.\",\"param\":null},\"sequence_number\":2}\n\n";

/// Go's `codex_executor_sse_events_test.go` (7795de19) for events whose data spans several
/// `data:` lines: a split event reads as its one-line form, stream and non-stream, with
/// and without bootstrap buffering, and an invalid split stays invalid.
#[tokio::test]
async fn split_sse_events_read_as_one_event() {
    let completed = r#"{"type":"response.completed","response":{"id":"resp_split","object":"response","model":"gpt-5.4","status":"completed","output":[{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}],"usage":{"input_tokens":5,"output_tokens":1,"total_tokens":6}}}"#;
    let cut = completed.find(r#""output":"#).unwrap();
    let delta = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"delta\":\"hi\"}\n\n";
    let one_line = format!("{SPLIT_CREATED}{delta}event: response.completed\ndata: {completed}\n\n");
    let split = format!(
        "{SPLIT_CREATED}{delta}event: response.completed\ndata: {}\ndata: {}\n\n",
        &completed[..cut],
        &completed[cut..]
    );
    for buffering in [false, true] {
        // TestCodexExecuteStreamTreatsAnEventSplitAcrossDataLinesAsOneEvent
        let want = through_sse(&one_line, true, buffering).await.unwrap();
        let got = through_sse(&split, true, buffering).await.unwrap();
        assert!(want.1.is_none() && want.0.last().unwrap().contains("response.completed"));
        assert_eq!(got.0, want.0, "buffering={buffering}");
        assert!(got.1.is_none());
        // TestCodexExecuteStreamMinimalReproFromIssue6575 and
        // TestCodexExecuteStreamAssemblesSplitEventsWithAndWithoutBootstrapBuffering
        for sse in [
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r\",\"status\":\"in_progress\",\"output\":[]}}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"status\":\"completed\",\ndata: \"output\":[]}}\n\n".to_owned(),
            format!("{SPLIT_CREATED}data: {{\"type\":\"response.in_progress\",\"response\":{{\"id\":\"resp_split\",\"output\":[\ndata: {{\"x\":1}},\ndata: {{\"x\":1}}]}}}}\n\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_split\",\"status\":\"completed\",\ndata: \"output\":[]}}}}\n\n"),
        ] {
            let (frames, failure) = through_sse(&sse, true, buffering).await.unwrap();
            assert!(failure.is_none(), "{failure:?}");
            assert!(!frames.iter().any(|f| f.contains("data: {\"x\":1},")), "{frames:?}");
            let last = frames.last().unwrap().trim_end();
            assert!(last.contains("\"response.completed\""), "{last}");
            let data = last.lines().find_map(|l| l.strip_prefix("data: ")).unwrap();
            assert!(serde_json::from_str::<Value>(data).is_ok(), "{last}");
        }
    }

    // TestCodexExecuteNonStreamReadsAnEventSplitAcrossDataLines, CRLF included.
    let sse = format!(
        "event: response.completed\r\ndata: {}\r\ndata: {}\r\n\r\n",
        &completed[..cut],
        &completed[cut..]
    );
    let (frames, failure) = through_sse(&sse, false, false).await.unwrap();
    assert!(failure.is_none());
    let body: Value = serde_json::from_str(&frames[0]).unwrap();
    assert_eq!(body["id"], "resp_split", "{body}");
    assert_eq!(body["output"].as_array().unwrap().len(), 1, "{body}");
}

/// Go's bootstrap budget tests for split events (7795de19): a split handshake that passes
/// the 48-line or 1 MiB budget while it is being collected releases the stream, so the
/// overload after it arrives in-stream instead of failing over, and the lines before the
/// failure in its event are delivered.
#[tokio::test]
async fn split_sse_events_spend_the_bootstrap_budget() {
    let open = "data: {\"type\":\"response.in_progress\",\"response\":{\"id\":\"resp_split\",\"items\":[\n";
    // TestCodexExecuteStreamSplitHandshakeExceedingLineBudgetReleasesBootstrapWithoutFailover
    let over_lines = format!(
        "{SPLIT_CREATED}{open}{}data: ]}}}}\n\n{SPLIT_OVERLOAD}",
        "data: \n".repeat(50)
    );
    // TestCodexExecuteStreamEventExceedsBudgetWhileAssemblingReleasesStream: the event
    // reaches 48 lines while collected, and the overload follows without a blank line.
    let no_blank = format!(
        "{SPLIT_CREATED}{open}{}data: {{\"key_at_boundary\":123}},\ndata: {{\"key_after_boundary\":456}}\ndata: ]}}}}\n{SPLIT_OVERLOAD}",
        "data: \n".repeat(43)
    );
    // TestCodexExecuteStreamMixedFramingCommentInsideSplitEventCountsBudget
    let comments = format!(
        "data: {{\"type\":\"response.in_progress\",\"items\":[\n{}: keepalive-1\n: keepalive-2\n{SPLIT_OVERLOAD}data: ]}}}}\n\n",
        "data: \n".repeat(46)
    );
    // TestCodexExecuteStreamEventExceedsByteBudgetWhileAssemblingReleasesStream
    let large = format!("data: \"{}\",\n", "x".repeat(250 * 1024));
    let over_bytes = format!(
        "data: {{\"type\":\"response.in_progress\",\"items\":[\n{}data: \"last\"]}}\n\n{SPLIT_OVERLOAD}",
        large.repeat(5)
    );
    // An overload that precedes a long split payload in its event decides first.
    let overload_first = format!(
        "{SPLIT_CREATED}{}\n{open}{}data: ]}}}}\n\n",
        SPLIT_OVERLOAD.trim_end(),
        "data: \n".repeat(60)
    );
    let error = through_sse(&overload_first, true, true).await.expect_err("fails over");
    assert_eq!(error.status, 503, "{error:?}");
    // Control: a split handshake within the budget still lets the overload fail over.
    let within = format!(
        "{SPLIT_CREATED}{open}{}data: ]}}}}\n{SPLIT_OVERLOAD}",
        "data: \n".repeat(10)
    );
    let error = through_sse(&within, true, true).await.expect_err("fails over");
    assert_eq!(error.status, 503, "{error:?}");
    // Each delivered line spends the budget before the next is read, and a handshake may
    // fill it exactly (`BOOTSTRAP_MAX_FRAMES`): 47 comments and the overload's
    // `event:` line are 48 held lines; one comment of raw plus translated bytes equal to
    // 1 MiB is held, so the overload after it fails over.
    let overload_data = SPLIT_OVERLOAD.strip_prefix("event: error\n").unwrap();
    let comment = |len: usize| format!(": {}\n", "p".repeat(len - 2));
    // A completed 40-line split event, its blank line and `n` comments.
    let split_40 = |n: usize| {
        format!(
            "data: {{\"type\":\"response.in_progress\",\"items\":[\n{}data: ]}}\n\n{}{SPLIT_OVERLOAD}",
            "data: \n".repeat(38),
            ": k\n".repeat(n)
        )
    };
    for (name, sse) in [
        ("47 comments", format!("{}{SPLIT_OVERLOAD}", ": k\n".repeat(47))),
        ("40 split lines and 6 comments", split_40(6)),
        ("1 MiB exactly", format!("{}{overload_data}", comment(512 * 1024))),
    ] {
        let error = through_sse(&sse, true, true)
            .await
            .expect_err(&format!("{name}: fails over"));
        assert_eq!(error.status, 503, "{name}: {error:?}");
    }
    // Go sends a delta as read, which starts the stream before the overload after it in
    // the same event is seen.
    let delta_first =
        format!("{SPLIT_CREATED}data: {{\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}}\n{SPLIT_OVERLOAD}");
    // A split handshake that completes over the byte budget (600 KB read plus 600 KB
    // translated) releases the stream before the overload after it is read.
    let completed_over_bytes = format!(
        "data: {{\"type\":\"response.in_progress\",\"response\":{{\"model\":\"gpt-5.4\"}},\"pad\":\"{}\",\ndata: \"n\":0}}\n{SPLIT_OVERLOAD}",
        "x".repeat(600_000)
    );
    let comments_48 = format!("{}{SPLIT_OVERLOAD}", ": k\n".repeat(48));
    let comments_49 = format!("{}{SPLIT_OVERLOAD}", ": k\n".repeat(49));
    let over_one_mib = format!("{}{overload_data}", comment(512 * 1024 + 1));
    let split_40_and_7 = split_40(7);
    for (name, sse) in [
        ("completed over the byte budget", &completed_over_bytes),
        ("48 comments", &comments_48),
        ("49 comments", &comments_49),
        ("1 MiB and one byte", &over_one_mib),
        ("40 split lines and 7 comments", &split_40_and_7),
        ("delta first", &delta_first),
        ("line budget", &over_lines),
        ("no blank line", &no_blank),
        ("comments", &comments),
        ("byte budget", &over_bytes),
    ] {
        let (frames, failure) = through_sse(sse, true, true)
            .await
            .unwrap_or_else(|e| panic!("{name}: the attempt failed over: {e:?}"));
        assert!(failure.is_some(), "{name}: the failure arrives in-stream");
        if name == "delta first" {
            assert!(frames.iter().any(|f| f.contains("\"delta\":\"hi\"")), "{frames:?}");
        }
        if name == "no blank line" {
            let progress = frames.iter().find(|f| f.contains("response.in_progress")).unwrap();
            let data = progress.lines().find_map(|l| l.strip_prefix("data: ")).unwrap();
            let value: Value = serde_json::from_str(data).unwrap();
            assert_eq!(
                value["response"]["items"],
                serde_json::json!([{"key_at_boundary": 123}, {"key_after_boundary": 456}])
            );
        }
    }
}

/// A Responses history may contain completed built-in tool calls without requesting
/// those tools again. Native HTTP and compaction forward that distinction unchanged.
#[tokio::test]
async fn native_web_search_history_does_not_synthesize_tools_or_instructions() {
    for stream in [false, true] {
        for compact in [false, true] {
            if compact && stream {
                continue;
            }
            for tools in ["", ",\"tools\":[]", ",\"tools\":[{\"type\":\"web_search\"}]"] {
                for lite in [false, true] {
                    let mock = Mock::start().await;
                    let path = if compact { "/responses/compact" } else { "/responses" };
                    let response = if stream {
                        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"search-contract\",\"status\":\"completed\",\"output\":[]}}\n\n"
                    } else {
                        r#"{"id":"search-contract","object":"response","status":"completed","output":[]}"#
                    };
                    mock.script(
                        path,
                        vec![Reply {
                            status: 200,
                            headers: vec![(
                                "content-type".into(),
                                if stream {
                                    "text/event-stream"
                                } else {
                                    "application/json"
                                }
                                .into(),
                            )],
                            body: response.into(),
                        }],
                    );
                    let raw = format!(
                        r#"{{ "model":"gpt-5.4", "input":[{{"type":"message","role":"user","content":"search contract"}},{{"id":"search-1","type":"web_search_call","status":"completed","action":{{"type":"search","query":"Rust contract"}}}}],"unknown":9007199254740993{tools} }}"#
                    );
                    let mut req = plain_request(stream, compact.then_some("responses/compact"));
                    req.source_format = Format::OpenAIResponse;
                    req.response_format = Format::OpenAIResponse;
                    req.body = Bytes::from(raw.clone());
                    req.original_body = req.body.clone();
                    if lite {
                        req.headers
                            .insert("x-openai-internal-codex-responses-lite", "true".parse().unwrap());
                    }
                    let mut credential = oauth_credential(&mock.url);
                    credential
                        .attributes
                        .insert("api_key".into(), "private-contract".into());
                    let config = Config::parse("disable-image-generation: true\n").unwrap();
                    let response = executor().execute(&credential, req, &config).await.unwrap();
                    if let ResponseBody::Stream(mut events) = response.body {
                        while let Some(event) = events.next().await {
                            event.unwrap();
                        }
                    }
                    let sent = mock.take();
                    assert_eq!(sent.len(), 1);
                    assert_eq!(sent[0].path, path);
                    assert_eq!(sent[0].body.as_ref(), raw.as_bytes());
                }
            }
        }
    }
}

fn siwc_credential(base_url: &str) -> Credential {
    let mut credential=Credential::from_file(Path::new("/fake"),Path::new("/fake/siwc.json"),
        serde_json::json!({"type":"codex","access_token":"outer-contract","account_id":"outer-account","use_siwc":true,"siwc":{"access_token":"  siwc-contract  ","client_id":"issued-contract","refresh_token":"private-refresh"}}).as_object().unwrap().clone()).unwrap();
    credential.attributes.insert("base_url".into(), base_url.into());
    credential.attributes.insert("use_siwc".into(), "false".into());
    credential
        .attributes
        .insert("header:X-Operator".into(), "operator-contract".into());
    credential
}

/// The SIWC partition selects authentication without rewriting native Responses
/// semantics. Exact input bytes include fields a recorder used to discard.
#[tokio::test]
async fn siwc_http_profiles_keep_native_bytes_and_use_only_the_selected_partition() {
    for stream in [false, true] {
        for compact in [false, true] {
            if compact && stream {
                continue;
            }
            for instructions in ["", "be brief"] {
                let mock = Mock::start().await;
                let path = if compact { "/responses/compact" } else { "/responses" };
                let reply = if stream {
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"status\":\"completed\",\"output\":[]}}\n\n"
                } else {
                    r#"{"id":"r","object":"response","output":[],"status":"completed"}"#
                };
                mock.script(
                    path,
                    vec![Reply {
                        status: 200,
                        headers: vec![(
                            "content-type".into(),
                            if stream {
                                "text/event-stream"
                            } else {
                                "application/json"
                            }
                            .into(),
                        )],
                        body: reply.into(),
                    }],
                );
                let raw = format!(
                    r#"{{ "model":"gpt-5.4", "input":[{{"type":"configuration_update","reasoning":{{"effort":"low"}}}},{{"type":"message","role":"user","content":[{{"type":"input_text","text":"contract"}}]}}],"instructions":{instructions:?},"stream":{stream},"store":true,"temperature":0.5,"max_output_tokens":100,"prompt_cache_retention":"24h","prompt_cache_options":{{"mode":"keep"}},"prompt_cache_key":"native-cache","unknown":9007199254740993 }}"#
                );
                let mut req = plain_request(stream, compact.then_some("responses/compact"));
                req.body = Bytes::from(raw.clone());
                req.original_body = req.body.clone();
                req.headers
                    .insert("user-agent", "siwc-client-contract".parse().unwrap());
                req.headers
                    .insert("authorization", "Bearer caller-key-never-upstream".parse().unwrap());
                let response = executor()
                    .execute(&siwc_credential(&mock.url), req, &Config::default())
                    .await
                    .unwrap();
                if let ResponseBody::Stream(mut events) = response.body {
                    while let Some(event) = events.next().await {
                        event.unwrap();
                    }
                }
                let sent = mock.take();
                assert_eq!(sent.len(), 1);
                assert_eq!(sent[0].path, path);
                assert_eq!(sent[0].body.as_ref(), raw.as_bytes());
                let headers = &sent[0].headers;
                assert_eq!(headers["authorization"], "Bearer siwc-contract");
                assert_eq!(headers["user-agent"], "siwc-client-contract");
                assert_eq!(
                    headers["accept"],
                    if stream {
                        "text/event-stream"
                    } else {
                        "application/json"
                    }
                );
                assert_eq!(headers["x-operator"], "operator-contract");
                assert!(
                    !headers.contains_key("chatgpt-account-id"),
                    "outer account is not the SIWC partition"
                );
            }
        }
    }
    let mock = Mock::start().await;
    let mut credential = siwc_credential(&mock.url);
    credential.metadata["siwc"] = serde_json::json!({"client_id":"issued-contract"});
    let error = executor()
        .execute(&credential, plain_request(true, None), &Config::default())
        .await
        .err()
        .unwrap();
    assert_eq!((error.status, error.scope), (401, FailureScope::Credential));
    assert!(String::from_utf8_lossy(&error.body).contains("siwc access_token is missing"));
    assert!(mock.take().is_empty());
    let error = executor()
        .execute(
            &siwc_credential(&mock.url),
            {
                let mut req = plain_request(true, None);
                req.model = "gpt-5.4(low)".into();
                req
            },
            &Config::default(),
        )
        .await
        .err()
        .unwrap();
    assert_eq!((error.status, error.scope), (400, FailureScope::Request));
    assert!(mock.take().is_empty());
    let production = siwc_credential("");
    let mut production = production;
    production.attributes.remove("base_url");
    let view = View::new(&production);
    assert_eq!(
        (view.base_url, view.token),
        ("https://api.openai.com/v1", "siwc-contract")
    );
}

#[test]
fn siwc_websocket_headers_keep_protocol_metadata_and_do_not_use_the_outer_account() {
    let credential = siwc_credential("http://local.invalid");
    let view = View::new(&credential);
    let cfg = Config::default();
    let settings = Settings::scoped(&cfg, &view);
    for beta in ["responses_websockets=2026-09-01", "assistants=v2"] {
        let mut headers = HeaderMap::new();
        for (key, value) in [
            ("user-agent", "siwc-client-contract"),
            ("openai-beta", beta),
            ("x-client-request-id", "client-request"),
            ("conversation_id", "outer-conversation"),
        ] {
            headers.insert(key, value.parse().unwrap());
        }
        let headers = request::ws_headers(
            &view,
            &settings,
            &headers,
            "{}",
            "gpt-5.4",
            Some("session-contract"),
            true,
            crate::providers::codex_identity::Origin::Caller,
        );
        assert_eq!(headers["authorization"], "Bearer siwc-contract");
        assert_eq!(headers["user-agent"], "siwc-client-contract");
        assert_eq!(headers["session_id"], "session-contract");
        assert_eq!(headers["x-client-request-id"], "session-contract");
        assert_eq!(
            headers["openai-beta"],
            if beta.starts_with("responses_websockets=") {
                beta
            } else {
                request::WS_BETA
            }
        );
        assert_eq!(headers["x-operator"], "operator-contract");
        assert!(!headers.contains_key("conversation_id"));
        assert!(!headers.contains_key("chatgpt-account-id"));
    }
}

#[tokio::test]
async fn siwc_image_models_use_the_responses_image_tool_and_proxy_identity() {
    let mock = Mock::start().await;
    mock.script("/responses",vec![Reply{status:200,headers:vec![("content-type".into(),"text/event-stream".into())],body:"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"status\":\"completed\",\"created_at\":17,\"output\":[{\"type\":\"image_generation_call\",\"result\":\"SU1H\"}]}}\n\n".into()}]);
    let path = "/v1/images/generations";
    let response = executor()
        .images(
            &siwc_credential(&mock.url),
            image_request(
                br#"{"model":"gpt-image-2","prompt":"paint a contract"}"#.to_vec(),
                "gpt-image-2",
                false,
                path,
            ),
            path,
            &Config::default(),
        )
        .await
        .unwrap();
    let sent = mock.take();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].path, "/responses");
    assert_eq!(sent[0].headers["authorization"], "Bearer siwc-contract");
    assert_eq!(sent[0].headers["user-agent"], PROXY_UA);
    let body: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(body["tools"][0]["model"], "gpt-image-2");
    assert_eq!(body["input"][0]["content"][0]["text"], "paint a contract");
    let ResponseBody::Buffered(result) = response.body else {
        panic!("buffered image result")
    };
    let result: Value = serde_json::from_slice(&result).unwrap();
    assert_eq!(result["data"][0]["b64_json"], "SU1H");
}

/// Sign in with ChatGPT refresh (Go `refreshLocked`): the `siwc` partition refreshes on
/// its own 5-minute lead, a rejected token refreshes the partition requests use, a
/// management refresh both, and one partition failing keeps the other's result.
#[tokio::test]
async fn refresh_partitions_schedule_independently_retain_success_and_retry_only_failures() {
    use crate::providers::codex_testkit::json;
    const OUTER: &str = "/oauth/token";
    const SIWC: &str = "/api/accounts/oauth/token";
    let mock = Mock::start().await;
    let oauth = CodexOAuth::with_endpoints(wreq::Client::new(), &format!("{}{OUTER}", mock.url), "", "")
        .with_siwc_token_url(&format!("{}{SIWC}", mock.url))
        .with_retry_delay(std::time::Duration::from_millis(1));
    let executor = CodexExecutor::with_client(wreq::Client::new(), oauth);
    let cfg = Config::default();
    let at = |minutes: i64| (Utc::now() + chrono::TimeDelta::minutes(minutes)).to_rfc3339();
    // Unique refresh tokens per case: results are single-flighted and retained.
    let cred = |id: &str, use_siwc: bool, siwc_expires: &str| {
        Credential::from_file(
            Path::new("/fake"),
            Path::new("/fake/c.json"),
            serde_json::json!({
                "type": "codex", "use_siwc": use_siwc,
                "access_token": "outer-at", "refresh_token": format!("outer-{id}"), "expired": at(5 * 24 * 60),
                "siwc": {"access_token": "siwc-at", "refresh_token": format!("siwc-{id}"), "client_id": "client-1",
                         "device_id": "dev-1", "expired": siwc_expires, "custom": "kept"},
            })
            .as_object()
            .unwrap()
            .clone(),
        )
        .unwrap()
    };
    let outer_ok = || {
        json(
            200,
            r#"{"access_token":"outer-new","refresh_token":"outer-rt-new","id_token":"","expires_in":3600}"#,
        )
    };
    let siwc_ok = || {
        json(
            200,
            r#"{"access_token":"siwc-new","refresh_token":"siwc-rt-new","scope":"openid chatgpt.tokens.use.direct","expires_in":3600}"#,
        )
    };
    let paths = |sent: Vec<Captured>| sent.into_iter().map(|c| c.path).collect::<Vec<_>>();

    // The 5-minute lead: due at four minutes left, not at six.
    assert!(!executor.needs_prepare(&cred("a", true, &at(6)), &cfg));
    let due = cred("b", true, &at(4));
    assert!(executor.needs_prepare(&due, &cfg));
    mock.script(SIWC, vec![siwc_ok()]);
    let patch = executor.prepare(&due, &cfg).await.unwrap();
    let sent = mock.take();
    assert_eq!(paths(sent.clone()), [SIWC], "only the due partition refreshes");
    assert_eq!(
        url::form_urlencoded::parse(&sent[0].body)
            .into_owned()
            .collect::<std::collections::BTreeMap<_, _>>(),
        std::collections::BTreeMap::from([
            ("client_id".into(), "client-1".into()),
            ("grant_type".into(), "refresh_token".into()),
            ("refresh_token".into(), "siwc-b".into()),
            ("resource".into(), "https://api.openai.com/v1".into())
        ])
    );
    assert_eq!(patch.set.keys().collect::<Vec<_>>(), ["siwc"]);
    let siwc = &patch.set["siwc"];
    assert_eq!(
        (&siwc["access_token"], &siwc["refresh_token"], &siwc["client_id"]),
        (&"siwc-new".into(), &"siwc-rt-new".into(), &"client-1".into())
    );
    assert_eq!((&siwc["device_id"], &siwc["custom"]), (&"dev-1".into(), &"kept".into()));

    // A rejected token: the partition requests use, whichever is due otherwise.
    mock.script(SIWC, vec![siwc_ok()]);
    executor.refresh_now(&cred("c", true, &at(60)), &cfg).await.unwrap();
    assert_eq!(paths(mock.take()), [SIWC]);
    mock.script(OUTER, vec![outer_ok()]);
    let patch = executor.refresh_now(&cred("d", false, &at(60)), &cfg).await.unwrap();
    assert_eq!(paths(mock.take()), [OUTER]);
    assert!(!patch.set.contains_key("siwc"));

    // A management refresh: both, outer first.
    mock.script(OUTER, vec![outer_ok()]);
    mock.script(SIWC, vec![siwc_ok()]);
    let patch = executor.refresh_all(&cred("e", false, &at(60)), &cfg).await.unwrap();
    assert_eq!(paths(mock.take()), [OUTER, SIWC]);
    assert_eq!(
        (&patch.set["access_token"], &patch.set["siwc"]["access_token"]),
        (&"outer-new".into(), &"siwc-new".into())
    );

    // One partition failing keeps the other's refresh; both failing returns the outer error.
    mock.script(OUTER, vec![json(500, "{}"); 3]);
    mock.script(SIWC, vec![siwc_ok()]);
    let patch = executor.refresh_all(&cred("f", false, &at(60)), &cfg).await.unwrap();
    assert_eq!(patch.set.keys().collect::<Vec<_>>(), ["siwc"]);
    mock.take();
    mock.script(OUTER, vec![outer_ok()]);
    mock.script(SIWC, vec![json(500, "{}"); 3]);
    let patch = executor.refresh_all(&cred("k", false, &at(60)), &cfg).await.unwrap();
    assert_eq!(patch.set["access_token"], "outer-new");
    assert!(!patch.set.contains_key("siwc"));
    assert_eq!(mock.take().len(), 4);
    // A retry right after: the outer success is retained, the SIWC failure is not (Go's
    // singleflight forgets it), so only the SIWC endpoint is called again.
    mock.script(SIWC, vec![siwc_ok()]);
    let patch = executor.refresh_all(&cred("k", false, &at(60)), &cfg).await.unwrap();
    assert_eq!(paths(mock.take()), [SIWC]);
    assert_eq!(patch.set["siwc"]["access_token"], "siwc-new");
    mock.script(OUTER, vec![json(500, "{}"); 3]);
    mock.script(SIWC, vec![json(500, "{}"); 3]);
    let error = executor
        .refresh_all(&cred("g", false, &at(60)), &cfg)
        .await
        .unwrap_err();
    assert!(
        String::from_utf8_lossy(&error.body).starts_with("token refresh failed after 3 attempts"),
        "{error:?}"
    );
    assert_eq!(mock.take().len(), 6);

    // refresh_token_reused stops the retries, and only the OAuth code is kept.
    mock.script(
        SIWC,
        vec![json(400, r#"{"error":"refresh_token_reused","echo":"secret"}"#)],
    );
    let error = executor.refresh_now(&cred("h", true, &at(60)), &cfg).await.unwrap_err();
    assert_eq!(mock.take().len(), 1);
    assert_eq!((error.status, error.scope), (401, FailureScope::Credential));
    assert_eq!(
        String::from_utf8_lossy(&error.body),
        "OpenAI OAuth token request failed (400): refresh_token_reused"
    );

    // A failed scheduled refresh keeps a still-valid SIWC token in use.
    mock.script(SIWC, vec![json(500, "{}"); 3]);
    assert!(
        executor
            .prepare(&cred("i", true, &at(4)), &cfg)
            .await
            .unwrap()
            .set
            .is_empty()
    );
    mock.take();
    mock.script(SIWC, vec![json(500, "{}"); 3]);
    assert!(executor.prepare(&cred("j", true, &at(-1)), &cfg).await.is_err());
}

/// TestPayloadBarrierCodexImageFilter, HTTP half (868e137): a filter removes what the
/// executor adds late (the image tool, the empty instructions, the prompt cache key).
#[tokio::test]
async fn payload_filters_remove_late_codex_additions() {
    let executor = executor();
    let cfg = Config::parse(
        "payload:\n  filter:\n    - models: [{name: gpt-5.6-sol}]\n      params: ['tools.#(type==\"image_generation\")#', instructions, prompt_cache_key]\n",
    )
    .unwrap();
    for stream in [false, true] {
        for model in ["gpt-5.6-sol", "gpt-5.6-luna"] {
            let mock = Mock::start().await;
            mock.script(
                "/responses",
                vec![Reply {
                    status: 200,
                    headers: vec![("content-type".into(), "text/event-stream".into())],
                    body: "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_barrier\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":0,\"output_tokens\":0,\"total_tokens\":0}}}\n\n".into(),
                }],
            );
            let mut credential = oauth_credential(&mock.url);
            credential.attributes.insert("api_key".into(), "test".into());
            credential.attributes.insert("plan_type".into(), "pro".into());
            let mut req = plain_request(stream, None);
            req.source_format = Format::OpenAIResponse;
            req.response_format = Format::OpenAIResponse;
            req.model = model.into();
            req.requested_model = model.into();
            req.body = Bytes::from_static(br#"{"input":"hello","prompt_cache_key":"injected-cache"}"#);
            req.original_body = req.body.clone();
            match executor.execute(&credential, req, &cfg).await.unwrap().body {
                ResponseBody::Stream(mut stream) => while stream.next().await.is_some() {},
                ResponseBody::Buffered(_) => {}
            }
            let sent = String::from_utf8(mock.take().pop().unwrap().body.to_vec()).unwrap();
            // A Responses request on an API key takes the native route (#23): the executor
            // adds nothing late, so no rule has anything to remove, and the payload rules
            // do not run. Only the routed `model` is added. Translated requests keep the
            // late additions and the barrier until #27.
            assert_eq!(
                sent,
                format!(r#"{{"model":"{model}","input":"hello","prompt_cache_key":"injected-cache"}}"#),
                "{model} stream={stream}"
            );
        }
    }
}

/// Go 868e137 gives CountTokens the payload barrier: the rules run on the shaped body
/// before the tokenizer, so a configured `input` changes the count.
#[test]
fn count_tokens_ignores_the_payload_rules_and_is_marked_local() {
    let count = |config: &str| {
        let mut req = plain_request(false, None);
        req.operation = Operation::CountTokens;
        req.body = Bytes::from_static(br#"{"model":"gpt-5.4","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#);
        req.original_body = req.body.clone();
        let out = super::count_tokens(&req, &Config::parse(config).unwrap()).unwrap();
        let ResponseBody::Buffered(bytes) = out.body else {
            panic!("count answers are buffered")
        };
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        crate::protocol::rawjson::get(&text, "response.usage.input_tokens").i64()
    };
    let plain = count("");
    let configured = count(
        "payload:\n  override:\n    - models: [{name: gpt-5.4}]\n      params: {input.0.content.0.text: one two three four five six seven eight nine ten}\n",
    );
    // #27: the count reads the body the request would send, which payload rules no
    // longer change on the native and translated routes.
    assert!(plain > 0);
    assert_eq!(configured, plain, "{plain} -> {configured}");
    let mut req = plain_request(false, None);
    req.operation = Operation::CountTokens;
    let out = super::count_tokens(&req, &Config::default()).unwrap();
    assert_eq!(out.headers[super::LOCAL_TOKEN_COUNT], "local-estimate");
}

/// Issue #8: a WebSocket turn keeps an explicit `service_tier`, and the handshake's
/// routing hint names the same tier.
#[test]
fn native_codex_websocket_turns_keep_service_tier_in_body_and_routing_hint() {
    let shaped = |ua: &str, tier: &str| {
        let credential = oauth_credential("http://127.0.0.1:1");
        let mut req = plain_request(true, None);
        (req.source_format, req.response_format) = (Format::OpenAIResponse, Format::OpenAIResponse);
        req.headers.insert("user-agent", ua.parse().unwrap());
        req.body = Bytes::from(format!(
            r#"{{"model":"gpt-5.4","input":[],"store":false,"stream":true{tier}}}"#
        ));
        req.original_body = req.body.clone();
        let view = View::new(&credential);
        let settings = Settings::scoped(&Config::default(), &view);
        // A native frame goes upstream as the client sent it (#24, #28).
        let body = String::from_utf8(req.body.to_vec()).unwrap();
        let headers = request::ws_headers(
            &view,
            &settings,
            &req.headers,
            &body,
            "gpt-5.4",
            None,
            true,
            crate::providers::codex_identity::Origin::of(&req),
        );
        let hint = headers
            .get("x-codex-routing-hint")
            .map(|v| v.to_str().unwrap().to_owned());
        (
            crate::protocol::rawjson::get(&body, "service_tier").json().to_owned(),
            hint,
        )
    };
    let codex = "codex-tui/0.160.0 (Mac OS 26.5.2; arm64)";
    for (tier, want, want_hint) in [
        (r#","service_tier":"flex""#, r#""flex""#, "model=gpt-5.4;tier=flex"),
        (
            r#","service_tier":"priority""#,
            r#""priority""#,
            "model=gpt-5.4;tier=priority",
        ),
        (
            r#","service_tier":"ultrafast""#,
            r#""ultrafast""#,
            "model=gpt-5.4;tier=ultrafast",
        ),
        (r#","service_tier":"scale""#, r#""scale""#, "model=gpt-5.4;tier=scale"),
        ("", "", "model=gpt-5.4"),
    ] {
        assert_eq!(
            shaped(codex, tier),
            (want.to_owned(), Some(want_hint.to_owned())),
            "{tier}"
        );
    }
    // Any Responses client: no client-dependent tier rule remains (#28).
    assert_eq!(
        shaped("OpenAI/Python 2.1.0", r#","service_tier":"flex""#),
        (r#""flex""#.to_owned(), Some("model=gpt-5.4;tier=flex".to_owned()))
    );
}

/// The Images API tool adapter (#27) sends only what the conversion needs: the model,
/// the forced `image_generation` tool with the client's settings, the prompt, `stream`
/// and `store`. Payload rules cannot rewrite it, and an Images field the tool cannot
/// express is refused before anything is sent.
#[tokio::test]
async fn the_image_tool_adapter_adds_nothing() {
    let executor = executor();
    let sse = concat!(
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"image_generation_call\",\"id\":\"ig_0\",\"status\":\"completed\",\"result\":\"SU1HMQ==\",\"output_format\":\"png\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_img\",\"created_at\":1700000000,\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":2,\"total_tokens\":3}}}\n\n",
    );
    let rules = "payload:\n  override:\n    - models: [{name: \"*\"}]\n      params: {model: policy/image-model, instructions: injected, reasoning.effort: high, \"audit.-1\": once}\n";
    for (body, refused) in [
        (r#"{"prompt":"a cat","size":"1024x1024"}"#, None),
        (r#"{"prompt":"a cat","style":"vivid"}"#, Some("style")),
        (r#"{"prompt":"a cat","n":2}"#, Some("`n`")),
    ] {
        let mock = Mock::start().await;
        mock.script(
            "/responses",
            vec![Reply {
                status: 200,
                headers: vec![("content-type".into(), "text/event-stream".into())],
                body: sse.into(),
            }],
        );
        let case = serde_json::json!({"source": "openai", "model": "gpt-5.4", "payload": body, "attributes": {}, "metadata": {"type": "codex", "access_token": "at-FAKE"}});
        let credential = credential(&case, &mock.url);
        let mut req = request(&case);
        req.request_path = "/v1/images/generations".into();
        let result = executor
            .images(
                &credential,
                req,
                "/v1/images/generations",
                &Config::parse(rules).unwrap(),
            )
            .await;
        let captured = mock.take();
        if let Some(field) = refused {
            let error = result.err().unwrap_or_else(|| panic!("{field}: refused"));
            assert_eq!(error.status, 400, "{field}");
            assert!(String::from_utf8_lossy(&error.body).contains(field), "{field}");
            assert!(captured.is_empty(), "{field}: nothing is sent");
            continue;
        }
        result.unwrap();
        let sent: Value = serde_json::from_slice(&captured[0].body).unwrap();
        let mut keys: Vec<&str> = sent.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["input", "model", "store", "stream", "tool_choice", "tools"],
            "{sent}"
        );
        assert_eq!(sent["tools"][0]["type"], "image_generation");
        assert_eq!(sent["tools"][0]["size"], "1024x1024");
        assert!(
            !sent.to_string().contains("policy/image-model"),
            "no payload rule: {sent}"
        );
    }
}

/// The image tool adapter keeps every image reference it accepts (#27): a JSON edit's
/// file-ID image and file-ID mask become the Responses `input_image` and
/// `input_image_mask` of the same form, and an image without a reference is refused.
/// Multipart edits follow the same field policy as JSON.
#[tokio::test]
async fn image_references_are_kept_or_refused() {
    let executor = executor();
    let sse = concat!(
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"image_generation_call\",\"id\":\"ig_0\",\"status\":\"completed\",\"result\":\"SU1HMQ==\",\"output_format\":\"png\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_img\",\"created_at\":1700000000,\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":2,\"total_tokens\":3}}}\n\n",
    );
    let form = |fields: &str| {
        format!(
            "--XB\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nedit it\r\n{fields}--XB\r\nContent-Disposition: form-data; name=\"image\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\nPNG\r\n--XB--\r\n"
        )
    };
    let field =
        |name: &str, value: &str| format!("--XB\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n");
    let extra_file = "--XB\r\nContent-Disposition: form-data; name=\"thumbnail\"; filename=\"t.png\"\r\nContent-Type: image/png\r\n\r\nPNG\r\n";
    let cases: Vec<(String, &str, Option<&str>)> = vec![
        (
            r#"{"prompt":"edit it","images":[{"file_id":"file-IMG"},{"image_url":"https://example.com/b.png"}],"mask":{"file_id":"file-MASK"}}"#.into(),
            "application/json",
            None,
        ),
        (
            r#"{"prompt":"edit it","images":[{"file_id":"file-IMG"},{"detail":"high"}]}"#.into(),
            "application/json",
            Some("an image without exactly one image_url or file_id"),
        ),
        (form(&field("n", "2")), "multipart/form-data; boundary=XB", Some("`n`")),
        (form(&field("style", "vivid")), "multipart/form-data; boundary=XB", Some("style")),
        (form(extra_file), "multipart/form-data; boundary=XB", Some("thumbnail")),
    ];
    for (body, content_type, refused) in cases {
        let mock = Mock::start().await;
        mock.script(
            "/responses",
            vec![Reply {
                status: 200,
                headers: vec![("content-type".into(), "text/event-stream".into())],
                body: sse.into(),
            }],
        );
        let case = serde_json::json!({"source": "openai", "model": "gpt-5.4", "payload": body, "attributes": {}, "metadata": {"type": "codex", "access_token": "at-FAKE"}});
        let credential = credential(&case, &mock.url);
        let mut req = request(&case);
        req.request_path = "/v1/images/edits".into();
        req.headers
            .insert(http::header::CONTENT_TYPE, content_type.parse().unwrap());
        let result = executor
            .images(&credential, req, "/v1/images/edits", &Config::default())
            .await;
        let captured = mock.take();
        if let Some(field) = refused {
            let error = result.err().unwrap_or_else(|| panic!("{field}: refused"));
            assert_eq!(error.status, 400, "{field}");
            assert!(
                String::from_utf8_lossy(&error.body).contains(field),
                "{field}: {:?}",
                error.body
            );
            assert!(captured.is_empty(), "{field}: nothing is sent");
            continue;
        }
        result.unwrap();
        let sent: Value = serde_json::from_slice(&captured[0].body).unwrap();
        let content = &sent["input"][0]["content"];
        assert_eq!(
            content[1],
            serde_json::json!({"type": "input_image", "file_id": "file-IMG"}),
            "{sent}"
        );
        assert_eq!(content[2]["image_url"], "https://example.com/b.png", "{sent}");
        assert_eq!(
            sent["tools"][0]["input_image_mask"],
            serde_json::json!({"file_id": "file-MASK"}),
            "{sent}"
        );
    }
}

/// Contract probes for header inspection, not captured client traffic. A cache key is
/// opaque text; other JSON types still travel in the body, but cannot become identity.
#[test]
fn codex_cache_identity_reads_strings_without_gjson_coercion() {
    for (raw, expected) in [
        (
            r#"{"prompt_cache_key":"opaque\u002dkey","future":1e400}"#,
            Some("opaque-key"),
        ),
        (r#"{"prompt_cache_key":"  opaque key  "}"#, Some("  opaque key  ")),
        (r#"{"prompt_cache_key":123}"#, None),
        (r#"{"prompt_cache_key":true}"#, None),
        (r#"{"prompt_cache_key":{"future":1e400}}"#, None),
        (r#"{"prompt_cache_key":null}"#, None),
        (r#"{}"#, None),
    ] {
        let mut req = plain_request(true, None);
        req.source_format = Format::OpenAIResponse;
        req.body = Bytes::from(raw.to_owned());
        req.original_body = req.body.clone();
        req.execution_session = None;
        req.derived_session = None;
        assert_eq!(
            request::prompt_cache_id(&req, raw, None, false).as_deref(),
            expected,
            "{raw}"
        );
        assert_eq!(req.body.as_ref(), raw.as_bytes());
        req.derived_session = Some("fallback-session".into());
        let fallback = request::provider_session_uuid("derived-session", "fallback-session");
        assert_eq!(
            request::prompt_cache_id(&req, raw, None, true).as_deref(),
            expected.or(fallback.as_deref())
        );
    }
}

/// Header observation must not apply the native-only duplicate-key policy to raw
/// schemas retained by a translated request. This is a contract probe, not a fixture.
#[test]
fn codex_headers_read_raw_translated_schemas() {
    let raw = r#"{"model":"upstream","service_tier":"fl\u0065x","prompt_cache_key":"  client-key  ","tools":[{"parameters":{"future":1e400,"same":1,"same":2}}]}"#;
    let mut req = plain_request(true, None);
    req.source_format = Format::OpenAI;
    req.body = Bytes::from(raw.to_owned());
    req.original_body = req.body.clone();
    let credential = oauth_credential("http://127.0.0.1:1");
    let view = View::new(&credential);
    let cache = request::prompt_cache_id(&req, raw, None, false);
    assert_eq!(cache.as_deref(), Some("client-key"));
    let headers = request::http_headers(
        &view,
        &req.headers,
        raw,
        "upstream",
        cache.as_deref(),
        true,
        crate::providers::codex_identity::Origin::Proxy,
    );
    assert_eq!(headers["x-codex-routing-hint"], "model=upstream;tier=flex");
    assert_eq!(req.body.as_ref(), raw.as_bytes());
}

#[test]
fn translated_header_observation_keeps_independent_first_matches() {
    for raw in [
        r#"{"service_tier":"flex","service_tier":"flex","prompt_cache_key":"cache-FAKE"}"#,
        r#"{"service_tier":"flex","service_\u0074ier":"flex","prompt_cache_key":"cache-FAKE"}"#,
        r#"{"model":"upstream","model":"upstream","service_tier":"flex","prompt_cache_key":"cache-FAKE"}"#,
        r#"{"service_tier":"flex","prompt_cache_key":"cache-FAKE","prompt_cache_key":"later-FAKE"}"#,
        r#"{"client_metadata":null,"client_metadata":{},"service_tier":"flex","prompt_cache_key":"cache-FAKE"}"#,
    ] {
        let mut req = plain_request(true, None);
        req.source_format = Format::OpenAI;
        req.body = Bytes::from(raw.to_owned());
        req.original_body = req.body.clone();
        let cache = request::prompt_cache_id(&req, raw, None, false);
        assert_eq!(cache.as_deref(), Some("cache-FAKE"), "{raw}");
        let credential = oauth_credential("http://127.0.0.1:1");
        let headers = request::http_headers(
            &View::new(&credential),
            &req.headers,
            raw,
            "upstream",
            cache.as_deref(),
            true,
            crate::providers::codex_identity::Origin::Proxy,
        );
        assert_eq!(headers["x-codex-routing-hint"], "model=upstream;tier=flex");
        assert_eq!(req.body.as_ref(), raw.as_bytes());
    }
}

// ------------------------------------------------------------------ N1 #111: request admission over TLS

/// What a local chatgpt.com saw of the application protocol: HTTP/2 request HEADERS and
/// DATA bytes, or HTTP/1.1 bytes, apart from TCP, TLS and the HTTP/2 connection setup.
#[derive(Default)]
struct Seen {
    tcp: std::sync::atomic::AtomicUsize,
    tls: std::sync::atomic::AtomicUsize,
    headers: std::sync::atomic::AtomicUsize,
    data: std::sync::atomic::AtomicUsize,
    http1: std::sync::atomic::AtomicUsize,
    /// Complete requests read: HTTP/2 streams whose body ended, HTTP/1.1 requests whose
    /// body arrived in full.
    requests: std::sync::atomic::AtomicUsize,
}

/// A local chatgpt.com over TLS. ALPN picks `alpn`. HTTP/2 connections advertise one
/// concurrent stream. While `hold_tls` is set, each new connection's TLS handshake waits
/// for `release_tls` after announcing the TCP accept (`tcp_accepted`): the client's dial
/// is held. While `hold_first` is set, the first HTTP/2 stream's answer waits for
/// `release_first`, keeping the connection's only stream slot busy. While `h1_keep_alive`
/// is set, a new HTTP/1.1 connection answers without `connection: close` and serves
/// further requests. When `drop_next` is set, the next request read in full gets no
/// answer: its connection is dropped (a stale or failing connection after the request
/// was sent).
struct AdmissionUpstream {
    addr: std::net::SocketAddr,
    ca: Vec<u8>,
    seen: Arc<Seen>,
    hold_tls: Arc<std::sync::atomic::AtomicBool>,
    tcp_accepted: Arc<tokio::sync::Notify>,
    release_tls: Arc<tokio::sync::Notify>,
    hold_first: Arc<std::sync::atomic::AtomicBool>,
    release_first: Arc<tokio::sync::Notify>,
    h1_keep_alive: Arc<std::sync::atomic::AtomicBool>,
    drop_next: Arc<std::sync::atomic::AtomicBool>,
    /// Notified for each HTTP/2 request HEADERS received.
    headers_seen: Arc<tokio::sync::Notify>,
    /// How many connections' server tasks have ended.
    ended: Arc<tokio::sync::watch::Sender<usize>>,
}

impl AdmissionUpstream {
    async fn start(alpn: &'static [u8]) -> Self {
        use std::sync::atomic::Ordering::SeqCst;
        let (ca, acceptor) = crate::providers::test_tls::acceptor(&["chatgpt.com".to_owned()], alpn);
        let acceptor = Arc::new(acceptor);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let up = Self {
            addr,
            ca,
            seen: Arc::default(),
            hold_tls: Arc::default(),
            tcp_accepted: Arc::default(),
            release_tls: Arc::default(),
            hold_first: Arc::default(),
            release_first: Arc::default(),
            h1_keep_alive: Arc::default(),
            drop_next: Arc::default(),
            headers_seen: Arc::default(),
            ended: Arc::new(tokio::sync::watch::Sender::new(0)),
        };
        let (h1_keep_alive, drop_next) = (up.h1_keep_alive.clone(), up.drop_next.clone());
        let headers_seen = up.headers_seen.clone();
        let ended = up.ended.clone();
        let (seen, hold_tls, tcp_accepted, release_tls, hold_first, release_first) = (
            up.seen.clone(),
            up.hold_tls.clone(),
            up.tcp_accepted.clone(),
            up.release_tls.clone(),
            up.hold_first.clone(),
            up.release_first.clone(),
        );
        tokio::spawn(async move {
            let first_stream = Arc::new(std::sync::atomic::AtomicBool::new(true));
            while let Ok((tcp, _)) = listener.accept().await {
                seen.tcp.fetch_add(1, SeqCst);
                let headers_seen = headers_seen.clone();
                let ended = ended.clone();
                let (keep_h1, drop_next) = (h1_keep_alive.load(SeqCst), drop_next.clone());
                let (acceptor, seen, hold, accepted, release, hold_first, release_first, first_stream) = (
                    acceptor.clone(),
                    seen.clone(),
                    hold_tls.load(SeqCst),
                    tcp_accepted.clone(),
                    release_tls.clone(),
                    hold_first.clone(),
                    release_first.clone(),
                    first_stream.clone(),
                );
                tokio::spawn(async move {
                    if hold {
                        accepted.notify_one();
                        release.notified().await;
                    }
                    struct Ended(Arc<tokio::sync::watch::Sender<usize>>);
                    impl Drop for Ended {
                        fn drop(&mut self) {
                            self.0.send_modify(|ended| *ended += 1);
                        }
                    }
                    let _ended = Ended(ended);
                    let ssl = btls::ssl::Ssl::new(acceptor.context()).unwrap();
                    let mut tls = tokio_btls::SslStream::new(ssl, tcp).unwrap();
                    if std::pin::Pin::new(&mut tls).accept().await.is_err() {
                        return;
                    }
                    seen.tls.fetch_add(1, SeqCst);
                    if tls.ssl().selected_alpn_protocol() == Some(b"h2") {
                        let tapped = FrameTap {
                            inner: tls,
                            seen: seen.clone(),
                            headers_seen: headers_seen.clone(),
                            buffered: Vec::new(),
                            preface: 0,
                        };
                        let Ok(mut conn) = http2::server::Builder::new()
                            .max_concurrent_streams(1)
                            .handshake::<_, Bytes>(tapped)
                            .await
                        else {
                            return;
                        };
                        let (dropped, drop_conn) = (Arc::new(tokio::sync::Notify::new()), drop_next.clone());
                        loop {
                            let accepted = tokio::select! {
                                accepted = conn.accept() => accepted,
                                () = dropped.notified() => return,
                            };
                            let Some(Ok((request, mut respond))) = accepted else {
                                break;
                            };
                            let (hold_first, release_first, first_stream) =
                                (hold_first.clone(), release_first.clone(), first_stream.clone());
                            let (seen, drop_conn, dropped) = (seen.clone(), drop_conn.clone(), dropped.clone());
                            tokio::spawn(async move {
                                let mut body = request.into_body();
                                while let Some(Ok(chunk)) = body.data().await {
                                    let _ = body.flow_control().release_capacity(chunk.len());
                                }
                                seen.requests.fetch_add(1, SeqCst);
                                if drop_conn.swap(false, SeqCst) {
                                    // No answer: the whole connection goes, without GOAWAY.
                                    dropped.notify_one();
                                    return;
                                }
                                if first_stream.swap(false, SeqCst) && hold_first.load(SeqCst) {
                                    release_first.notified().await;
                                }
                                let response = http::Response::builder()
                                    .status(200)
                                    .header("content-type", "text/event-stream")
                                    .body(())
                                    .unwrap();
                                if let Ok(mut out) = respond.send_response(response, false) {
                                    let _ = out.send_data(Bytes::from_static(b"data: {}\n\n"), true);
                                }
                            });
                        }
                    } else {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = [0; 4096];
                        let mut got = Vec::new();
                        loop {
                            match tls.read(&mut buf).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => {
                                    seen.http1.fetch_add(n, SeqCst);
                                    got.extend_from_slice(&buf[..n]);
                                }
                            }
                            if let Some(at) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                                let head = String::from_utf8_lossy(&got[..at]).to_ascii_lowercase();
                                let length = head
                                    .lines()
                                    .find_map(|l| l.strip_prefix("content-length:"))
                                    .and_then(|v| v.trim().parse::<usize>().ok())
                                    .unwrap_or(0);
                                if got.len() >= at + 4 + length {
                                    seen.requests.fetch_add(1, SeqCst);
                                    if drop_next.swap(false, SeqCst) {
                                        return;
                                    }
                                    if keep_h1 {
                                        got.drain(..at + 4 + length);
                                        if tls
                                            .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 10\r\n\r\ndata: {}\n\n")
                                            .await
                                            .is_err()
                                        {
                                            return;
                                        }
                                        continue;
                                    }
                                    let _ = tls
                                        .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 10\r\nconnection: close\r\n\r\ndata: {}\n\n")
                                        .await;
                                    let _ = tls.shutdown().await;
                                    return;
                                }
                            }
                        }
                    }
                });
            }
        });
        up
    }

    fn executor(&self) -> CodexExecutor {
        let mut executor = CodexExecutor::new().unwrap();
        executor.transport = crate::providers::codex_tls::Transport::new(crate::providers::proxy::Hooks {
            trust: Some(wreq::tls::trust::CertStore::from_pem_stack(self.ca.clone()).unwrap()),
            resolve: vec![("chatgpt.com".into(), self.addr)],
        });
        executor
    }

    fn url(&self) -> String {
        format!("https://chatgpt.com:{}/backend-api/codex/responses", self.addr.port())
    }

    /// Waits until every accepted connection's server task has ended, so every frame the
    /// client sent was read.
    async fn settled(&self) {
        let accepted = Self::count(&self.seen.tcp);
        let mut ended = self.ended.subscribe();
        tokio::time::timeout(std::time::Duration::from_secs(10), ended.wait_for(|n| *n >= accepted))
            .await
            .expect("every connection ends")
            .unwrap();
    }

    fn count(counter: &std::sync::atomic::AtomicUsize) -> usize {
        counter.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// The client half of a decrypted HTTP/2 connection, read through to the server while
/// its frames are counted as they arrive: request HEADERS frames and DATA payload bytes,
/// whatever the server later makes of a stream (a stream reset before the server
/// accepts it is still seen here). The connection preface and SETTINGS, PING and
/// WINDOW_UPDATE frames are setup, not request bytes.
struct FrameTap<S> {
    inner: S,
    seen: Arc<Seen>,
    headers_seen: Arc<tokio::sync::Notify>,
    buffered: Vec<u8>,
    preface: usize,
}

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for FrameTap<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use std::sync::atomic::Ordering::SeqCst;
        let this = &mut *self;
        let before = buf.filled().len();
        let polled = std::pin::Pin::new(&mut this.inner).poll_read(cx, buf);
        let mut fresh = &buf.filled()[before..];
        let skip = fresh.len().min(24 - this.preface);
        this.preface += skip;
        fresh = &fresh[skip..];
        this.buffered.extend_from_slice(fresh);
        while this.buffered.len() >= 9 {
            let len = (usize::from(this.buffered[0]) << 16)
                | (usize::from(this.buffered[1]) << 8)
                | usize::from(this.buffered[2]);
            if this.buffered.len() < 9 + len {
                break;
            }
            match this.buffered[3] {
                0x0 => {
                    this.seen.data.fetch_add(len, SeqCst);
                }
                0x1 => {
                    this.seen.headers.fetch_add(1, SeqCst);
                    this.headers_seen.notify_one();
                }
                _ => {}
            }
            this.buffered.drain(..9 + len);
        }
        polled
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for FrameTap<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        data: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, data)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

const ADMIT_H2: &[u8] = b"\x02h2\x08http/1.1";
const ADMIT_H1: &[u8] = b"\x08http/1.1";

/// A gate that refuses once `revoked` is set, counting its checks.
fn admission_gate(
    revoked: &Arc<std::sync::atomic::AtomicBool>,
    checks: &Arc<std::sync::atomic::AtomicUsize>,
) -> crate::providers::proxy::Gate {
    let (revoked, checks) = (revoked.clone(), checks.clone());
    Arc::new(move || {
        checks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        !revoked.load(std::sync::atomic::Ordering::SeqCst)
    })
}

fn admission_headers() -> http::HeaderMap {
    let mut headers = http::HeaderMap::new();
    headers.insert("authorization", "Bearer at-FAKE".parse().unwrap());
    headers.insert("content-type", "application/json".parse().unwrap());
    headers
}

const ADMISSION_BODY: &str = r#"{"model":"gpt-5.5","input":[],"stream":true}"#;

/// N1 #111 review: a gated request whose client key is revoked while its connection is
/// being dialled (the TLS handshake held by the upstream: a deterministic barrier) sends
/// no request at all. Over HTTP/2 the upstream sees the TLS handshake and nothing of the
/// request: no HEADERS frame, no DATA. Over HTTP/1.1 it reads no request byte. The
/// request is refused, not failed over. Controls: with the key accepted throughout, the
/// same request is served over both protocols.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_revoked_during_the_dial_sends_no_request_headers() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
    for (alpn, h2) in [(ADMIT_H2, true), (ADMIT_H1, false)] {
        for revoke in [true, false] {
            let case = format!("h2={h2} revoke={revoke}");
            let up = AdmissionUpstream::start(alpn).await;
            let executor = Arc::new(up.executor());
            up.hold_tls.store(true, SeqCst);
            let (revoked, checks) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicUsize::new(0)));
            let gate = admission_gate(&revoked, &checks);
            let send = tokio::spawn({
                let (executor, url) = (executor.clone(), up.url());
                async move {
                    let credential = oauth_credential("https://chatgpt.com/backend-api/codex");
                    let view = crate::providers::codex_request::View::for_request(&credential, &Config::default());
                    executor
                        .post_certain(
                            &view,
                            &url,
                            &admission_headers(),
                            Bytes::from_static(ADMISSION_BODY.as_bytes()),
                            Some(gate),
                        )
                        .await
                        .map(|upstream| upstream.status)
                        .map_err(|(error, unsent)| (error.status, error.scope, unsent))
                }
            });
            tokio::time::timeout(std::time::Duration::from_secs(5), up.tcp_accepted.notified())
                .await
                .expect("the dial reaches the upstream");
            if revoke {
                revoked.store(true, SeqCst);
            }
            up.release_tls.notify_one();
            let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), send)
                .await
                .unwrap()
                .unwrap();
            drop(executor);
            up.settled().await;
            assert_eq!(AdmissionUpstream::count(&up.seen.tls), 1, "{case}: the dial completed");
            if revoke {
                assert!(outcome.is_err(), "{case}: {outcome:?}");
                assert_eq!(
                    AdmissionUpstream::count(&up.seen.headers),
                    0,
                    "{case}: no request HEADERS"
                );
                assert_eq!(AdmissionUpstream::count(&up.seen.data), 0, "{case}: no DATA");
                assert_eq!(AdmissionUpstream::count(&up.seen.http1), 0, "{case}: no HTTP/1.1 byte");
            } else {
                assert_eq!(outcome, Ok(200), "{case}");
                if h2 {
                    assert_eq!(AdmissionUpstream::count(&up.seen.headers), 1, "{case}");
                    assert_eq!(AdmissionUpstream::count(&up.seen.data), ADMISSION_BODY.len(), "{case}");
                } else {
                    assert!(
                        AdmissionUpstream::count(&up.seen.http1) > ADMISSION_BODY.len(),
                        "{case}"
                    );
                }
            }
            assert_eq!(
                AdmissionUpstream::count(&up.seen.tcp),
                1,
                "{case}: one attempt, no failover"
            );
            assert!(checks.load(SeqCst) >= 1, "{case}");
        }
    }
}

/// N1 #111 review: a gated request never waits on, or queues its HEADERS into, a shared
/// HTTP/2 connection. A pooled `chatgpt-keep-alive` connection whose only stream slot is
/// busy is left alone. With the key revoked, the gated request sends nothing on that
/// connection, or on any other, even once the slot frees. The busy stream still
/// completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gated_request_never_queues_headers_on_a_busy_pooled_http2_connection() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
    let up = AdmissionUpstream::start(ADMIT_H2).await;
    let executor = Arc::new(up.executor());
    up.hold_first.store(true, SeqCst);
    let busy = tokio::spawn({
        let (executor, url) = (executor.clone(), up.url());
        async move {
            let credential = oauth_credential("https://chatgpt.com/backend-api/codex");
            let mut view = crate::providers::codex_request::View::for_request(&credential, &Config::default());
            view.chatgpt_keep_alive = true;
            let upstream = executor
                .post_certain(
                    &view,
                    &url,
                    &admission_headers(),
                    Bytes::from_static(ADMISSION_BODY.as_bytes()),
                    None,
                )
                .await
                .map_err(|(error, _)| error.status)?;
            let mut body = upstream.body;
            while let Some(chunk) = body.next().await {
                chunk.map_err(|error| error.status)?;
            }
            Ok::<u16, u16>(upstream.status)
        }
    });
    // The pooled connection's only stream is now open and held.
    tokio::time::timeout(std::time::Duration::from_secs(5), up.headers_seen.notified())
        .await
        .expect("the busy stream opens");
    assert_eq!(AdmissionUpstream::count(&up.seen.headers), 1, "the busy stream is open");
    // The key is revoked; the gated request starts while the slot is busy, and the slot
    // is freed at once: a request queued on the pooled connection would now send its
    // HEADERS there.
    let (revoked, checks) = (Arc::new(AtomicBool::new(true)), Arc::new(AtomicUsize::new(0)));
    let gated = tokio::spawn({
        let (executor, url, gate) = (executor.clone(), up.url(), admission_gate(&revoked, &checks));
        async move {
            let credential = oauth_credential("https://chatgpt.com/backend-api/codex");
            let mut view = crate::providers::codex_request::View::for_request(&credential, &Config::default());
            view.chatgpt_keep_alive = true;
            executor
                .post_certain(
                    &view,
                    &url,
                    &admission_headers(),
                    Bytes::from_static(ADMISSION_BODY.as_bytes()),
                    Some(gate),
                )
                .await
                .map(|upstream| upstream.status)
                .map_err(|(error, _)| error.status)
        }
    });
    up.release_first.notify_one();
    let refused = tokio::time::timeout(std::time::Duration::from_secs(10), gated)
        .await
        .expect("the gated request ends")
        .unwrap();
    assert!(refused.is_err(), "the gated request is refused: {refused:?}");
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(10), busy)
            .await
            .unwrap()
            .unwrap(),
        Ok(200),
        "the busy stream completes"
    );
    drop(executor);
    up.settled().await;
    assert_eq!(
        AdmissionUpstream::count(&up.seen.headers),
        1,
        "no HEADERS from the gated request, on the pooled connection or any other"
    );
    assert_eq!(AdmissionUpstream::count(&up.seen.data), ADMISSION_BODY.len());
}

/// `oauth.providers.codex.chatgpt-keep-alive` as the config file spells it (#147): absent
/// (the default, reuse), explicit `true`, and explicit `false` (the operator's opt-out,
/// Go's connection per request).
const KEEP_ALIVE_CONFIGS: [(&str, &str, bool); 3] = [
    ("absent", "", true),
    (
        "true",
        "oauth:\n  providers:\n    codex:\n      chatgpt-keep-alive: true\n",
        true,
    ),
    (
        "false",
        "oauth:\n  providers:\n    codex:\n      chatgpt-keep-alive: false\n",
        false,
    ),
];

/// One ordinary native streaming request through [`CodexExecutor::execute`], the config
/// path every Codex HTTP turn takes (`View::for_request`), read to its end.
async fn ordinary_turn(executor: &CodexExecutor, up: &AdmissionUpstream, cfg: &Config) {
    let base = format!("https://chatgpt.com:{}/backend-api/codex", up.addr.port());
    let response = executor
        .execute(&oauth_credential(&base), plain_request(true, None), cfg)
        .await;
    if let Ok(ExecResponse {
        body: ResponseBody::Stream(mut stream),
        ..
    }) = response
    {
        while stream.next().await.is_some() {}
    }
}

/// #147: with the setting absent or `true`, sequential ordinary chatgpt.com requests
/// reuse one connection (one TCP accept, one TLS handshake) over HTTP/2 and HTTP/1.1;
/// with explicit `false` each request dials, handshakes and closes its own, as Go does.
/// Every request reaches the upstream exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_codex_requests_follow_the_configured_keep_alive() {
    use std::sync::atomic::Ordering::SeqCst;
    for (name, yaml, reuse) in KEEP_ALIVE_CONFIGS {
        let cfg = Config::parse(yaml).unwrap();
        for (alpn, h2) in [(ADMIT_H2, true), (ADMIT_H1, false)] {
            let case = format!("{name} h2={h2}");
            let up = AdmissionUpstream::start(alpn).await;
            up.h1_keep_alive.store(true, SeqCst);
            let executor = up.executor();
            for _ in 0..3 {
                ordinary_turn(&executor, &up, &cfg).await;
            }
            let dials = if reuse { 1 } else { 3 };
            assert_eq!(
                AdmissionUpstream::count(&up.seen.requests),
                3,
                "{case}: each request once"
            );
            assert_eq!(AdmissionUpstream::count(&up.seen.tcp), dials, "{case}: connections");
            assert_eq!(AdmissionUpstream::count(&up.seen.tls), dials, "{case}: TLS handshakes");
            if !reuse {
                // Opted out: none is left open while the executor lives.
                up.settled().await;
            }
            drop(executor);
            up.settled().await;
        }
    }
}

/// #147 keeps N1 #111: a gated request dials a dedicated connection whatever the
/// setting, never the idle pooled one an ordinary request left, and its gate decides
/// after that dial and before any HTTP byte. The upstream holds each new connection's
/// TLS handshake; the gated request's dial reaching it proves the pooled connection was
/// not used. Revoked during the dial, the request sends no HTTP/2 HEADERS or DATA and no
/// HTTP/1.1 byte and provably sent nothing; dropped (cancelled) during the dial, it sends
/// nothing either; accepted, it is served once. The ordinary connection stays pooled for
/// the next ordinary request when keep-alive is on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gated_codex_requests_dial_their_own_connection_under_every_keep_alive_setting() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Gated {
        Revoked,
        Cancelled,
        Accepted,
    }
    for (name, yaml, reuse) in KEEP_ALIVE_CONFIGS {
        let cfg = Config::parse(yaml).unwrap();
        for (alpn, h2) in [(ADMIT_H2, true), (ADMIT_H1, false)] {
            for gated in [Gated::Revoked, Gated::Cancelled, Gated::Accepted] {
                let case = format!("{name} h2={h2} {gated:?}");
                let up = AdmissionUpstream::start(alpn).await;
                up.h1_keep_alive.store(true, SeqCst);
                let executor = Arc::new(up.executor());
                ordinary_turn(&executor, &up, &cfg).await;
                let (headers, data, http1) = (
                    AdmissionUpstream::count(&up.seen.headers),
                    AdmissionUpstream::count(&up.seen.data),
                    AdmissionUpstream::count(&up.seen.http1),
                );
                if !reuse {
                    // The opted-out warm-up connection closes first, so the next ended
                    // connection below is the gated one.
                    up.settled().await;
                }
                let ended_before = *up.ended.borrow();
                up.hold_tls.store(true, SeqCst);
                let (revoked, checks) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicUsize::new(0)));
                let send = tokio::spawn({
                    let (executor, url, gate, cfg) = (
                        executor.clone(),
                        up.url(),
                        admission_gate(&revoked, &checks),
                        cfg.clone(),
                    );
                    async move {
                        let credential = oauth_credential("https://chatgpt.com/backend-api/codex");
                        let view = crate::providers::codex_request::View::for_request(&credential, &cfg);
                        executor
                            .post_certain(
                                &view,
                                &url,
                                &admission_headers(),
                                Bytes::from_static(ADMISSION_BODY.as_bytes()),
                                Some(gate),
                            )
                            .await
                            .map(|upstream| upstream.status)
                            .map_err(|(error, unsent)| (error.status, unsent))
                    }
                });
                tokio::time::timeout(std::time::Duration::from_secs(5), up.tcp_accepted.notified())
                    .await
                    .unwrap_or_else(|_| panic!("{case}: the gated request dials its own connection"));
                match gated {
                    Gated::Revoked => revoked.store(true, SeqCst),
                    Gated::Cancelled => send.abort(),
                    Gated::Accepted => {}
                }
                up.hold_tls.store(false, SeqCst);
                up.release_tls.notify_one();
                let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), send)
                    .await
                    .unwrap_or_else(|_| panic!("{case}: the gated request ends"));
                match gated {
                    Gated::Revoked => {
                        let (_, unsent) = outcome.unwrap().expect_err("refused");
                        assert!(unsent, "{case}: a refusal provably sent nothing");
                    }
                    Gated::Cancelled => assert!(outcome.unwrap_err().is_cancelled(), "{case}"),
                    Gated::Accepted => assert_eq!(outcome.unwrap(), Ok(200), "{case}"),
                }
                if gated != Gated::Accepted {
                    // The gated connection is gone before the counts are read: every
                    // byte the client wrote on it was seen.
                    let mut ended = up.ended.subscribe();
                    tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        ended.wait_for(|n| *n > ended_before),
                    )
                    .await
                    .unwrap_or_else(|_| panic!("{case}: the gated connection ends"))
                    .unwrap();
                    assert_eq!(
                        AdmissionUpstream::count(&up.seen.headers),
                        headers,
                        "{case}: no HEADERS"
                    );
                    assert_eq!(AdmissionUpstream::count(&up.seen.data), data, "{case}: no DATA");
                    assert_eq!(
                        AdmissionUpstream::count(&up.seen.http1),
                        http1,
                        "{case}: no HTTP/1.1 byte"
                    );
                    assert_eq!(AdmissionUpstream::count(&up.seen.requests), 1, "{case}");
                } else {
                    assert_eq!(AdmissionUpstream::count(&up.seen.requests), 2, "{case}");
                }
                assert_eq!(AdmissionUpstream::count(&up.seen.tcp), 2, "{case}: one dedicated dial");
                // The next ordinary request takes the pooled connection when keep-alive is
                // on, and dials when it is off.
                ordinary_turn(&executor, &up, &cfg).await;
                let dials = if reuse { 2 } else { 3 };
                assert_eq!(
                    AdmissionUpstream::count(&up.seen.tcp),
                    dials,
                    "{case}: ordinary connections"
                );
                drop(executor);
                up.settled().await;
            }
        }
    }
}

/// #147 adds no retry: a pooled connection that takes a request in full and then dies
/// without answering fails that request, over HTTP/2 and HTTP/1.1. The upstream read it
/// exactly once, no other connection was dialled for it, and the failure is not reported
/// as provably unsent, so no caller may fail it over. The next request dials afresh.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pooled_connection_lost_after_the_request_is_not_retried() {
    use std::sync::atomic::Ordering::SeqCst;
    for (alpn, h2) in [(ADMIT_H2, true), (ADMIT_H1, false)] {
        let case = format!("h2={h2}");
        let up = AdmissionUpstream::start(alpn).await;
        up.h1_keep_alive.store(true, SeqCst);
        let executor = up.executor();
        let cfg = Config::default();
        let credential = oauth_credential("https://chatgpt.com/backend-api/codex");
        let view = crate::providers::codex_request::View::for_request(&credential, &cfg);
        assert!(view.chatgpt_keep_alive, "the default reuses connections");
        let (url, headers) = (up.url(), admission_headers());
        let post = || {
            executor.post_certain(
                &view,
                &url,
                &headers,
                Bytes::from_static(ADMISSION_BODY.as_bytes()),
                None,
            )
        };
        // Each body is read to its end and dropped: a live HTTP/2 stream keeps its
        // connection open.
        let drain = |upstream: crate::providers::proxy::Upstream| async move {
            let mut body = upstream.body;
            while body.next().await.is_some() {}
            upstream.status
        };
        let first = post().await.map_err(|(error, _)| error.status).unwrap();
        assert_eq!(drain(first).await, 200, "{case}");
        up.drop_next.store(true, SeqCst);
        let failed = match post().await {
            Err((_, unsent)) => unsent,
            Ok(upstream) => {
                // The head may already have arrived on HTTP/2 when the connection goes:
                // then the body fails instead.
                let mut body = upstream.body;
                let mut broke = false;
                while let Some(chunk) = body.next().await {
                    broke |= chunk.is_err();
                }
                assert!(broke, "{case}: the lost request fails");
                false
            }
        };
        assert!(!failed, "{case}: a request the upstream read is not provably unsent");
        assert_eq!(
            AdmissionUpstream::count(&up.seen.requests),
            2,
            "{case}: read once, never resent"
        );
        assert_eq!(
            AdmissionUpstream::count(&up.seen.tcp),
            1,
            "{case}: no dial to resend it"
        );
        let third = post().await.map_err(|(error, _)| error.status).unwrap();
        assert_eq!(drain(third).await, 200, "{case}");
        assert_eq!(AdmissionUpstream::count(&up.seen.tcp), 2, "{case}: a fresh connection");
        drop(executor);
        up.settled().await;
    }
}

/// #147: connections the default keeps are retired when a reload sets
/// `chatgpt-keep-alive: false`. The next request runs on its own connection and the
/// pooled one closes at once, while the executor lives, over HTTP/2 and HTTP/1.1.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_opt_out_reload_retires_the_default_pool() {
    use std::sync::atomic::Ordering::SeqCst;
    let on = Config::default();
    let off = Config::parse(KEEP_ALIVE_CONFIGS[2].1).unwrap();
    for (alpn, h2) in [(ADMIT_H2, true), (ADMIT_H1, false)] {
        let case = format!("h2={h2}");
        let up = AdmissionUpstream::start(alpn).await;
        up.h1_keep_alive.store(true, SeqCst);
        let executor = up.executor();
        ordinary_turn(&executor, &up, &on).await;
        ordinary_turn(&executor, &up, &on).await;
        assert_eq!(AdmissionUpstream::count(&up.seen.tcp), 1, "{case}: pooled");
        ordinary_turn(&executor, &up, &off).await;
        assert_eq!(AdmissionUpstream::count(&up.seen.tcp), 2, "{case}: opted out");
        // Both connections close with the executor still alive: the retired pool's and
        // the opted-out request's own.
        up.settled().await;
        ordinary_turn(&executor, &up, &on).await;
        assert_eq!(AdmissionUpstream::count(&up.seen.tcp), 3, "{case}: a new pool");
        // Sol review: a gated request under the opt-out retires the pool too, though it
        // never uses it.
        let credential = oauth_credential("https://chatgpt.com/backend-api/codex");
        let view = crate::providers::codex_request::View::for_request(&credential, &off);
        let gated = executor
            .post_certain(
                &view,
                &up.url(),
                &admission_headers(),
                Bytes::from_static(ADMISSION_BODY.as_bytes()),
                Some(Arc::new(|| true)),
            )
            .await
            .map_err(|(error, _)| error.status)
            .unwrap();
        let mut body = gated.body;
        while body.next().await.is_some() {}
        drop(body);
        assert_eq!(
            AdmissionUpstream::count(&up.seen.tcp),
            4,
            "{case}: the gated request's own dial"
        );
        up.settled().await;
        drop(executor);
    }
}
