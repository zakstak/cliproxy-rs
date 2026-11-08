//! Executor parity: each case in `tests/fixtures/codex_go.json` ran through Go's
//! `CodexExecutor` against a scripted loopback upstream. The Rust executor gets the same
//! request and upstream reply; the upstream request and the client-visible output must match.

use super::*;
use crate::codex_testkit::{Captured, GO, Mock, Reply, Tap, Wiretap};
use cpa_core::exec::Caller;
use cpa_core::gostr::GoStr;
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
        source: cpa_core::credential::Source::File("/fake/codex-fixture.json".into()),
        disabled: false,
        label: "codex".into(),
        attributes: Default::default(),
        metadata: Default::default(),
        revision: 0,
        next_refresh_after: None,
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
        && let Ok(frame) = cpa_common::codex_body::CodexBody::parse(&body)
        && frame.get("type").is_none()
    {
        body = Bytes::from(frame.with_member("type", r#""response.create""#));
    }
    // Go's executor-side fallback (EnsureSessionContext -> CanonicalSessionID) for a
    // request without conductor metadata, which is how the fixtures were generated: the
    // canonical session from headers and body, with no derived identity.
    let session = Some(cpa_common::session::extract_session_id(
        &headers,
        &body,
        &Default::default(),
    ))
    .filter(|s| !s.is_empty())
    .map(|s| cpa_common::session::bound_session_identity(&s));
    ExecRequest {
        operation: Operation::Generate,
        source_format: source,
        response_format: response,
        stream_payloads: false,
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

fn uuid_shaped(s: &str) -> bool {
    uuid::Uuid::parse_str(s).is_ok()
}

/// The proxy's own identity, spelled out here rather than taken from the implementation.
const PROXY_UA: &str = concat!("cliproxy-rs/", env!("CARGO_PKG_VERSION"));

/// Go's recorded upstream request with the identity #26 sends instead of Go's fixed
/// `codex-tui` one (docs/DIFFERENCES-FROM-GO.md): on a native Responses exchange the
/// caller's own `User-Agent` and `originator`, otherwise cliproxy-rs's; an OAuth
/// credential always carries an originator, an API key only the caller's. Every other
/// header still has to match Go's recording.
fn under_identity_policy(go: &Value, client: &HeaderMap, native: bool, oauth: bool) -> Value {
    let mut go = go.clone();
    let caller = |name: &str| {
        client
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| native && !v.is_empty())
            .map(str::to_owned)
    };
    let body = go["body"].as_str().unwrap_or_default().to_owned();
    let headers = go["headers"].as_object_mut().unwrap();
    let go_ua = headers["user-agent"][0].as_str().unwrap_or_default().to_owned();
    let ua = caller("user-agent").unwrap_or_else(|| PROXY_UA.into());
    // Go adds a random `session_id` when a model override leaves a "Mac OS" User-Agent
    // (`applyModelHeaderOverrides`); the catalog's codex-tui override is gone, so a
    // session that only that rule invented is gone too.
    let random_session = headers
        .get("session_id")
        .and_then(|v| v[0].as_str())
        .is_some_and(|id| !body.contains(id));
    if go_ua.contains("Mac OS") && !ua.contains("Mac OS") && random_session {
        headers.remove("session_id");
    }
    headers.insert("user-agent".into(), serde_json::json!([ua]));
    match caller("originator").or_else(|| oauth.then(|| "cliproxy-rs".into())) {
        Some(originator) => {
            headers.insert("originator".into(), serde_json::json!([originator]));
        }
        None => {
            headers.remove("originator");
        }
    }
    go
}

/// Same path, body bytes and header set. Generated session UUIDs only need the same shape.
fn assert_same_upstream(name: &str, rust: &Captured, go: &Value) {
    assert_eq!(rust.path, go["path"].as_str().unwrap(), "{name}: path");
    assert_eq!(
        String::from_utf8_lossy(&rust.body),
        go["body"].as_str().unwrap(),
        "{name}: upstream body"
    );
    let skip = |n: &str| matches!(n, "host" | "content-length");
    let go_headers = go["headers"].as_object().unwrap();
    for (name_, values) in go_headers {
        if skip(name_) {
            continue;
        }
        let expected = values[0].as_str().unwrap();
        let actual = rust.header(name_);
        if name_ == "referer" {
            // The previous hop's URL, on each run's own mock port.
            let path = |u: &str| url::Url::parse(u).map(|u| u.path().to_owned()).unwrap_or_default();
            assert_eq!(path(actual), path(expected), "{name}: header {name_}");
            continue;
        }
        if name_ == "session_id"
            && go_headers.contains_key("session_id")
            && !go["body"].as_str().unwrap().contains(expected)
        {
            assert!(
                uuid_shaped(actual),
                "{name}: {name_} should be a fresh UUID, got {actual:?}"
            );
            continue;
        }
        assert_eq!(actual, expected, "{name}: header {name_}");
    }
    for name_ in rust.headers.keys() {
        assert!(
            skip(name_.as_str()) || go_headers.contains_key(name_.as_str()),
            "{name}: extra header {name_}"
        );
    }
}

fn assert_same_error(name: &str, rust: &ExecError, go: &Value) {
    assert_eq!(rust.status, go["status"].as_u64().unwrap() as u16, "{name}: status");
    assert_eq!(
        String::from_utf8_lossy(&rust.body),
        go["message"].as_str().unwrap(),
        "{name}: message"
    );
    assert_eq!(
        rust.retry_after.map(|d| d.as_secs_f64()),
        go["retry_after_s"].as_f64(),
        "{name}: retry_after"
    );
    if go["credential_scoped"] == true {
        assert_eq!(rust.scope, FailureScope::Credential, "{name}: credential scoped");
    } else {
        assert_ne!(
            (rust.status, rust.scope),
            (429, FailureScope::Credential),
            "{name}: only usage limits cool the whole credential"
        );
    }
    if go["request_scoped"] == true {
        assert_eq!(rust.scope, FailureScope::Request, "{name}: request scoped");
    }
}

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
        req.resolved_model = Some(cpa_core::exec::ResolvedModel {
            info: cpa_core::registry::ModelInfo::from_raw(raw.as_object().unwrap().clone()).unwrap(),
            source: cpa_core::exec::ResolvedSource::ApiKey,
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
        }) => {
            while let Some(item) = stream.next().await {
                items.push(item);
            }
            Ok(ExecResponse {
                status,
                headers,
                body: ResponseBody::Buffered(Bytes::new()),
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

/// Go's image-tool request without the defaults #27 removes (`instructions`,
/// `reasoning`, `parallel_tool_calls`, `include`), and the Rust body, both as JSON
/// (docs/DIFFERENCES-FROM-GO.md, "Codex translated route").
fn image_tool_bodies(go: &str, rust: &[u8]) -> (Value, Value) {
    let mut go: Value = serde_json::from_str(go).unwrap();
    for key in ["instructions", "reasoning", "parallel_tool_calls", "include"] {
        go.as_object_mut().unwrap().remove(key);
    }
    (go, serde_json::from_slice(rust).unwrap())
}

/// A recorded case the translated route serves (#27): a Claude or Chat Completions
/// request on a native profile.
fn translated_case(case: &Value, base_url: &str) -> bool {
    case["resolved_compat"] != true
        && case["exec_metadata"]["execution_session_id"].as_str().is_none()
        && super::translated::route(&View::new(&credential(case, base_url)), &request(case))
}

/// The translated contract for a recorded case (docs/DIFFERENCES-FROM-GO.md, "Codex
/// translated route"): the upstream body is the built-in strict conversion with only the
/// routed model and `stream`, so Go's injected `instructions`, reasoning default,
/// prompt-cache key, replayed reasoning, tools and payload rules are absent; a refused
/// conversion sends nothing.
fn assert_translated_contract(
    name: &str,
    case: &Value,
    result: &Result<ExecResponse, ExecError>,
    captured: &[Captured],
) {
    let mut req = request(case);
    if case["resolved_compat"] == true {
        // As `run` binds it: the conversion reads the model's thinking capabilities.
        let raw = serde_json::json!({"id": req.model, "is_compat": true});
        req.resolved_model = Some(cpa_core::exec::ResolvedModel {
            info: cpa_core::registry::ModelInfo::from_raw(raw.as_object().unwrap().clone()).unwrap(),
            source: cpa_core::exec::ResolvedSource::ApiKey,
        });
    }
    let model = super::request::base_model(&req.model);
    match super::translated::upstream_body(&req, &model) {
        Err(expected) => {
            let error = result
                .as_ref()
                .err()
                .unwrap_or_else(|| panic!("{name}: the conversion is refused"));
            assert_eq!(error.status, expected.status, "{name}");
            assert!(captured.is_empty(), "{name}: nothing is sent");
        }
        Ok(expected) => {
            let sent = captured
                .first()
                .unwrap_or_else(|| panic!("{name}: an upstream request"));
            let body = String::from_utf8_lossy(&sent.body);
            assert_eq!(body, expected, "{name}: translated upstream body");
            let source = String::from_utf8_lossy(&req.body);
            assert!(!body.contains("cpa_codex_replay_turn"), "{name}: no replay marker");
            assert!(!body.contains(r#""instructions":"""#), "{name}: no empty instructions");
            if !source.contains("prompt_cache_key") {
                assert!(!body.contains("prompt_cache_key"), "{name}: no synthesized cache key");
            }
            if !source.contains("image_generation") {
                assert!(!body.contains("image_generation"), "{name}: no injected image tool");
            }
        }
    }
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
    if cpa_common::thinking::parse_suffix(&req.model).has_suffix {
        let error = result
            .as_ref()
            .err()
            .unwrap_or_else(|| panic!("{name}: a suffix is refused"));
        assert_eq!(error.status, 400, "{name}");
        assert!(captured.is_empty(), "{name}: nothing is sent");
        return;
    }
    let ws_session = case["exec_metadata"]["execution_session_id"].as_str().is_some();
    use cpa_common::codex_body::{CodexBody, Kind};
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
async fn executor_matches_go_on_every_fixture_case() {
    let executor = executor();
    for case in GO["executor"].as_array().unwrap() {
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
                (500, cpa_core::exec::FailureScope::Credential),
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
        assert_eq!(error.scope, cpa_core::exec::FailureScope::Request, "{name}");
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
    let executor = CodexExecutor::with_transport(crate::codex_tls::Transport::new(Default::default()), oauth);
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
    use crate::codex_testkit::json;
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
    mock.script("/oauth/token", vec![json(400, r#"{"error":"invalid_grant"}"#); 3]);
    let error = executor.prepare(&expired, &cfg).await.unwrap_err();
    assert_eq!((error.status, error.scope), (401, FailureScope::Credential));

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
fn alpha_search_body_rules_match_go_marshal() {
    for case in GO["alpha_search"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap().as_bytes();
        let sanitized = sanitize_alpha_search(input);
        assert_eq!(
            String::from_utf8_lossy(&sanitized),
            case["sanitized"].as_str().unwrap(),
            "{case}"
        );
        let rewritten = rewrite_alpha_search_model(sanitized, "real-model");
        assert_eq!(
            String::from_utf8_lossy(&rewritten),
            case["rewritten"].as_str().unwrap(),
            "{case}"
        );
    }
}

#[test]
fn quota_events_and_signals_match_go() {
    for case in GO["quota"]["events"].as_array().unwrap() {
        let parsed = crate::codex_quota::event_headers(case["payload"].as_str().unwrap());
        let expected: Vec<(String, String)> = case["headers"]
            .as_object()
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.to_ascii_lowercase(), v[0].as_str().unwrap().to_owned()))
                    .collect()
            })
            .unwrap_or_default();
        let mut actual: Vec<(String, String)> = parsed
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v))
            .collect();
        actual.sort();
        let mut expected = expected;
        expected.sort();
        assert_eq!(actual, expected, "{}", case["payload"]);
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
    let signals: serde_json::Map<String, Value> = crate::codex_quota::collect_signals(&headers)
        .into_iter()
        .map(|(k, v)| (k, Value::String(v)))
        .collect();
    assert_eq!(Value::Object(signals), GO["quota"]["signals"]);
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
        stream_payloads: false,
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
async fn compact_failures_follow_gos_availability_neutral_policy() {
    // Go: compact 404/405/501 and request faults stop the request without cooling;
    // other failures fail over without cooling; auth and quota keep the normal policy.
    use crate::codex_testkit::json;
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
    assert_eq!(error.scope, FailureScope::Transport, "{error}");
    assert_ne!(error.status, 401);
}

#[tokio::test]
async fn alpha_search_reads_at_most_32_mib_whatever_the_status() {
    use crate::codex_testkit::Reply;
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

impl cpa_core::exec::UsageObserver for Recorded {
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
/// whatever the client speaks. Expected payloads come from the Go fixture.
#[tokio::test]
async fn usage_records_see_upstream_payloads() {
    let executor = executor();
    for name in [
        "apikey_claude_not_compat",
        "oauth_nonstream_no_instructions_free_plan",
        "oauth_compact",
    ] {
        let case = GO["executor"]
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
        req.usage = cpa_core::exec::UsageSink::new(recorded.clone());
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
        // edits (`executor_matches_go_on_every_fixture_case` checks those bytes).
        let sent = if native_case(case, &mock.url) || translated_case(case, &mock.url) {
            String::from_utf8(mock.take().pop().unwrap().body.to_vec()).unwrap()
        } else {
            case["upstream"]["body"].as_str().unwrap().to_owned()
        };
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
        let case = GO["executor"]
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
        req.usage = cpa_core::exec::UsageSink::new(recorded.clone());
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
        assert_eq!(gjson::get(&sent, "reasoning.effort").str(), effort, "{name}: upstream");
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
                vec![crate::codex_testkit::json(
                    200,
                    r#"{"created":1713833628,"data":[{"b64_json":"AA=="}]}"#,
                )],
            );
            let cfg = Config::parse(&format!(
                "codex-api-key:\n  - api-key: codex-token\n    base-url: {}\n    prefix: tenant\n    models:\n      - name: {configured}\n        alias: gpt-image-2\n{rules}",
                mock.url
            ))
            .unwrap();
            let credential = cpa_core::config::credentials::from_config(&cfg)
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
                stream_payloads: false,
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

/// The Images API cases (`GO["images"]`): Go's `CodexExecutor` ran each with source
/// format `openai-image`, the route in `request_path` metadata and the client headers in
/// the gin context, as the images handler calls it.
#[tokio::test]
async fn images_match_go_on_every_fixture_case() {
    use base64::Engine;
    let executor = executor();
    for case in GO["images"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let mock = Mock::start().await;
        let path = case["upstream"]["path"].as_str().unwrap_or("/responses");
        mock.script(path, vec![reply(case)]);
        let credential = credential(case, &mock.url);
        let config_text = case["config"].as_str().filter(|s| !s.trim().is_empty()).unwrap_or("{}");
        let cfg = Config::parse(config_text).unwrap();
        let mut headers = HeaderMap::new();
        for (k, v) in case["headers"].as_object().into_iter().flatten() {
            headers.insert(
                http::HeaderName::try_from(k.as_str()).unwrap(),
                v.as_str().unwrap().parse().unwrap(),
            );
        }
        let body = Bytes::from(
            base64::engine::general_purpose::STANDARD
                .decode(case["payload_b64"].as_str().unwrap())
                .unwrap(),
        );
        let request_path = case["request_path"].as_str().unwrap();
        let model = case["model"].as_str().unwrap();
        let client = headers.clone();
        let req = ExecRequest {
            operation: Operation::Generate,
            source_format: Format::OpenAI,
            response_format: Format::OpenAI,
            stream_payloads: false,
            requested_model: model.into(),
            model: model.into(),
            original_body: body.clone(),
            body,
            stream: case["stream"].as_bool().unwrap_or(false),
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
        let result = executor.images(&credential, req, request_path, &cfg).await;
        let captured = mock.take();
        if matches!(name, "tool_generation_nonstream_b64" | "tool_edit_json") {
            // `n: 2` and `style`, or an image entry with a blank `image_url`, cannot be
            // expressed by the image tool: refused before the send (#27). Go drops them
            // (and the edit's file-ID image too).
            let error = result.err().unwrap_or_else(|| panic!("{name}: refused"));
            assert_eq!(error.status, 400, "{name}");
            assert!(captured.is_empty(), "{name}: nothing is sent");
            continue;
        }
        match case.get("upstream").filter(|u| !u.is_null()) {
            None => assert!(captured.is_empty(), "{name}: no upstream request"),
            Some(go) => {
                assert_eq!(captured.len(), 1, "{name}: one upstream request");
                let mut go = go.clone();
                if name == "direct_edit_multipart" {
                    // Go ranges over its form map: field order varies per run.
                    let go_body: Value = serde_json::from_str(go["body"].as_str().unwrap()).unwrap();
                    let rust_body: Value = serde_json::from_slice(&captured[0].body).unwrap();
                    assert_eq!(rust_body, go_body, "{name}: upstream body fields");
                    go["body"] = Value::String(String::from_utf8_lossy(&captured[0].body).into_owned());
                }
                // The proxy builds every Images request: never the caller's identity.
                let oauth = !View::new(&credential).api_key;
                let mut go = under_identity_policy(&go, &client, false, oauth);
                if path == "/responses" {
                    let (go_body, rust_body) = image_tool_bodies(go["body"].as_str().unwrap(), &captured[0].body);
                    assert_eq!(rust_body, go_body, "{name}: the tool request without Go's defaults");
                    go["body"] = Value::String(String::from_utf8_lossy(&captured[0].body).into_owned());
                }
                assert_same_upstream(name, &captured[0], &go);
            }
        }
        let output = &case["output"];
        if let Some(go_error) = output.get("error") {
            let error = result.err().unwrap_or_else(|| panic!("{name}: expected error"));
            match go_error.get("status") {
                Some(_) => assert_same_error(name, &error, go_error),
                // A plain Go error: the handler answers 500.
                None => {
                    assert_eq!(error.status, 500, "{name}: status");
                    assert_eq!(
                        String::from_utf8_lossy(&error.body),
                        go_error["message"].as_str().unwrap(),
                        "{name}: message"
                    );
                }
            }
            continue;
        }
        let response = result.unwrap_or_else(|e| panic!("{name}: {e}"));
        match response.body {
            ResponseBody::Buffered(body) => {
                assert_eq!(
                    String::from_utf8_lossy(&body),
                    output["payload"]
                        .as_str()
                        .unwrap_or_else(|| panic!("{name}: Go streamed")),
                    "{name}: payload"
                );
            }
            ResponseBody::Stream(mut stream) => {
                let mut items = Vec::new();
                while let Some(item) = stream.next().await {
                    items.push(item);
                }
                let joined: String = items
                    .iter()
                    .filter_map(|i| i.as_ref().ok())
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .collect();
                let go_joined: String = output["chunks"]
                    .as_array()
                    .unwrap_or_else(|| panic!("{name}: Go answered without a stream"))
                    .iter()
                    .map(|c| c.as_str().unwrap())
                    .collect();
                assert_eq!(joined, go_joined, "{name}: stream bytes");
                match (items.last(), output.get("stream_error")) {
                    (Some(Err(error)), Some(go)) => assert_same_error(name, error, go),
                    (_, Some(_)) => panic!("{name}: expected a terminal stream error"),
                    (Some(Err(error)), None) => panic!("{name}: unexpected stream error {error}"),
                    _ => {}
                }
            }
        }
    }
}

/// Go `Auth.AccountInfo` for the fixture credentials.
fn go_account(credential: &Credential) -> [String; 5] {
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
async fn capture_records_each_attempt_as_go_does() {
    let executor = executor();
    for case in GO["executor"].as_array().unwrap() {
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
        req.usage = cpa_core::exec::UsageSink::default().with_capture(cpa_core::exec::CaptureSink::new(tap.clone()));
        let ws_session = case["exec_metadata"]["execution_session_id"].as_str();
        let result = match ws_session {
            Some(session) => ws_http(&executor, &credential, &cfg, req, session).await,
            None => executor.execute(&credential, req, &cfg).await,
        };
        if let Ok(ExecResponse {
            body: ResponseBody::Stream(mut stream),
            ..
        }) = result
        {
            while stream.next().await.is_some() {}
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
        assert_eq!(*account, go_account(&credential), "{name}: account");
        let mut recorded: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.go_lower(), v.clone())).collect();
        recorded.sort();
        let mut wire: Vec<(String, String)> = sent
            .headers
            .iter()
            .filter(|(k, _)| !matches!(k.as_str(), "host" | "content-length" | "accept-encoding"))
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_owned()))
            .collect();
        wire.sort();
        assert_eq!(recorded, wire, "{name}: request headers");
        assert!(
            headers.iter().all(|(k, _)| *k == crate::proxy::canonical_header(k)),
            "{name}: Go header spelling {headers:?}"
        );
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
        match case["output"].get("stream_error") {
            Some(go) => assert_eq!(errors, [go["message"].as_str().unwrap()], "{name}: stream error"),
            None if case["output"].get("error").is_some() => {
                assert_eq!(errors.len(), 1, "{name}: the bootstrap failure: {errors:?}")
            }
            None => {
                assert_eq!(chunks.len(), lines.len(), "{name}: every line");
                assert!(errors.is_empty(), "{name}: {errors:?}");
            }
        }
    }
}

/// Go's transport failure path: the request, then `RecordAPIResponseError`.
#[tokio::test]
async fn capture_records_a_transport_failure() {
    let tap = Arc::new(Wiretap::default());
    let mut req = plain_request(true, None);
    req.usage = cpa_core::exec::UsageSink::default().with_capture(cpa_core::exec::CaptureSink::new(tap.clone()));
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
    req.usage = cpa_core::exec::UsageSink::default().with_capture(cpa_core::exec::CaptureSink::new(tap.clone()));
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
            &cpa_core::exec::CaptureSink::new(tap.clone()),
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
    use crate::codex_response::{Bootstrap, Processor, bootstrap, processed};
    let processor = Processor::new(false, false);
    if !stream {
        let mut processor = processor;
        let completed = processor.buffered(sse.as_bytes())?.expect("a terminal event");
        return Ok((vec![gjson::get(&completed, "response").json().to_owned()], None));
    }
    let upstream = crate::codex::events(ResponseBody::Stream(crate::upstream::framed(
        futures_util::stream::iter([Ok::<_, ExecError>(Bytes::from(sse.to_owned()))]).boxed(),
    )));
    let mut body = if buffering {
        match bootstrap(upstream, processor, None, Instant::now()).await {
            Bootstrap::Reject(error) => return Err(error),
            Bootstrap::Stream(stream) => stream,
        }
    } else {
        processed(upstream, processor, 0)
    };
    let (mut frames, mut failure) = (Vec::new(), None);
    while let Some(item) = body.next().await {
        match item {
            Ok(frame) => frames.push(String::from_utf8(frame.to_vec()).unwrap()),
            Err(error) => failure = Some(error),
        }
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
    // fill it exactly (outcomes recorded from Go v8.0.35): 47 comments and the overload's
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

/// Go `TestCodexExecutorReplayedWebSearchCallHistoryEnsuresWebSearchTool_Issue6770`
/// (e7d4c731) through the executor. Go added a web search tool (and the image tool and
/// an `instructions` default) to the body; every call now takes the native route, so the
/// client's body goes upstream unchanged on HTTP and WebSocket alike (#23, #24, #28).
#[tokio::test]
async fn replayed_web_search_history_matches_go() {
    let go: Value = serde_json::from_str(include_str!("../tests/fixtures/codex_web_search_history_go.json")).unwrap();
    let executor = executor();
    for case in go["executor"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let call = case["call"].as_str().unwrap();
        let mut attributes = serde_json::json!({"api_key": "test"});
        if let Some(plan) = case["plan"].as_str() {
            attributes["plan_type"] = plan.into();
        }
        let mut headers = serde_json::json!({});
        if case["lite_header"] == true {
            headers["x-openai-internal-codex-responses-lite"] = "true".into();
        }
        let synthetic = serde_json::json!({
            "source": "openai-response",
            "model": "gpt-6.1-sol",
            "payload": case["payload"],
            "stream": call != "non_stream" && call != "compact",
            "alt": if call == "compact" { Value::from("responses/compact") } else { Value::Null },
            "headers": headers,
            "attributes": attributes,
        });
        let cfg = Config::parse(if case["no_image"] == true {
            "disable-image-generation: true\n"
        } else {
            "{}"
        })
        .unwrap();
        let sent = if call == "websocket" {
            let req = request(&synthetic);
            let model = super::native::upstream_model(&req).unwrap();
            String::from_utf8(super::native::routed(&req.body, &model).unwrap().to_vec()).unwrap()
        } else {
            let mock = Mock::start().await;
            let (path, content_type, body) = if call == "compact" {
                (
                    "/responses/compact",
                    "application/json",
                    r#"{"id":"resp_compact_1","object":"response.compaction","usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12}}"#,
                )
            } else {
                (
                    "/responses",
                    "text/event-stream",
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"total_tokens\":15}}}\n\n",
                )
            };
            mock.script(
                path,
                vec![Reply {
                    status: 200,
                    headers: vec![("content-type".into(), content_type.into())],
                    body: body.into(),
                }],
            );
            let credential = credential(&synthetic, &mock.url);
            if let ResponseBody::Stream(mut stream) = executor
                .execute(&credential, request(&synthetic), &cfg)
                .await
                .unwrap()
                .body
            {
                while stream.next().await.is_some() {}
            }
            String::from_utf8(mock.take().pop().unwrap().body.to_vec()).unwrap()
        };
        // Without the web search or image tool Go added.
        assert_eq!(sent, case["payload"].as_str().unwrap(), "{name}");
    }
}

/// Sign in with ChatGPT (7b0319fb): Go v8.0.35's upstream headers for a `use_siwc`
/// credential on every call, and its missing-token error; see tests/reference/codex_siwc.
/// The bodies follow the native contract instead (#28, docs/DIFFERENCES-FROM-GO.md): the
/// `siwc` profile takes the native route, so the client's body goes upstream with only
/// its `model` routed, and Go's Sign in with ChatGPT body rules no longer run.
#[tokio::test]
async fn siwc_requests_match_go() {
    let go: Value = serde_json::from_str(include_str!("../tests/fixtures/codex_siwc_go.json")).unwrap();
    let executor = executor();
    for case in go["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let call = case["call"].as_str().unwrap();
        let mut metadata = case["metadata"].clone();
        metadata["type"] = "codex".into();
        let synthetic = serde_json::json!({
            "source": "openai-response",
            "model": case["model"],
            "payload": case["payload"],
            "stream": call == "stream" || call == "websocket",
            "alt": if call == "compact" { Value::from("responses/compact") } else { Value::Null },
            "headers": case["headers"],
            "attributes": case["attributes"],
            "metadata": metadata,
        });
        let cfg = Config::parse(case["config"].as_str().unwrap_or("{}")).unwrap();
        let mock = Mock::start().await;
        let credential = credential(&synthetic, &mock.url);
        // Go sends `<signin-with-chatgpt-identifier> (<GOOS>; <GOARCH>)`, an
        // operator-chosen identity. #26 (docs/DIFFERENCES-FROM-GO.md): a native caller's
        // own User-Agent, else cliproxy-rs's. The image call is translated, so the
        // proxy's. Every other recorded header still has to match.
        let mut sent_headers = case["sent_headers"].clone();
        if let Some(ua) = sent_headers.get_mut("user-agent") {
            assert!(
                ua[0].as_str().unwrap().contains(" (linux; amd64)"),
                "{name}: Go's SIWC identity"
            );
            let caller = case["headers"]
                .as_object()
                .and_then(|h| h.iter().find(|(k, _)| k.eq_ignore_ascii_case("user-agent")))
                .and_then(|(_, v)| v.as_str())
                .map(str::trim)
                .filter(|v| call != "image" && !v.is_empty());
            ua[0] = caller.unwrap_or(PROXY_UA).into();
        }
        // A buffered request keeps the client's `stream: false` on this profile (#28), so
        // it asks for JSON, where Go forced a stream.
        if call == "non_stream"
            && let Some(accept) = sent_headers.get_mut("accept")
        {
            accept[0] = "application/json".into();
        }
        // The native body: the client's, with only the routed model.
        let native_body = {
            let req = request(&synthetic);
            let model = super::native::upstream_model(&req).ok();
            model
                .and_then(|m| super::native::routed(&req.body, &m).ok())
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default()
        };
        let go_upstream = serde_json::json!({
            "path": case["path"],
            "body": if call == "image" { case["body"].clone() } else { Value::String(native_body.clone()) },
            "headers": sent_headers,
        });
        if call == "websocket" {
            let req = request(&synthetic);
            let view = View::new(&credential);
            let settings = Settings::scoped(&cfg, &view);
            let body = native_body.clone();
            let cache = request::prompt_cache_id(&req, &body, Some("exec-session"), true);
            let headers = request::ws_headers(
                &view,
                &settings,
                &req.headers,
                &body,
                &request::base_model(&req.model),
                cache.as_deref(),
                request::is_native(&req),
                crate::codex_identity::Origin::Caller,
            );
            let mut actual: Vec<(String, String)> = headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_owned()))
                .collect();
            actual.sort();
            let mut expected: Vec<(String, String)> = go_upstream["headers"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v[0].as_str().unwrap().to_owned()))
                .collect();
            expected.sort();
            assert_eq!(actual, expected, "{name}: headers");
            continue;
        }
        if call == "image" {
            let sse = concat!(
                "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"image_generation_call\",\"id\":\"ig_0\",\"status\":\"completed\",\"result\":\"SU1HMQ==\",\"output_format\":\"png\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_img\",\"created_at\":1700000000,\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":2,\"total_tokens\":3}}}\n\n",
            );
            mock.script(
                "/responses",
                vec![Reply {
                    status: 200,
                    headers: vec![("content-type".into(), "text/event-stream".into())],
                    body: sse.into(),
                }],
            );
            let path = "/v1/images/generations";
            let mut req = request(&synthetic);
            (req.source_format, req.response_format, req.request_path) = (Format::OpenAI, Format::OpenAI, path.into());
            executor.images(&credential, req, path, &cfg).await.unwrap();
            let sent = mock.take().pop().unwrap();
            let (go_body, rust_body) = image_tool_bodies(go_upstream["body"].as_str().unwrap(), &sent.body);
            assert_eq!(rust_body, go_body, "{name}: the tool request without Go's defaults");
            let mut go_upstream = go_upstream;
            go_upstream["body"] = Value::String(String::from_utf8_lossy(&sent.body).into_owned());
            assert_same_upstream(name, &sent, &go_upstream);
            continue;
        }
        let (path, content_type, reply) = if call == "compact" {
            (
                "/responses/compact",
                "application/json",
                r#"{"id":"resp_compact_1","object":"response.compaction","usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12}}"#,
            )
        } else {
            (
                "/responses",
                "text/event-stream",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"total_tokens\":15}}}\n\n",
            )
        };
        mock.script(
            path,
            vec![Reply {
                status: 200,
                headers: vec![("content-type".into(), content_type.into())],
                body: reply.into(),
            }],
        );
        match executor.execute(&credential, request(&synthetic), &cfg).await {
            Ok(res) => {
                if let ResponseBody::Stream(mut stream) = res.body {
                    while stream.next().await.is_some() {}
                }
                assert_same_upstream(name, &mock.take().pop().unwrap(), &go_upstream);
            }
            // A thinking suffix has no native meaning, so the native route refuses it
            // before any upstream request, where Go turned it into body fields.
            Err(error) if case["status"].is_null() => {
                assert!(
                    cpa_common::thinking::parse_suffix(case["model"].as_str().unwrap()).has_suffix,
                    "{name}: {error:?}"
                );
                assert_eq!(error.status, 400, "{name}");
                assert!(mock.take().is_empty(), "{name}: nothing may reach upstream");
            }
            Err(error) => {
                assert_eq!(error.status as i64, case["status"].as_i64().unwrap(), "{name}: status");
                assert_eq!(
                    String::from_utf8_lossy(&error.body),
                    case["error"].as_str().unwrap(),
                    "{name}: error"
                );
                assert!(mock.take().is_empty(), "{name}: nothing may reach upstream");
            }
        }
    }
    // Without a `base_url` (the cases above dial a mock): Go's `defaultOpenAISIWCBaseURL`.
    let production = Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/c.json"),
        serde_json::json!({"type": "codex", "use_siwc": true, "access_token": "outer-at",
                           "siwc": {"access_token": "siwc-at"}})
        .as_object()
        .unwrap()
        .clone(),
    )
    .unwrap();
    let view = View::new(&production);
    assert_eq!((view.base_url, view.token), ("https://api.openai.com/v1", "siwc-at"));
}

/// Sign in with ChatGPT refresh (Go `refreshLocked`): the `siwc` partition refreshes on
/// its own 5-minute lead, a rejected token refreshes the partition requests use, a
/// management refresh both, and one partition failing keeps the other's result.
#[tokio::test]
async fn siwc_partition_refreshes_like_go() {
    use crate::codex_testkit::json;
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
        String::from_utf8_lossy(&sent[0].body),
        "client_id=client-1&grant_type=refresh_token&refresh_token=siwc-b&resource=https%3A%2F%2Fapi.openai.com%2Fv1"
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
        gjson::get(&text, "response.usage.input_tokens").i64()
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
            crate::codex_identity::Origin::of(&req),
        );
        let hint = headers
            .get("x-codex-routing-hint")
            .map(|v| v.to_str().unwrap().to_owned());
        (gjson::get(&body, "service_tier").json().to_owned(), hint)
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
