use super::*;
use cpa_core::exec::Caller;
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

thread_local! {
    /// The clock requests prepared on this test thread read, when set.
    static CLOCK: std::cell::Cell<Option<chrono::DateTime<chrono::Utc>>> = const { std::cell::Cell::new(None) };
}

/// The instant [`set_clock`] set for this thread.
pub(super) fn clock() -> Option<chrono::DateTime<chrono::Utc>> {
    CLOCK.get()
}

/// Set a fixed request clock until a [`Pins`] guard drops. Each Tokio test runs
/// on its own thread.
fn set_clock(now: Option<&str>) {
    CLOCK.set(now.map(|now| chrono::DateTime::parse_from_rfc3339(now).unwrap().to_utc()));
}

/// Release the fixed clock even when an assertion panics.
pub(super) struct Pins;

impl Drop for Pins {
    fn drop(&mut self) {
        CLOCK.set(None);
    }
}

/// An in-memory OAuth credential containing only local test identities.
fn harness_credential() -> Credential {
    let metadata = serde_json::json!({
        "type": "claude", "access_token": "sk-ant-oat01-FAKE-LOCAL-ONLY",
        "refresh_token": "sk-ant-ort01-FAKE-LOCAL-ONLY", "id_token": "",
        "email": "local-fixture@example.invalid", "account_uuid": "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
        "organization_uuid": "12345678-1234-4234-8234-123456789abc", "organization_name": "Local fixture",
        "claude_device_ids": ["0123456789abcdef".repeat(4)],
        "expired": "2099-01-01T00:00:00Z", "last_refresh": "2026-10-01T00:00:00Z",
    });
    Credential::from_file(
        Path::new("/auth"),
        Path::new("/auth/fixture.json"),
        metadata.as_object().unwrap().clone(),
    )
    .unwrap()
}

fn request(case: &Value) -> ExecRequest {
    let body = Bytes::from(case["request_body"].as_str().unwrap().to_owned());
    let headers: http::HeaderMap = case["request_headers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            (
                h[0].as_str().unwrap().parse().unwrap(),
                h[1].as_str().unwrap().parse().unwrap(),
            )
        })
        .collect();
    let count = case["path"].as_str().unwrap().ends_with("count_tokens");
    ExecRequest {
        operation: if count {
            Operation::CountTokens
        } else {
            Operation::Generate
        },
        source_format: Format::Claude,
        response_format: Format::Claude,
        requested_model: "claude-sonnet-4-6".into(),
        model: "claude-sonnet-4-6".into(),
        original_body: body.clone(),
        stream: rawjson::get(&String::from_utf8_lossy(&body), "stream").bool(),
        body,
        alt: None,
        session: None,
        execution_session: None,
        derived_session: None,
        resolved_model: None,
        usage: Default::default(),
        request_path: String::new(),
        headers,
        caller: Caller {
            principal: "fixture-client-key".into(),
            source: "authorization",
            provider: String::new(),
        },
    }
}

/// The Messages pipeline retains caller content and isolates upstream credentials.
/// First-party identity values have their own measured-client fixture tests.
#[tokio::test]
async fn prepared_messages_preserve_content_tools_and_credential_boundaries() {
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let credential = harness_credential();
    let cfg = Config::parse("").unwrap();
    for stream in [false, true] {
        for tools in [false, true] {
            let mut body = serde_json::json!({
                "model":"claude-sonnet-4-6", "max_tokens":17, "stream":stream,
                "messages":[{"role":"user","content":"Local question"}],
                "system":"Preserve caller instructions", "unknown_extension":{"value":"untouched"}
            });
            if tools {
                body["tools"] = serde_json::json!([
                    {"name":"lookup","description":"Local tool","input_schema":{"type":"object"}},
                    {"name":"mcp__native__lookup","input_schema":{"type":"object"}}
                ]);
                body["tool_choice"] = serde_json::json!({"type":"tool","name":"lookup"});
                body["messages"].as_array_mut().unwrap().extend([
                    serde_json::json!({"role":"assistant","content":[{"type":"tool_use","id":"previous_tool","name":"lookup","input":{"name":"literal"}}]}),
                    serde_json::json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"previous_tool","content":"Local result"}]}),
                ]);
            }
            let case = serde_json::json!({"request_body":body.to_string(),"request_headers":[["authorization","Bearer client-only-secret"]],"path":"/v1/messages"});
            let req = enrich(request(&case));
            let mut ctx = Ctx::new(&executor, &credential, &req, &cfg);
            ctx.today = "2026-10-02".into();
            let translated = translate::request(
                &req,
                ctx.codex,
                &ctx.base_model,
                ctx.is_compat,
                ctx.tool_search(),
                ctx.system_turns(&req, false),
            )
            .unwrap();
            let original = translate::original(
                &req,
                &translated,
                ctx.codex,
                &ctx.base_model,
                ctx.is_compat,
                ctx.tool_search(),
                ctx.system_turns(&req, false),
            )
            .unwrap();
            let prepared = ctx
                .prepare_messages(&req, &translated, &original, stream)
                .await
                .unwrap();
            let sent: Value = serde_json::from_str(&prepared.body).unwrap();
            assert_eq!(sent["model"], body["model"]);
            assert_eq!(sent["max_tokens"], 17);
            assert_eq!(sent["stream"], stream);
            assert_eq!(sent["unknown_extension"], body["unknown_extension"]);
            assert!(prepared.body.contains("Local question"));
            assert!(prepared.body.contains("Preserve caller instructions"));
            assert!(!prepared.body.contains("client-only-secret"));
            let headers: http::HeaderMap = prepared
                .headers
                .iter()
                .map(|(k, v)| {
                    (
                        k.parse::<http::HeaderName>().unwrap(),
                        v.parse::<http::HeaderValue>().unwrap(),
                    )
                })
                .collect();
            assert_eq!(headers["authorization"], "Bearer sk-ant-oat01-FAKE-LOCAL-ONLY");
            assert!(prepared.headers.iter().all(|(_, v)| !v.contains("client-only-secret")));
            assert_eq!(signing::sign(prepared.body.clone()).unwrap(), prepared.body);
            if tools {
                let alias = sent["tools"][0]["name"].as_str().unwrap();
                assert_ne!(alias, "lookup");
                assert_eq!(prepared.reverse.get(alias).map(String::as_str), Some("lookup"));
                assert_eq!(sent["tools"][1]["name"], "mcp__native__lookup");
                assert_eq!(sent["tool_choice"]["name"], alias);
                let assistant = sent["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|m| m["role"] == "assistant")
                    .unwrap();
                assert_eq!(assistant["content"][0]["name"], alias);
                assert_eq!(assistant["content"][0]["input"]["name"], "literal");
                assert!(prepared.body.contains("previous_tool"));
                assert!(prepared.body.contains("Local result"));
            }
        }
    }
}

#[tokio::test]
async fn custom_origin_counts_locally_without_sending_credentials() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    // A first-party default must not override the credential's custom origin.
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let mut credential = Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/claude.json"),
        serde_json::json!({"type":"claude","access_token":"sk-ant-oat-fake"})
            .as_object()
            .unwrap()
            .clone(),
    )
    .unwrap();
    credential.attributes.insert("base_url".into(), base);
    let body = Bytes::from_static(br#"{"model":"claude","messages":[{"role":"user","content":"Hello."}]}"#);
    let req = ExecRequest {
        operation: Operation::CountTokens,
        source_format: Format::Claude,
        response_format: Format::Claude,
        requested_model: "claude".into(),
        model: "claude".into(),
        original_body: body.clone(),
        body,
        stream: false,
        alt: None,
        session: None,
        execution_session: None,
        derived_session: None,
        resolved_model: None,
        usage: Default::default(),
        request_path: String::new(),
        headers: Default::default(),
        caller: Caller {
            principal: "fake-client".into(),
            source: "x-api-key",
            provider: String::new(),
        },
    };
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        executor.execute(&credential, req, &Config::parse("").unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status, 200);
    let ResponseBody::Buffered(body) = response.body else {
        panic!("count is buffered")
    };
    assert_eq!(body, br#"{"input_tokens":4}"#.as_slice());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
}

/// count_tokens checks `default` rules against the client's original request, not the
/// working body (868e137): a default for a field only the original sets is not added.
#[tokio::test]
async fn count_defaults_read_the_original_request() {
    let mut credential = Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/claude.json"),
        serde_json::json!({"type":"claude","access_token":"sk-ant-oat-fake"})
            .as_object()
            .unwrap()
            .clone(),
    )
    .unwrap();
    // A custom origin counts locally, so nothing is sent.
    credential
        .attributes
        .insert("base_url".into(), "http://127.0.0.1:9".into());
    let working = br#"{"model":"claude","messages":[{"role":"user","content":"Hello."}]}"#;
    let count = |config: &'static str, original: &'static [u8]| {
        let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
        let credential = credential.clone();
        async move {
            let req = ExecRequest {
                operation: Operation::CountTokens,
                source_format: Format::Claude,
                response_format: Format::Claude,
                requested_model: "claude".into(),
                model: "claude".into(),
                original_body: Bytes::from_static(original),
                body: Bytes::from_static(working),
                stream: false,
                alt: None,
                session: None,
                execution_session: None,
                derived_session: None,
                resolved_model: None,
                usage: Default::default(),
                request_path: String::new(),
                headers: Default::default(),
                caller: Caller {
                    principal: "fake-client".into(),
                    source: "x-api-key",
                    provider: String::new(),
                },
            };
            let response = executor
                .execute(&credential, req, &Config::parse(config).unwrap())
                .await
                .unwrap();
            let ResponseBody::Buffered(body) = response.body else {
                panic!("count is buffered")
            };
            String::from_utf8(body.to_vec()).unwrap()
        }
    };
    let rule = "payload:\n  default:\n    - models: [{name: claude}]\n      params: {system: \"A configured system prompt that adds tokens\"}\n";
    let original = br#"{"model":"claude","system":"x","messages":[{"role":"user","content":"Hello."}]}"#;
    let plain = count("", working).await;
    let defaulted = count(rule, working).await;
    assert_ne!(plain, defaulted, "the default applies when neither body sets system");
    assert_eq!(count(rule, original).await, plain, "the original sets system");
}

/// Messages update the credential's quota snapshot on success, decode failure and
/// upstream rejection; local token counts do not invent quota observations.
#[tokio::test]
async fn messages_responses_record_the_quota_snapshot() {
    use axum::response::IntoResponse;
    let replies = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from([
        (200u16, "allowed"),
        (429, "rejected"),
        (200, "undecodable"),
        (200, "count-must-not-observe"),
    ])));
    let router = axum::Router::new().fallback(move || {
        let replies = replies.clone();
        async move {
            let (status, value) = replies.lock().unwrap().pop_front().unwrap();
            let body = if status == 200 {
                r#"{"id":"msg_q","type":"message","role":"assistant","model":"m","content":[],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1},"input_tokens":3}"#
            } else {
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"limited"}}"#
            };
            // A 2xx whose gzip body cannot be decoded still observes the headers.
            let encoding = if value == "undecodable" { "gzip" } else { "identity" };
            (
                http::StatusCode::from_u16(status).unwrap(),
                [
                    ("content-type", "application/json"),
                    ("content-encoding", encoding),
                    ("anthropic-ratelimit-unified-status", value),
                    ("anthropic-workspace-id", "ws-not-a-signal"),
                ],
                body,
            )
                .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let mut credential = Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/claude.json"),
        serde_json::json!({"type":"claude"}).as_object().unwrap().clone(),
    )
    .unwrap();
    credential
        .attributes
        .insert("api_key".into(), "fake-gateway-key".into());
    credential.attributes.insert("base_url".into(), base);
    let request = |operation| {
        let body = Bytes::from_static(br#"{"model":"m","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}"#);
        ExecRequest {
            operation,
            source_format: Format::Claude,
            response_format: Format::Claude,
            requested_model: "m".into(),
            model: "m".into(),
            original_body: body.clone(),
            body,
            stream: false,
            alt: None,
            session: None,
            execution_session: None,
            derived_session: None,
            resolved_model: None,
            usage: Default::default(),
            request_path: String::new(),
            headers: Default::default(),
            caller: Caller {
                principal: "fake-client".into(),
                source: "x-api-key",
                provider: String::new(),
            },
        }
    };
    let cfg = Config::parse("").unwrap();
    let signal = |executor: &ClaudeExecutor| {
        let snapshot = executor.quota().snapshot(&credential.id).unwrap();
        assert_eq!(snapshot.signals.len(), 1, "only quota headers are signals");
        snapshot.signals["Anthropic-Ratelimit-Unified-Status"].clone()
    };
    assert!(executor.quota().snapshot(&credential.id).is_none());
    executor
        .execute(&credential, request(Operation::Generate), &cfg)
        .await
        .unwrap();
    assert_eq!(signal(&executor), "allowed");
    // Go also observes the result model's state (`model_quotas`).
    assert_eq!(
        executor.quota().model_snapshots(&credential.id).get("m"),
        executor.quota().snapshot(&credential.id).as_ref()
    );
    let error = executor
        .execute(&credential, request(Operation::Generate), &cfg)
        .await
        .err()
        .unwrap();
    assert_eq!(error.status, 429);
    assert_eq!(signal(&executor), "rejected");
    assert!(
        executor
            .execute(&credential, request(Operation::Generate), &cfg)
            .await
            .is_err()
    );
    assert_eq!(signal(&executor), "undecodable");
    // A custom origin counts locally and never observes.
    executor
        .execute(&credential, request(Operation::CountTokens), &cfg)
        .await
        .unwrap();
    assert_eq!(signal(&executor), "undecodable");
}

/// Apply the server's shared session derivation before preparing a request.
fn enrich(mut req: ExecRequest) -> ExecRequest {
    use cpa_common::session::{self as shared, Meta};
    let scope = shared::caller_scope(&req.caller.principal);
    req.derived_session = shared::derived_id(
        req.source_format,
        &req.headers,
        &req.original_body,
        req.execution_session.as_deref(),
        &scope,
    );
    let meta = Meta {
        execution_session: req.execution_session.as_deref(),
        derived: req.derived_session.as_deref(),
    };
    let id = shared::extract_session_id(&req.headers, &req.original_body, &meta);
    req.session = (!id.is_empty()).then(|| shared::bound_session_identity(&id));
    req
}

/// Targeted request edits preserve unknown fields and untouched byte spans.
#[test]
fn payload_edits_preserve_unmodified_bytes_and_json_meaning() {
    use crate::rawjson::{delete, set_raw, set_str};
    let body = r#"{ "unknown":1e+03, "messages":[{"role":"user","content":"a<b"}], "nested":{"value":true} }"#;
    let changed = set_str(body, "messages.0.content", "changed <text>");
    assert!(changed.starts_with(r#"{ "unknown":1e+03, "messages":[{"role":"user","content":"#));
    assert!(changed.ends_with(r#"}], "nested":{"value":true} }"#));
    let parsed: Value = serde_json::from_str(&changed).unwrap();
    assert_eq!(parsed["messages"][0]["content"], "changed <text>");
    let changed = set_raw(&changed, "nested.value", r#"{"keep":9007199254740993}"#);
    let parsed: Value = serde_json::from_str(&changed).unwrap();
    assert_eq!(parsed["nested"]["value"]["keep"].as_u64(), Some(9_007_199_254_740_993));
    assert!(changed.contains(r#""unknown":1e+03"#));
    let deleted = delete(&changed, "nested.value");
    let parsed: Value = serde_json::from_str(&deleted).unwrap();
    assert_eq!(parsed["nested"], serde_json::json!({}));
    assert_eq!(delete(body, "missing.path"), body);
    assert_eq!(
        serde_json::from_str::<Value>(&set_str("{}", "metadata.user_id", "identity")).unwrap(),
        serde_json::json!({"metadata":{"user_id":"identity"}})
    );
}

/// Prepare a native Messages operation with public config, without opening a socket.
async fn prepare_native_contract(
    body: &str,
    headers: http::HeaderMap,
    credential: &Credential,
    cfg: &Config,
    count: bool,
) -> Result<Prepared, ExecError> {
    let executor = ClaudeExecutor::with_client(wreq::Client::builder().no_proxy().build().unwrap(), DEFAULT_BASE_URL);
    let mut req = client_request(body.to_owned(), headers);
    req.operation = if count {
        Operation::CountTokens
    } else {
        Operation::Generate
    };
    let ctx = Ctx::new(&executor, credential, &req, cfg);
    let translated = translate::request(
        &req,
        ctx.codex,
        &ctx.base_model,
        ctx.is_compat,
        ctx.tool_search(),
        ctx.system_turns(&req, count),
    )?;
    if count {
        let original = translate::count_original(
            &req,
            &translated,
            ctx.codex,
            &ctx.base_model,
            ctx.is_compat,
            ctx.tool_search(),
            ctx.system_turns(&req, true),
        )?;
        ctx.prepare_count(&req, &translated, &original).await
    } else {
        let original = translate::original(
            &req,
            &translated,
            ctx.codex,
            &ctx.base_model,
            ctx.is_compat,
            ctx.tool_search(),
            ctx.system_turns(&req, false),
        )?;
        ctx.prepare_messages(&req, &translated, &original, req.stream).await
    }
}

/// Anthropic's explicit-cache mode gives the caller ownership of its breakpoints.
/// Normal agent requests use the configured shared prefix; subagents opt in to 1h
/// independently, avoiding an accidental TTL upgrade of short-lived helper traffic.
#[tokio::test]
async fn cache_ownership_and_subagent_ttl_are_explicit() {
    let cfg = Config::parse("").unwrap();
    let credential = harness_credential();
    for (name, mode, subagent, requested_1h) in [
        ("ordinary", "auto", false, false),
        ("subagent short", "auto", true, false),
        ("subagent requested one hour", "auto", true, true),
        ("explicit caller cache", "explicit", false, false),
        ("explicit subagent cache", "explicit", true, false),
    ] {
        let mut body = serde_json::json!({"model":"claude-sonnet-4-6","max_tokens":128,"prompt_cache_options":{"mode":mode},"tools":[{"name":"lookup","input_schema":{"type":"object"}}],"system":[{"type":"text","text":"House rules"}],"messages":[{"role":"user","content":[{"type":"text","text":"Keep this prefix","cache_control":{"type":"ephemeral"}}]}]});
        let mut headers = http::HeaderMap::new();
        if subagent {
            headers.insert("x-claude-code-agent-id", "child-1".parse().unwrap());
        }
        if requested_1h {
            headers.insert("anthropic-beta", betas::EXTENDED_CACHE_TTL.parse().unwrap());
        }
        if mode == "explicit" {
            let marker = serde_json::json!({"type":"ephemeral","ttl":"1h"});
            body["tools"][0]["cache_control"] = marker.clone();
            body["system"][0]["cache_control"] = marker.clone();
            body["messages"][0]["content"][0]["cache_control"] = marker;
        }
        let prepared = prepare_native_contract(&body.to_string(), headers, &credential, &cfg, false)
            .await
            .unwrap();
        let sent: Value = serde_json::from_str(&prepared.body).unwrap();
        assert!(
            sent.get("prompt_cache_options").is_none(),
            "{name}: proxy option leaked"
        );
        let markers: Vec<_> = signals::blocks(&prepared.body)
            .into_iter()
            .filter_map(|(_, block)| {
                let value: Value = serde_json::from_str(&block).unwrap();
                value.get("cache_control").cloned()
            })
            .collect();
        assert!(!markers.is_empty(), "{name}");
        assert!(markers.len() <= 4, "{name}");
        if mode == "explicit" {
            assert_eq!(
                markers,
                vec![serde_json::json!({"type":"ephemeral","ttl":"1h"}); 3],
                "{name}"
            );
        } else if !subagent || requested_1h {
            assert!(markers.iter().all(|cc| cc["ttl"] == "1h"), "{name}: {markers:?}");
        } else {
            assert!(markers.iter().all(|cc| cc.get("ttl").is_none()), "{name}: {markers:?}");
        }
        assert!(prepared.body.contains("Keep this prefix"));
    }
}

#[tokio::test]
async fn explicit_cache_markers_survive_counting_and_never_create_automatic_markers() {
    let cfg = Config::parse("").unwrap();
    let credential = harness_credential();
    for model in ["claude-sonnet-4-5", "claude-sonnet-4-6"] {
        for with_markers in [false, true] {
            let mut body = serde_json::json!({"model":model,"max_tokens":128,"prompt_cache_options":{"mode":"explicit"},"system":[{"type":"text","text":"Caller system"}],"tools":[{"name":"lookup","input_schema":{"type":"object"}}],"messages":[{"role":"user","content":[{"type":"text","text":"Keep the caller cache"}]}]});
            if with_markers {
                body["system"][0]["cache_control"] = serde_json::json!({"type":"ephemeral","ttl":"1h"});
                body["tools"][0]["cache_control"] = serde_json::json!({"type":"ephemeral","ttl":"1h"});
                body["messages"][0]["content"][0]["cache_control"] = serde_json::json!({"type":"ephemeral"});
            }
            for count in [false, true] {
                let prepared = prepare_native_contract(&body.to_string(), Default::default(), &credential, &cfg, count)
                    .await
                    .unwrap();
                assert!(!rawjson::get(&prepared.body, "prompt_cache_options").exists());
                assert_eq!(
                    cloak::count_cache_controls(&prepared.body),
                    if with_markers { 3 } else { 0 },
                    "model={model} count={count}"
                );
                if with_markers {
                    let markers = signals::blocks(&prepared.body)
                        .into_iter()
                        .filter_map(|(_, b)| serde_json::from_str::<Value>(&b).unwrap().get("cache_control").cloned())
                        .collect::<Vec<_>>();
                    assert_eq!(markers.iter().filter(|cc| cc["ttl"] == "1h").count(), 2);
                }
            }
        }
    }
}

/// Count-token requests are independently shaped and operator rules remain final.
/// The upstream API excludes generated identity/diagnostics and unsupported tools.
#[tokio::test]
async fn count_payload_rules_keep_the_operator_system_and_filter_tools() {
    let cfg = Config::parse(
        r#"
requests:
  payload:
    override:
      - models: [{name: '*', protocol: claude}]
        params: {system: "Configured count system"}
    default-raw:
      - models: [{name: '*', protocol: claude}]
        params: {diagnostics: '{"operator":true}'}
    filter:
      - models: [{name: '*', protocol: claude}]
        params: [tools.1]
"#,
    )
    .unwrap();
    let body = serde_json::json!({"model":"claude-opus-5-5","system":"Caller count system","metadata":{"user_id":"caller"},"tools":[{"name":"lookup","input_schema":{"type":"object"}}, {"type":"web_search_20250305","name":"web_search","allowed_domains":[]}],"messages":[{"role":"user","content":"Count these words"}]});
    let prepared = prepare_native_contract(&body.to_string(), Default::default(), &harness_credential(), &cfg, true)
        .await
        .unwrap();
    let sent: Value = serde_json::from_str(&prepared.body).unwrap();
    assert_eq!(sent["system"], "Configured count system");
    assert_eq!(sent["diagnostics"], serde_json::json!({"operator":true}));
    assert_eq!(sent["tools"].as_array().unwrap().len(), 1);
    assert!(sent.to_string().contains("Count these words"));
    assert!(sent.get("metadata").is_none());
    assert!(sent.get("context_management").is_none());
}

/// Cloaking policy and supported header configuration are observable operator choices.
#[tokio::test]
async fn configured_cloaking_and_header_defaults_control_the_prepared_request() {
    let body = r#"{"model":"claude-sonnet-4-6","max_tokens":128,"system":"Preserve these house rules","messages":[{"role":"user","content":"Keep the question"}]}"#;
    let credential = harness_credential();
    for disabled in [false, true] {
        let cfg = Config::parse(&format!("upstream:\n  claude:\n    disable-claude-cloak-mode: {disabled}\n    header-defaults:\n      os: Linux\n      arch: x64\n      timezone: UTC\n")).unwrap();
        let prepared = prepare_native_contract(body, Default::default(), &credential, &cfg, false)
            .await
            .unwrap();
        assert_eq!(prepared.body.contains(cloak::CLI_IDENTITY), !disabled);
        assert!(prepared.body.contains("Preserve these house rules"));
        assert!(prepared.body.contains("Keep the question"));
        let headers: http::HeaderMap = prepared
            .headers
            .iter()
            .map(|(k, v)| {
                (
                    k.parse::<http::HeaderName>().unwrap(),
                    v.parse::<http::HeaderValue>().unwrap(),
                )
            })
            .collect();
        assert_eq!(headers["authorization"], "Bearer sk-ant-oat01-FAKE-LOCAL-ONLY");
        if !disabled {
            assert_eq!(headers["x-stainless-os"], "Linux");
            assert_eq!(headers["x-stainless-arch"], "x64");
        }
    }
    let mut key = harness_credential();
    key.attributes.insert("api_key".into(), "private-test-key".into());
    for strict in [false, true] {
        let cfg = Config::parse(&format!("api-keys:\n  claude:\n    - keys:\n        - api-key: private-test-key\n          cloak:\n            mode: always\n            strict-mode: {strict}\n")).unwrap();
        let prepared = prepare_native_contract(body, Default::default(), &key, &cfg, false)
            .await
            .unwrap();
        assert!(prepared.body.contains(cloak::CLI_IDENTITY));
        assert_eq!(prepared.body.contains("Preserve these house rules"), !strict);
        assert!(prepared.body.contains("Keep the question"));
    }
}

#[tokio::test]
async fn helper_requests_omit_interactive_continuity_and_disabled_images_leave_other_tools() {
    let credential = harness_credential();
    let cfg = Config::parse("").unwrap();
    for (tokens, system, question) in [
        (1, "House rules", "quota"),
        (64, "Return a short title for this coding session.", "Fix the login bug"),
    ] {
        let body = serde_json::json!({"model":"claude-sonnet-4-6","max_tokens":tokens,"system":system,"messages":[{"role":"user","content":question}]});
        let prepared = prepare_native_contract(&body.to_string(), Default::default(), &credential, &cfg, false)
            .await
            .unwrap();
        assert!(prepared.continuity.key.is_empty());
        assert!(rawjson::get(&prepared.body, "diagnostics").json().is_empty());
        assert!(!prepared.body.contains("cc_turn_origin=human"));
        assert!(prepared.body.contains(question));
        assert!(!signals::has_1h_ttl(&prepared.body));
    }
    let cfg = Config::parse("disable-image-generation: true").unwrap();
    for only_image in [false, true] {
        let mut body = serde_json::json!({"model":"claude-sonnet-4-6","max_tokens":128,"messages":[{"role":"user","content":"Keep the question"}],"tools":[{"type":"image_generation"}],"tool_choice":{"type":"image_generation"}});
        if !only_image {
            body["tools"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({"name":"lookup","input_schema":{"type":"object"}}));
        }
        let prepared = prepare_native_contract(&body.to_string(), Default::default(), &credential, &cfg, false)
            .await
            .unwrap();
        assert!(!prepared.body.contains("image_generation"));
        assert_eq!(prepared.reverse.values().any(|name| name == "lookup"), !only_image);
        assert!(prepared.body.contains("Keep the question"));
    }
}

/// Caller system instructions follow the leading user run on current models.
/// Per-turn effort directives keep their place across compaction; legacy first-party
/// routes refuse unsupported middle-system turns unless rebuilding is configured.
#[tokio::test]
async fn system_placement_keeps_effort_and_validates_the_final_model() {
    let cfg = Config::parse("").unwrap();
    let credential = harness_credential();
    for count in [false, true] {
        let body = serde_json::json!({"model":"claude-opus-5-5","max_tokens":64,"system":"Caller guidance","messages":[{"role":"user","content":"<conversation-checkpoint>Earlier context</conversation-checkpoint>"},{"role":"system","content":[],"output_config":{"effort":"low"}},{"role":"user","content":"Next question"},{"role":"assistant","content":"Answer"},{"role":"user","content":"Continue"}]});
        let prepared = prepare_native_contract(&body.to_string(), Default::default(), &credential, &cfg, count)
            .await
            .unwrap();
        let sent: Value = serde_json::from_str(&prepared.body).unwrap();
        let messages = sent["messages"].as_array().unwrap();
        assert_eq!(messages[1]["output_config"]["effort"], "low");
        assert_eq!(messages[1]["role"], "system");
        let system = messages
            .iter()
            .position(|m| m["role"] == "system" && m.to_string().contains("Caller guidance"))
            .unwrap();
        let answer = messages.iter().position(|m| m["role"] == "assistant").unwrap();
        assert!(system > 2 && system < answer, "{messages:?}");
        assert!(prepared.body.contains("Earlier context"));
        assert!(prepared.body.contains("Next question"));
    }
    let body = r#"{"model":"claude-sonnet-4-5","max_tokens":64,"messages":[{"role":"user","content":"First"},{"role":"system","content":"Middle guidance"},{"role":"user","content":"Next"}]}"#;
    for count in [false, true] {
        let error = prepare_native_contract(body, Default::default(), &credential, &cfg, count)
            .await
            .err()
            .unwrap();
        assert_eq!((error.status, error.scope), (400, FailureScope::Request));
        let mut gateway = credential.clone();
        gateway
            .attributes
            .insert("base_url".into(), "http://gateway.invalid".into());
        let allowed = prepare_native_contract(body, Default::default(), &gateway, &cfg, count)
            .await
            .unwrap();
        assert!(allowed.body.contains("Middle guidance"));
    }
    let repaired_cfg = Config::parse("api-keys:\n  claude:\n    - keys:\n        - api-key: sk-ant-oat01-FAKE-LOCAL-ONLY\n          rebuild-mid-system-message: true\n").unwrap();
    let repaired = prepare_native_contract(body, Default::default(), &credential, &repaired_cfg, false)
        .await
        .unwrap();
    let repaired: Value = serde_json::from_str(&repaired.body).unwrap();
    assert!(repaired.to_string().contains("Middle guidance"));
    assert!(
        !repaired["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "system")
    );
}

/// A decoded HTTP error keeps its status/body. Fast mode returns it directly and
/// stays request-scoped, while decoding failures must never masquerade as success.
#[tokio::test]
async fn fast_errors_and_compressed_responses_preserve_status_and_failure_boundaries() {
    use std::io::Write;
    let gzip = |data: &[u8]| {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    };
    for fast in [false, true] {
        for status in [200, 429, 500] {
            for invalid in [false, true] {
                let original = if status == 200 {
                    b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".as_slice()
                } else {
                    br#"{"error":{"type":"overloaded_error","message":"try later"}}"#.as_slice()
                };
                let mut headers = http::HeaderMap::new();
                headers.insert("content-encoding", "gzip".parse().unwrap());
                headers.insert(
                    "content-type",
                    if status == 200 {
                        "text/event-stream"
                    } else {
                        "application/json"
                    }
                    .parse()
                    .unwrap(),
                );
                headers.insert("x-request-id", "local-request".parse().unwrap());
                let data = if invalid {
                    b"invalid gzip".to_vec()
                } else {
                    gzip(original)
                };
                let decoded = decode_upstream(crate::proxy::Upstream {
                    status,
                    headers,
                    body: futures_util::stream::iter([Ok(Bytes::from(data))]).boxed(),
                })
                .await;
                let result = finish(decoded, fast, false, &Default::default()).await;
                if status == 200 && !invalid {
                    let response = result.unwrap();
                    assert_eq!(response.status, 200);
                    assert!(response.headers.get("content-encoding").is_none());
                    let ResponseBody::Stream(raw) = response.body else {
                        panic!("decoded response must stream")
                    };
                    assert_eq!(collect(raw).await.unwrap().as_ref(), original);
                } else {
                    let error = result.err().expect("upstream failure");
                    if status == 200 {
                        assert!(error.status >= 400);
                        assert!(!error.direct);
                    } else {
                        assert_eq!(error.status, status);
                        assert_eq!(error.direct, fast && !invalid);
                    }
                    assert_eq!(
                        error.scope,
                        if fast {
                            FailureScope::Request
                        } else if status == 200 {
                            FailureScope::Transport
                        } else if status == 429 {
                            FailureScope::Model
                        } else {
                            FailureScope::Credential
                        }
                    );
                    if status != 200 {
                        assert_eq!(error.headers["x-request-id"], "local-request");
                    }
                    assert!(error.headers.get("content-encoding").is_none());
                    if !invalid {
                        assert_eq!(error.body.as_ref(), original);
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn duplicate_identity_metadata_is_refused_before_it_can_choose_credentials() {
    for metadata in [
        r#""metadata":{},"metadata":{"user_id":"attacker"}"#,
        r#""metadata":{"user_id":"first","user_id":"second"}"#,
        r#""metadata":{"user_id":"{\"extra\":1,\"extra\":2}"}"#,
    ] {
        let body = format!(
            r#"{{"model":"claude-sonnet-4-6","max_tokens":128,"messages":[{{"role":"user","content":"Question"}}],{metadata}}}"#
        );
        let error = prepare_native_contract(
            &body,
            Default::default(),
            &harness_credential(),
            &Config::parse("").unwrap(),
            false,
        )
        .await
        .err()
        .unwrap();
        assert_eq!((error.status, error.scope), (400, FailureScope::Request));
        assert!(!error.direct);
    }
}

#[tokio::test]
async fn native_search_aliases_definitions_history_and_references_before_final_payload_rules() {
    for count in [false, true] {
        for final_rule in [false, true] {
            let rules = if final_rule {
                "requests:\n  payload:\n    override:\n      - models: [{name: '*', protocol: claude}]\n        params: {tools.0.name: operator-final-name}\n"
            } else {
                ""
            };
            let cfg = Config::parse(&format!("upstream:\n  claude:\n    tool-search: true\n{rules}")).unwrap();
            let credential = harness_credential();
            let executor =
                ClaudeExecutor::with_client(wreq::Client::builder().no_proxy().build().unwrap(), DEFAULT_BASE_URL);
            let body = serde_json::json!({"model":"claude-sonnet-4-6","stream":false,"tools":[{"type":"function","name":"shell","parameters":{"type":"object"}},{"type":"tool_search","execution":"client","parameters":{"type":"object"}}],"input":[{"type":"message","role":"user","content":"Find the weather tool"},{"type":"tool_search_call","call_id":"previous","execution":"client","arguments":{"query":"weather"}},{"type":"tool_search_output","call_id":"previous","execution":"client","tools":[{"type":"function","name":"get_weather","parameters":{"type":"object"},"defer_loading":true}]},{"type":"message","role":"user","content":"Paris"}]}).to_string();
            let mut req = client_request(body, Default::default());
            req.operation = if count {
                Operation::CountTokens
            } else {
                Operation::Generate
            };
            req.source_format = Format::OpenAIResponse;
            req.response_format = Format::OpenAIResponse;
            let ctx = Ctx::new(&executor, &credential, &req, &cfg);
            ctx.tool_search_fallback(&mut req);
            let translated = translate::request(
                &req,
                ctx.codex,
                &ctx.base_model,
                ctx.is_compat,
                ctx.tool_search(),
                ctx.system_turns(&req, count),
            )
            .unwrap();
            let prepared = if count {
                let original = translate::count_original(
                    &req,
                    &translated,
                    ctx.codex,
                    &ctx.base_model,
                    ctx.is_compat,
                    ctx.tool_search(),
                    ctx.system_turns(&req, true),
                )
                .unwrap();
                ctx.prepare_count(&req, &translated, &original).await.unwrap()
            } else {
                let original = translate::original(
                    &req,
                    &translated,
                    ctx.codex,
                    &ctx.base_model,
                    ctx.is_compat,
                    ctx.tool_search(),
                    ctx.system_turns(&req, false),
                )
                .unwrap();
                ctx.prepare_messages(&req, &translated, &original, true).await.unwrap()
            };
            let sent: Value = serde_json::from_str(&prepared.body).unwrap();
            let tools = sent["tools"].as_array().unwrap();
            if final_rule {
                assert_eq!(tools[0]["name"], "operator-final-name");
            }
            let weather = tools
                .iter()
                .find(|tool| {
                    tool["defer_loading"] == true
                        && tool["name"].as_str().is_some_and(|name| name.ends_with("get_weather"))
                })
                .unwrap();
            let name = weather["name"].as_str().unwrap();
            assert_ne!(name, "get_weather");
            let mut refs = Vec::new();
            fn references(value: &Value, output: &mut Vec<String>) {
                match value {
                    Value::Object(fields) => {
                        if fields.get("type").is_some_and(|v| v == "tool_reference") {
                            output.push(fields["tool_name"].as_str().unwrap().to_owned());
                        }
                        for value in fields.values() {
                            references(value, output);
                        }
                    }
                    Value::Array(items) => {
                        for value in items {
                            references(value, output);
                        }
                    }
                    _ => {}
                }
            }
            references(&sent["messages"], &mut refs);
            assert_eq!(refs, [name], "count={count} final_rule={final_rule}");
            assert!(prepared.body.contains("previous"));
            assert!(prepared.body.contains("Paris"));
        }
    }
}

#[tokio::test]
async fn thread_continuations_reuse_completed_aliases_and_reserve_a_cache_breakpoint() {
    let executor = ClaudeExecutor::with_client(wreq::Client::builder().no_proxy().build().unwrap(), DEFAULT_BASE_URL);
    let mut credential = harness_credential();
    credential.id = format!("local-thread-{}", uuid::Uuid::new_v4());
    let cfg = Config::parse("").unwrap();
    let create = serde_json::json!({"model":"claude-opus-5-5","max_tokens":64,"thread":{"type":"create"},"system":[{"type":"text","text":"Thread rules","cache_control":{"type":"ephemeral"}},{"type":"text","text":"Additional rules","cache_control":{"type":"ephemeral"}}],"tools":[{"name":"Read","input_schema":{"type":"object"},"cache_control":{"type":"ephemeral"}}],"messages":[{"role":"user","content":[{"type":"text","text":"Read the changelog","cache_control":{"type":"ephemeral"}}]}]}).to_string();
    let req = client_request(create, Default::default());
    let ctx = Ctx::new(&executor, &credential, &req, &cfg);
    let translated = translate::request(
        &req,
        ctx.codex,
        &ctx.base_model,
        ctx.is_compat,
        ctx.tool_search(),
        ctx.system_turns(&req, false),
    )
    .unwrap();
    let original = translate::original(
        &req,
        &translated,
        ctx.codex,
        &ctx.base_model,
        ctx.is_compat,
        ctx.tool_search(),
        ctx.system_turns(&req, false),
    )
    .unwrap();
    let mut prepared = ctx.prepare_messages(&req, &translated, &original, false).await.unwrap();
    assert!(
        cloak::count_cache_controls(&prepared.body) <= 3,
        "thread itself occupies the fourth breakpoint"
    );
    let aliases = prepared.reverse.clone();
    let alias = aliases
        .iter()
        .find(|(_, name)| name.as_str() == "Read")
        .unwrap()
        .0
        .clone();
    ctx.completion(&mut prepared)
        .complete("msg_thread_created", "req_thread_created");
    for previous in ["msg_thread_created", "unknown-message"] {
        let body = serde_json::json!({"model":"claude-opus-5-5","max_tokens":64,"thread":{"type":"continue","previous_message_id":previous},"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool_read","content":"Version notes"}]}]}).to_string();
        let req = client_request(body, Default::default());
        let ctx = Ctx::new(&executor, &credential, &req, &cfg);
        let translated = translate::request(
            &req,
            ctx.codex,
            &ctx.base_model,
            ctx.is_compat,
            ctx.tool_search(),
            ctx.system_turns(&req, false),
        )
        .unwrap();
        let original = translate::original(
            &req,
            &translated,
            ctx.codex,
            &ctx.base_model,
            ctx.is_compat,
            ctx.tool_search(),
            ctx.system_turns(&req, false),
        )
        .unwrap();
        let result = ctx.prepare_messages(&req, &translated, &original, false).await;
        if previous == "unknown-message" {
            let error = result.err().unwrap();
            assert_eq!((error.status, error.scope), (404, FailureScope::Request));
        } else {
            let prepared = result.unwrap();
            assert!(Arc::ptr_eq(&prepared.reverse, &aliases));
            let response =
                serde_json::json!({"content":[{"type":"tool_use","name":alias,"id":"tool_next","input":{}}]})
                    .to_string();
            let restored: Value =
                serde_json::from_str(&alias::restore_response(&response, &prepared.reverse).unwrap()).unwrap();
            assert_eq!(restored["content"][0]["name"], "Read");
            assert!(prepared.body.contains("Version notes"));
        }
    }
}

/// Synthetic envelopes prove the local replay-safety policy, not cryptographic
/// validity or provider acceptance. Foreign and malformed thinking is removed before
/// sending, while valid structural history and ordinary tool data survive.
#[tokio::test]
async fn request_preparation_sanitizes_history_for_known_claude_models_only() {
    use base64::Engine;
    let model = b"synthetic-claude";
    let mut inner = vec![0x08, 11, 0x32, model.len() as u8];
    inner.extend_from_slice(model);
    let mut envelope = vec![0x12, (inner.len() + 2) as u8, 0x0a, inner.len() as u8];
    envelope.extend(inner);
    let signature = base64::engine::general_purpose::STANDARD.encode(envelope);
    let mut credential = harness_credential();
    credential.attributes.insert("api_key".into(), "local-api-key".into());
    let cfg = Config::parse("").unwrap();
    for model in ["claude-sonnet-4-6", "opaque-custom-model"] {
        let body = serde_json::json!({"model":model,"max_tokens":4096,"messages":[{"role":"user","content":"Question"},{"role":"assistant","content":[{"type":"thinking","thinking":"keep valid history","signature":signature},{"type":"thinking","thinking":"drop malformed history","signature":"not base64"},{"type":"thinking","thinking":"drop encrypted foreign history","signature":"gAAAA-not-valid"},{"type":"thinking","thinking":"","signature":""},{"type":"tool_use","id":"tool_one","name":"lookup","input":{"retain":9007199254740993u64},"signature":"foreign","thought_signature":"foreign"},{"type":"text","text":"Keep the answer"}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool_one","content":"Result"}]}]});
        let prepared = prepare_native_contract(&body.to_string(), Default::default(), &credential, &cfg, false)
            .await
            .unwrap();
        let sent: Value = serde_json::from_str(&prepared.body).unwrap();
        let blocks = sent["messages"][1]["content"].as_array().unwrap();
        assert!(prepared.body.contains("keep valid history"));
        assert!(prepared.body.contains("Keep the answer"));
        assert!(prepared.body.contains("Result"));
        let tool = blocks.iter().find(|block| block["type"] == "tool_use").unwrap();
        assert_eq!(tool["input"], serde_json::json!({"retain":9007199254740993u64}));
        if model.starts_with("claude-") {
            assert_eq!(blocks.len(), 3);
            assert!(!prepared.body.contains("drop malformed history"));
            assert!(!prepared.body.contains("drop encrypted foreign history"));
            assert!(tool.get("signature").is_none());
            assert!(tool.get("thought_signature").is_none());
        } else {
            assert_eq!(
                sent["messages"][1], body["messages"][1],
                "unknown providers own their signature format"
            );
        }
    }
}

#[tokio::test]
async fn request_thinking_suffix_uses_the_base_model_and_manual_token_budget() {
    let mut credential = harness_credential();
    credential.attributes.insert("api_key".into(), "local-api-key".into());
    let cfg = Config::parse("").unwrap();
    let body = r#"{"model":"claude-sonnet-4-5(2048)","max_tokens":4096,"messages":[{"role":"user","content":"Reason through this"}]}"#;
    let prepared = prepare_native_contract(body, Default::default(), &credential, &cfg, false)
        .await
        .unwrap();
    let sent: Value = serde_json::from_str(&prepared.body).unwrap();
    assert_eq!(sent["model"], "claude-sonnet-4-5");
    assert_eq!(sent["thinking"]["type"], "enabled");
    assert_eq!(sent["thinking"]["budget_tokens"], 2048);
    assert_eq!(sent["max_tokens"], 4096);
}

#[tokio::test]
async fn translated_stream_failures_cannot_return_a_completed_response() {
    for source in [Format::OpenAI, Format::OpenAIResponse] {
        for stream in [false, true] {
            for (name, reply) in [
                ("empty", ""),
                (
                    "incomplete",
                    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"partial\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-4-6\",\"content\":[]}}\n\n",
                ),
                (
                    "upstream error",
                    "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Provider refused\"}}\n\n",
                ),
            ] {
                let router = axum::Router::new()
                    .fallback(move || async move { ([(http::header::CONTENT_TYPE, "text/event-stream")], reply) });
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let base = format!("http://{}", listener.local_addr().unwrap());
                let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
                let mut credential = harness_credential();
                credential.attributes.insert("base_url".into(), base);
                let auth = std::env::temp_dir().join(format!("cpa-stream-failure-{}", uuid::Uuid::new_v4()));
                let cfg = Config::parse(&format!("oauth:\n  auth-dir: {}\n", auth.display())).unwrap();
                let body = if source == Format::OpenAI {
                    serde_json::json!({"model":"claude-sonnet-4-6","stream":stream,"messages":[{"role":"user","content":"Question"}]})
                } else {
                    serde_json::json!({"model":"claude-sonnet-4-6","stream":stream,"input":"Question"})
                };
                let mut req = client_request(body.to_string(), Default::default());
                req.source_format = source;
                req.response_format = source;
                let executor =
                    ClaudeExecutor::with_client(wreq::Client::builder().no_proxy().build().unwrap(), DEFAULT_BASE_URL);
                let result = executor.execute(&credential, req, &cfg).await;
                match result {
                    Err(error) => assert!(error.status >= 400, "{name}: {source:?} stream={stream}"),
                    Ok(response) => {
                        let ResponseBody::Stream(events) = response.body else {
                            panic!("{name}: {source:?} returned buffered success")
                        };
                        let chunks: Vec<_> = events.collect().await;
                        for error in chunks.iter().filter_map(|chunk| chunk.as_ref().err()) {
                            assert!(error.status >= 400, "{name}: invalid failure status");
                        }
                        // An unfinished peer may close without an error event. It
                        // cannot synthesize a normal terminal marker for either API.
                        let bytes = chunks
                            .iter()
                            .filter_map(|chunk| chunk.as_ref().ok())
                            .flat_map(|bytes| bytes.to_vec())
                            .collect::<Vec<_>>();
                        let text = String::from_utf8(bytes).unwrap();
                        for data in text.lines().filter_map(|line| line.strip_prefix("data: ")) {
                            assert_ne!(data, "[DONE]", "{name}: {source:?} invented a success terminator");
                            if let Ok(event) = serde_json::from_str::<Value>(data) {
                                assert_ne!(
                                    event["type"], "response.completed",
                                    "{name}: incomplete Responses stream completed"
                                );
                                if let Some(choices) = event["choices"].as_array() {
                                    assert!(
                                        choices
                                            .iter()
                                            .all(|choice| choice.get("finish_reason").is_none_or(Value::is_null)),
                                        "{name}: incomplete Chat stream finished: {event}"
                                    );
                                }
                            }
                        }
                    }
                }
                server.abort();
            }
        }
    }
}

#[tokio::test]
async fn oauth_tool_calls_round_trip_across_supported_client_formats() {
    use axum::response::IntoResponse;
    for source in [Format::Claude, Format::OpenAI, Format::OpenAIResponse] {
        for stream in [false, true] {
            let seen = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
            let captured = seen.clone();
            let router=axum::Router::new().fallback(move |body:Bytes| {
                let sent:Value=serde_json::from_slice(&body).unwrap();
                captured.lock().unwrap().push(sent.clone());
                async move {
                    let tool=sent["tools"][0]["name"].as_str().unwrap();
                    let call=serde_json::json!({"type":"tool_use","id":"tool_patch","name":tool,"input":{"patch":"*** Begin Patch\n*** End Patch"}});
                    let message=serde_json::json!({"id":"msg_patch","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[call.clone()],"stop_reason":"tool_use","usage":{"input_tokens":3,"output_tokens":4}});
                    if sent["stream"]==true {
                        let events=[serde_json::json!({"type":"message_start","message":{"id":"msg_patch","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[],"usage":{"input_tokens":3,"output_tokens":0}}}),serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tool_patch","name":tool,"input":{}}}),serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":call["input"].to_string()}}),serde_json::json!({"type":"content_block_stop","index":0}),serde_json::json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4}}),serde_json::json!({"type":"message_stop"})];
                        let sse=events.iter().map(|e|format!("event: {}\ndata: {e}\n\n",e["type"].as_str().unwrap())).collect::<String>();
                        ([(http::header::CONTENT_TYPE,"text/event-stream")],sse).into_response()
                    } else {( [(http::header::CONTENT_TYPE,"application/json")],message.to_string()).into_response()}
                }
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            let mut credential = harness_credential();
            credential.attributes.insert("base_url".into(), base);
            let auth = std::env::temp_dir().join(format!("cpa-tool-roundtrip-{}", uuid::Uuid::new_v4()));
            let cfg = Config::parse(&format!("oauth:\n  auth-dir: {}\n", auth.display())).unwrap();
            let body = match source {
                Format::Claude => {
                    serde_json::json!({"model":"claude-sonnet-4-6","max_tokens":64,"stream":stream,"messages":[{"role":"user","content":"Apply a patch"}],"tools":[{"name":"apply_patch","input_schema":{"type":"object","properties":{"patch":{"type":"string"}}}}]})
                }
                Format::OpenAI => {
                    serde_json::json!({"model":"claude-sonnet-4-6","stream":stream,"messages":[{"role":"user","content":"Apply a patch"}],"tools":[{"type":"function","function":{"name":"apply_patch","parameters":{"type":"object","properties":{"patch":{"type":"string"}}}}}]})
                }
                _ => {
                    serde_json::json!({"model":"claude-sonnet-4-6","stream":stream,"input":"Apply a patch","tools":[{"type":"function","name":"apply_patch","parameters":{"type":"object","properties":{"patch":{"type":"string"}}}}]})
                }
            };
            let mut req = client_request(body.to_string(), Default::default());
            req.source_format = source;
            req.response_format = source;
            let executor =
                ClaudeExecutor::with_client(wreq::Client::builder().no_proxy().build().unwrap(), DEFAULT_BASE_URL);
            let response = executor.execute(&credential, req, &cfg).await.unwrap();
            let bytes = match response.body {
                ResponseBody::Buffered(b) => b.to_vec(),
                ResponseBody::Stream(events) => events.map(|chunk| chunk.unwrap()).collect::<Vec<_>>().await.concat(),
            };
            let text = String::from_utf8(bytes).unwrap();
            assert!(text.contains("apply_patch"), "{source:?} stream={stream}: {text}");
            assert!(text.contains("tool_patch"));
            assert!(text.contains("Begin Patch"), "{text}");
            let captured = seen.lock().unwrap();
            assert_eq!(captured.len(), 1);
            let alias = captured[0]["tools"][0]["name"].as_str().unwrap();
            assert_ne!(alias, "apply_patch");
            assert!(!text.contains(alias), "wire alias leaked to client");
            server.abort();
        }
    }
}

#[tokio::test]
async fn native_markerless_helpers_keep_their_wire_shape_and_transport_encoding_is_controlled() {
    let session = "11111111-2222-4333-8444-555555555555";
    let identity = format!(
        r#"{{"device_id":"{}","account_uuid":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","session_id":"{session}"}}"#,
        "f".repeat(64)
    );
    let body = format!(
        r#"{{"model":"claude-haiku-4-5-20251001","max_tokens":1,"messages":[{{"role":"user","content":"helper probe"}}],"metadata":{{"user_id":{}}}}}"#,
        serde_json::to_string(&identity).unwrap()
    );
    let beta = "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05";
    let headers: http::HeaderMap = [
        ("accept", "application/json"),
        ("accept-encoding", "gzip, deflate, br, zstd"),
        ("anthropic-beta", beta),
        ("anthropic-dangerous-direct-browser-access", "true"),
        ("anthropic-version", "2023-06-01"),
        ("content-type", "application/json"),
        ("user-agent", profile::BASELINE.user_agent),
        ("x-app", "cli"),
        ("x-claude-code-session-id", session),
        ("x-stainless-lang", "js"),
        ("x-stainless-runtime", "node"),
        ("x-stainless-retry-count", "0"),
        ("x-stainless-timeout", "600"),
        ("x-stainless-package-version", profile::BASELINE.package_version),
        ("x-stainless-runtime-version", profile::BASELINE.runtime_version),
        ("x-stainless-os", "MacOS"),
        ("x-stainless-arch", "arm64"),
    ]
    .into_iter()
    .map(|(k, v)| {
        (
            k.parse::<http::HeaderName>().unwrap(),
            v.parse::<http::HeaderValue>().unwrap(),
        )
    })
    .collect();
    let cfg = Config::parse("").unwrap();
    let credential = harness_credential();
    assert!(detect::detect(&headers, &body, false, &Settings::from_config(&cfg)).helper_profile);
    let prepared = prepare_native_contract(&body, headers, &credential, &cfg, false)
        .await
        .unwrap();
    for absent in ["system", "stream", "tools", "diagnostics", "context_management"] {
        assert!(
            !rawjson::get(&prepared.body, absent).exists(),
            "{absent} added to markerless helper"
        );
    }
    assert_eq!(cloak::count_cache_controls(&prepared.body), 0);
    assert_eq!(
        rawjson::get(&prepared.body, "messages").json(),
        rawjson::get(&body, "messages").json()
    );
    for stream in [false, true] {
        let body=serde_json::json!({"model":"claude-sonnet-4-6","max_tokens":128,"stream":stream,"messages":[{"role":"user","content":"Question"}]}).to_string();
        let mut headers = http::HeaderMap::new();
        headers.insert("accept-encoding", "custom-unsupported".parse().unwrap());
        let prepared = prepare_native_contract(&body, headers, &credential, &cfg, false)
            .await
            .unwrap();
        let encoding = prepared
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("accept-encoding"))
            .unwrap()
            .1
            .as_str();
        // The first-party native transport negotiates compressed responses for
        // both buffered and streaming calls and decodes before relaying.
        assert!(encoding.split(',').map(str::trim).any(|token| token == "gzip"));
        assert_ne!(encoding, "custom-unsupported");
    }
}

/// Exercise the public payload configuration as the final operator-controlled
/// transformation. A model/probe change or removal must not trigger a second shaping
/// pass that silently overwrites the configured result.
#[tokio::test]
async fn payload_rules_are_final_and_defaults_respect_caller_presence() {
    async fn prepare(body: Value, rules: &str) -> Value {
        let dir = std::env::temp_dir().join(format!("cpa-payload-policy-{}", uuid::Uuid::new_v4()));
        let cfg = Config::parse(&format!(
            "config-version: 8\noauth:\n  auth-dir: {}\n{rules}",
            dir.display()
        ))
        .unwrap();
        let executor =
            ClaudeExecutor::with_client(wreq::Client::builder().no_proxy().build().unwrap(), DEFAULT_BASE_URL);
        let credential = harness_credential();
        let req = enrich(client_request(body.to_string(), Default::default()));
        let ctx = Ctx::new(&executor, &credential, &req, &cfg);
        let translated = translate::request(
            &req,
            ctx.codex,
            &ctx.base_model,
            ctx.is_compat,
            ctx.tool_search(),
            ctx.system_turns(&req, false),
        )
        .unwrap();
        let original = translate::original(
            &req,
            &translated,
            ctx.codex,
            &ctx.base_model,
            ctx.is_compat,
            ctx.tool_search(),
            ctx.system_turns(&req, false),
        )
        .unwrap();
        let result = ctx.prepare_messages(&req, &translated, &original, false).await.unwrap();
        assert_eq!(signing::sign(result.body.clone()).unwrap(), result.body);
        serde_json::from_str(&result.body).unwrap()
    }
    let initial = serde_json::json!({"model":"claude-sonnet-4-6", "max_tokens":256,"messages":[{"role":"user","content":"Preserve my prompt"}],"system":"Original instructions","thinking":{"type":"adaptive"},"extension":{"large":9007199254740993u64}});
    let changed = prepare(
        initial.clone(),
        r#"
requests:
  payload:
    override:
      - models: [{name: '*', protocol: claude}]
        params:
          model: claude-fable-5-1
          max_tokens: 1
          system: Final operator instructions
    override-raw:
      - models: [{name: '*', protocol: claude}]
        params:
          thinking: '{"type":"disabled"}'
          fallbacks: '[{"model":"operator-fallback"}]'
          diagnostics: '{"operator":true}'
"#,
    )
    .await;
    assert_eq!(changed["model"], "claude-fable-5-1");
    assert_eq!(changed["max_tokens"], 1);
    assert_eq!(changed["system"], "Final operator instructions");
    assert_eq!(changed["thinking"], serde_json::json!({"type":"disabled"}));
    assert_eq!(changed["fallbacks"], serde_json::json!([{"model":"operator-fallback"}]));
    assert_eq!(changed["diagnostics"], serde_json::json!({"operator":true}));
    assert_eq!(changed["extension"], initial["extension"]);
    assert!(changed.to_string().contains("Preserve my prompt"));
    let filtered = prepare(
        initial.clone(),
        r#"
requests:
  payload:
    filter:
      - models: [{name: '*', protocol: claude}]
        params: [system, metadata, diagnostics, thinking.display, context_management]
"#,
    )
    .await;
    for removed in ["system", "metadata", "diagnostics", "context_management"] {
        assert!(filtered.get(removed).is_none(), "{removed} was reinjected");
    }
    assert!(filtered["thinking"].get("display").is_none());
    let defaults = r#"
requests:
  payload:
    default-raw:
      - models: [{name: '*', protocol: claude}]
        params:
          diagnostics: '{"operator":true}'
          context_management: '{"edits":[]}'
"#;
    let absent = prepare(initial.clone(), defaults).await;
    assert_eq!(absent["diagnostics"], serde_json::json!({"operator":true}));
    assert_eq!(absent["context_management"], serde_json::json!({"edits":[]}));
    let mut explicit = initial;
    // Probes keep caller diagnostics; ordinary cloaked turns own continuity.
    explicit["max_tokens"] = 1.into();
    explicit["messages"] = serde_json::json!([{"role":"user","content":"quota"}]);
    explicit.as_object_mut().unwrap().remove("thinking");
    explicit["diagnostics"] = serde_json::json!({"caller":"preserved"});
    explicit["context_management"] = serde_json::json!({"edits":[{"type":"caller-extension"}]});
    let preserved = prepare(explicit.clone(), defaults).await;
    assert_eq!(preserved["diagnostics"], explicit["diagnostics"]);
    assert_eq!(preserved["context_management"], explicit["context_management"]);
    let conditions = prepare(serde_json::json!({"model":"claude-sonnet-4-6","max_tokens":128,"messages":[{"role":"user","content":"Do not replay rules"}]}), r#"
requests:
  payload:
    override:
      - models: [{name: '*', protocol: claude, exist: [metadata.user_id]}]
        params: {observed_generated_identity: true}
    filter:
      - models: [{name: '*', protocol: claude, exist: [metadata.user_id]}]
        params: [metadata]
"#).await;
    assert_eq!(
        conditions["observed_generated_identity"], true,
        "conditions inspect the prepared identity"
    );
    assert!(
        conditions.get("metadata").is_none(),
        "a later shaping pass must not reinject removed identity"
    );
}

/// Native search requires an enabled switch, capable model and trusted origin
/// or explicit credential capability.
#[test]
fn native_tool_search_requires_a_capable_origin_model_and_opt_in() {
    const GATEWAY: &str = "https://gateway.example.test";
    let on = "upstream:\n  claude:\n    tool-search: true\n";
    let forced =
        format!("{on}claude-api-key:\n  - api-key: k\n    base-url: {GATEWAY}\n    native-tool-search: true\n");
    type Case<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)], &'a str, bool);
    let cases: &[Case] = &[
        ("default origin", on, &[("api_key", "k")], "claude-sonnet-4-5", true),
        (
            "explicit anthropic",
            on,
            &[("api_key", "k"), ("base_url", "https://api.anthropic.com")],
            "claude-sonnet-4-5",
            true,
        ),
        (
            "anthropic with port",
            on,
            &[("api_key", "k"), ("base_url", "https://api.anthropic.com:443/")],
            "claude-sonnet-4-5",
            true,
        ),
        (
            "plain http anthropic",
            on,
            &[("api_key", "k"), ("base_url", "http://api.anthropic.com")],
            "claude-sonnet-4-5",
            false,
        ),
        (
            "gateway",
            on,
            &[("api_key", "k"), ("base_url", GATEWAY)],
            "claude-sonnet-4-5",
            false,
        ),
        (
            "gateway forced by attribute",
            on,
            &[("api_key", "k"), ("base_url", GATEWAY), ("native_tool_search", "TRUE")],
            "claude-sonnet-4-5",
            true,
        ),
        (
            "gateway forced by config",
            &forced,
            &[("api_key", "k"), ("base_url", GATEWAY)],
            "claude-sonnet-4-5",
            true,
        ),
        (
            "legacy model on anthropic",
            on,
            &[("api_key", "k")],
            "claude-opus-4-1-20250805",
            false,
        ),
        (
            "legacy model on forced gateway",
            on,
            &[("api_key", "k"), ("base_url", GATEWAY), ("native_tool_search", "true")],
            "claude-3-7-sonnet-20250219",
            false,
        ),
        ("switch off", "", &[("api_key", "k")], "claude-sonnet-4-5", false),
    ];
    for (name, cfg, attrs, model, want) in cases {
        let mut credential = harness_credential();
        credential.attributes = attrs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        let supports = native_tool_search(&Config::parse(cfg).unwrap());
        assert_eq!(supports(&credential, model), *want, "{name}");
    }
}

/// Responses clients receive native search calls only when the upstream credential
/// supports them; fallback tools remain ordinary function calls. The peer speaks the
/// Anthropic Messages SSE event schema, independently of any recorder output.
#[tokio::test]
async fn responses_tool_search_preserves_call_arguments_and_respects_upstream_capability() {
    for native in [false, true] {
        for stream in [false, true] {
            let reply = [
                serde_json::json!({"type":"message_start","message":{"id":"msg_search","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[],"usage":{"input_tokens":3,"output_tokens":0}}}),
                serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_search","name":"tool_search","input":{}}}),
                serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":r#"{"query":"weather","limit":2}"#}}),
                serde_json::json!({"type":"content_block_stop","index":0}),
                serde_json::json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4}}),
                serde_json::json!({"type":"message_stop"}),
            ].into_iter().map(|event| format!("event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap())).collect::<String>();
            let seen = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
            let captured = seen.clone();
            let router = axum::Router::new().fallback(move |body: Bytes| {
                captured.lock().unwrap().push(serde_json::from_slice(&body).unwrap());
                let reply = reply.clone();
                async move { ([(http::header::CONTENT_TYPE, "text/event-stream")], reply) }
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            let dir = std::env::temp_dir().join(format!("cpa-tool-search-{}", uuid::Uuid::new_v4()));
            let cfg = Config::parse(&format!("config-version: 8\noauth:\n  auth-dir: {}\nupstream:\n  claude:\n    tool-search: true\napi-keys:\n  claude:\n    - base-url: {base}\n      keys:\n        - api-key: test-upstream-key\n          native-tool-search: {native}\n", dir.display())).unwrap();
            let credential = cpa_core::config::credentials::load(&cfg)
                .into_iter()
                .find(|c| c.provider == "claude")
                .unwrap();
            let body = Bytes::from(serde_json::json!({
                "model":"claude-sonnet-4-6", "stream":stream,
                "tools":[{"type":"function","name":"shell","parameters":{"type":"object"}}, {"type":"tool_search","execution":"client","parameters":{"type":"object"}}],
                "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"Weather?"}]}, {"type":"tool_search_call","call_id":"previous_search","execution":"client","arguments":{"query":"weather"}}, {"type":"tool_search_output","call_id":"previous_search","execution":"client","tools":[{"type":"function","name":"get_weather","parameters":{"type":"object"},"defer_loading":true}]}, {"type":"message","role":"user","content":[{"type":"input_text","text":"Paris"}]}]
            }).to_string());
            let mut req = client_request(String::from_utf8(body.to_vec()).unwrap(), Default::default());
            req.source_format = Format::OpenAIResponse;
            req.response_format = Format::OpenAIResponse;
            req.model = "claude-sonnet-4-6".into();
            req.requested_model = req.model.clone();
            req.stream = stream;
            let client = wreq::Client::builder().no_proxy().build().unwrap();
            let executor = ClaudeExecutor::with_client(client, DEFAULT_BASE_URL);
            let output = executor.execute(&credential, req, &cfg).await.unwrap();
            let result: Value = match output.body {
                ResponseBody::Buffered(b) => serde_json::from_slice(&b).unwrap(),
                ResponseBody::Stream(events) => {
                    let chunks: Vec<_> = events.map(|c| c.unwrap()).collect().await;
                    let text = String::from_utf8(chunks.concat()).unwrap();
                    let events: Vec<Value> = text
                        .lines()
                        .filter_map(|l| l.strip_prefix("data: "))
                        .filter_map(|l| serde_json::from_str(l).ok())
                        .collect();
                    let done = events
                        .iter()
                        .find(|e| e["type"] == "response.completed")
                        .expect("completed event");
                    assert!(events.iter().any(|e| e["type"] == "response.output_item.done"));
                    done["response"].clone()
                }
            };
            assert_eq!(result["status"], "completed");
            assert_eq!(result["usage"]["input_tokens"], 3);
            assert_eq!(result["usage"]["output_tokens"], 4);
            let item = &result["output"][0];
            assert_eq!(item["call_id"], "call_search");
            assert_eq!(item["type"], if native { "tool_search_call" } else { "function_call" });
            let arguments = if native {
                item["arguments"].clone()
            } else {
                serde_json::from_str(item["arguments"].as_str().unwrap()).unwrap()
            };
            assert_eq!(arguments, serde_json::json!({"query":"weather","limit":2}));
            let requests = seen.lock().unwrap();
            assert_eq!(requests.len(), 1);
            let upstream = &requests[0];
            assert!(upstream.to_string().contains("Weather?"));
            assert!(upstream.to_string().contains("Paris"));
            let tools = upstream["tools"].as_array().unwrap();
            assert!(tools.iter().any(|t| t["name"] == "shell"));
            assert_eq!(tools.iter().any(|t| t["name"] == "tool_search"), native);
            assert_eq!(tools.iter().any(|t| t["name"] == "get_weather"), native);
            assert_eq!(upstream.to_string().contains("tool_reference"), native);
            drop(requests);
            server.abort();
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

async fn recording_upstream() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    use axum::response::IntoResponse;
    let sent = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = sent.clone();
    let router = axum::Router::new().fallback(move |body: String| {
        let seen = seen.clone();
        async move {
            let stream = rawjson::get(&body, "stream").bool();
            let id = format!("msg_recorded_{}", {
                let mut seen = seen.lock().unwrap();
                seen.push(body);
                seen.len()
            });
            let message = serde_json::json!({"id": id, "type": "message", "role": "assistant",
                "model": "claude-opus-5-5", "content": [], "usage": {"input_tokens": 3, "output_tokens": 0}});
            if !stream {
                return ([(http::header::CONTENT_TYPE, "application/json")], message.to_string()).into_response();
            }
            let events = [
                serde_json::json!({"type": "message_start", "message": message}),
                serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}}),
                serde_json::json!({"type": "message_stop"}),
            ];
            let sse: String = events
                .iter()
                .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
                .collect();
            ([(http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (base, sent)
}

/// A native Messages request from a coding client, with the session the server
/// derives for it.
fn client_request(body: String, headers: http::HeaderMap) -> ExecRequest {
    let model = rawjson::string(&body, "model");
    let stream = rawjson::get(&body, "stream").bool();
    let body = Bytes::from(body);
    enrich(ExecRequest {
        operation: Operation::Generate,
        source_format: Format::Claude,
        response_format: Format::Claude,
        requested_model: model.clone(),
        model,
        original_body: body.clone(),
        body,
        stream,
        alt: None,
        session: None,
        execution_session: None,
        derived_session: None,
        resolved_model: None,
        usage: Default::default(),
        request_path: String::new(),
        headers,
        caller: Caller {
            principal: "agent-client-key".into(),
            source: "authorization",
            provider: String::new(),
        },
    })
}

/// Sends `req` and reads the whole answer, so the turn is committed.
async fn drain(executor: &ClaudeExecutor, credential: &Credential, cfg: &Config, req: ExecRequest) {
    let response = executor.execute(credential, req, cfg).await.unwrap();
    if let ResponseBody::Stream(mut events) = response.body {
        while let Some(event) = events.next().await {
            event.unwrap();
        }
    }
}

/// The date in the currentDate reminder of the first user message.
fn reminder_date(body: &str) -> String {
    let reminder = rawjson::string(body, "messages.0.content.0.text");
    reminder.split("Today's date is ").nth(1).unwrap()[..10].to_owned()
}

/// A coding client streams one long cloaked conversation through a Claude OAuth
/// login. What Anthropic's prompt cache keys on stays byte for byte the same:
///
/// - warm: the next turn repeats the previous request's prefix;
/// - changed context: an edited caller system prompt changes the request only from
///   the turn it is moved to, after the first user message;
/// - idle: a turn 45 minutes later, past local midnight, repeats the prefix too, and
///   every breakpoint has the 1-hour TTL that outlives the gap. The currentDate
///   reminder sits in the first user message; it keeps the session's first date (Go
///   v8.0.27, 6d57ac9), and only a new conversation starts on the new date.
#[tokio::test]
async fn cloaked_conversation_prefix_holds_for_warm_changed_and_idle_turns() {
    let (base, sent) = recording_upstream().await;
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let mut credential = harness_credential();
    credential.id = "midnight.json".into();
    credential.attributes.insert("base_url".into(), base);
    credential.attributes.insert("timezone".into(), "UTC".into());
    let cfg = Config::parse("").unwrap();
    let tools = r#"[{"name":"Read","description":"Read a file","input_schema":{"type":"object"}}]"#;
    let system = r#"[{"type":"text","text":"You are a coding agent. Project rules: keep diffs small."}]"#;
    let first = r#"{"role":"user","content":[{"type":"text","text":"Port the cache fixes, then report."}]}"#;
    let edited =
        r#"[{"type":"text","text":"You are a coding agent. Project rules: keep diffs small. Run the tests."}]"#;
    let request = |system: &str, messages: &str| {
        format!(
            r#"{{"model":"claude-opus-5-5","max_tokens":64,"stream":true,"system":{system},"tools":{tools},"messages":[{messages}]}}"#
        )
    };
    let turn = |messages: &str| request(system, messages);
    let send = |body: String| client_request(body, Default::default());
    let run = |req: ExecRequest| drain(&executor, &credential, &cfg, req);
    let turn_two = format!(
        r#"{first},{{"role":"assistant","content":[{{"type":"text","text":"Done with the first half."}}]}},{{"role":"user","content":[{{"type":"text","text":"Now the second half."}}]}}"#
    );
    let turn_three = format!(
        r#"{turn_two},{{"role":"assistant","content":[{{"type":"text","text":"Second half done."}}]}},{{"role":"user","content":[{{"type":"text","text":"Report."}}]}}"#
    );
    let _pins = Pins;
    set_clock(Some("2026-10-13T23:59:30Z"));
    run(send(turn(first))).await;
    set_clock(Some("2026-10-13T23:59:40Z"));
    run(send(turn(&turn_two))).await;
    set_clock(Some("2026-10-13T23:59:50Z"));
    run(send(request(edited, &turn_two))).await;
    set_clock(Some("2026-10-14T00:45:00Z"));
    run(send(turn(&turn_three))).await;
    let fresh = r#"{"role":"user","content":[{"type":"text","text":"A new conversation."}]}"#;
    run(send(turn(fresh))).await;

    let sent = sent.lock().unwrap();
    let [cold, warm, changed, idle, fresh] = &sent[..] else {
        panic!("{} requests", sent.len());
    };
    assert_eq!(
        [cold, warm, changed, idle, fresh].map(|b| reminder_date(b)),
        ["2026-10-13", "2026-10-13", "2026-10-13", "2026-10-13", "2026-10-14"]
    );
    // The selected content the cache is keyed on, in Anthropic's order (tools, system,
    // messages), up to and including message `upto`. The billing block (system.0)
    // carries a per-request signature, as in native Claude Code; that Anthropic leaves
    // it out of the key is an upstream assumption requiring live cache-read
    // verification. The rolling breakpoint on the last message moves on with each
    // turn, so markers after the first two messages are left out here and checked by
    // path below.
    const ROLLING: &str = r#","cache_control":{"type":"ephemeral","ttl":"1h"}"#;
    let prefix = |body: &str, upto: usize| {
        assert!(rawjson::string(body, "system.0.text").starts_with(cloak::BILLING_PREFIX));
        let mut out = rawjson::get(body, "tools").json().to_owned();
        for block in rawjson::get(body, "system").array().iter().skip(1) {
            out.push_str(block.json());
        }
        let messages = rawjson::get(body, "messages");
        for (i, message) in messages.array().iter().enumerate().take(upto + 1) {
            out.push_str(&if i < 2 {
                message.json().to_owned()
            } else {
                message.json().replace(ROLLING, "")
            });
        }
        out
    };
    let messages = |body: &str| rawjson::get(body, "messages").array().len();
    // Warm: the whole cold request (first user message, then the caller's system turn).
    assert_eq!(messages(cold), 2);
    assert_eq!(prefix(warm, 1), prefix(cold, 1));
    // Changed context: equal through the first user message, different from the
    // relocated system turn on.
    assert_eq!(prefix(changed, 0), prefix(warm, 0));
    assert_ne!(prefix(changed, 1), prefix(warm, 1));
    // Idle: the whole warm request, past midnight.
    assert_eq!(messages(warm), 4);
    assert_eq!(prefix(idle, 3), prefix(warm, 3));
    // The breakpoints: the Claude Code identity block, the first user text, the caller
    // prompt's turn and the rolling last turn, each in the 1-hour pool (an OAuth login
    // selects it, as native Claude Code does), so none lapses after 5 minutes.
    let breakpoints = |body: &str| -> Vec<String> {
        signals::blocks(body)
            .into_iter()
            .filter(|(_, block)| rawjson::get(block, "cache_control").exists())
            .map(|(path, block)| {
                assert_eq!(
                    rawjson::get(&block, "cache_control").json(),
                    r#"{"type":"ephemeral","ttl":"1h"}"#,
                    "{path}"
                );
                path
            })
            .collect()
    };
    let fixed = ["system.1", "messages.0.content.1", "messages.1.content.0"];
    let with = |last: &'static str| [&fixed[..], &[last]].concat();
    assert_eq!(breakpoints(cold), fixed);
    assert_eq!(breakpoints(warm), with("messages.3.content.0"));
    assert_eq!(breakpoints(changed), with("messages.3.content.0"));
    assert_eq!(breakpoints(idle), with("messages.5.content.0"));
    assert_eq!(breakpoints(fresh), fixed);
}

/// A cloaked API key keeps the caller's system prompt where the next turn finds it:
/// in a system turn after the first user message, on the first turn and every later
/// one, as for an OAuth login. Go v8.0.27 keeps it top-level while that user message
/// ends the conversation and moves it on the second turn, which then misses the cache
/// from the Claude Code identity block on (docs/DIFFERENCES-FROM-GO.md).
#[tokio::test]
async fn api_key_cloak_keeps_the_caller_prompt_in_place_across_turns() {
    let (base, sent) = recording_upstream().await;
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let mut credential = Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/claude-api-key.json"),
        serde_json::json!({"type": "claude"}).as_object().unwrap().clone(),
    )
    .unwrap();
    for (key, value) in [
        ("api_key", "fake-gateway-key"),
        ("base_url", &base),
        ("cloak_mode", "always"),
    ] {
        credential.attributes.insert(key.into(), value.into());
    }
    let cfg = Config::parse("").unwrap();
    let first = r#"{"role":"user","content":"Port the cache fixes."}"#;
    let second =
        format!(r#"{first},{{"role":"assistant","content":"Half done."}},{{"role":"user","content":"Go on."}}"#);
    let third = format!(r#"{second},{{"role":"assistant","content":"Done."}},{{"role":"user","content":"Report."}}"#);
    for messages in [first, &second, &third] {
        let body = format!(
            r#"{{"model":"claude-opus-5-5","max_tokens":64,"system":"You are a coding agent.","messages":[{messages}]}}"#
        );
        drain(&executor, &credential, &cfg, client_request(body, Default::default())).await;
    }
    let sent = sent.lock().unwrap();
    let [one, two, three] = &sent[..] else {
        panic!("{} requests", sent.len());
    };
    // The system blocks, the first user message and the caller's system turn.
    let prefix = |body: &str| {
        let mut out = rawjson::get(body, "system").json().to_owned();
        for message in rawjson::get(body, "messages").array().iter().take(2) {
            out.push_str(message.json());
        }
        out
    };
    assert_eq!(prefix(two), prefix(one));
    assert_eq!(prefix(three), prefix(one));
    assert_eq!(
        rawjson::get(one, "system").array().len(),
        2,
        "billing and identity only"
    );
    assert_eq!(rawjson::get(one, "messages.1.role").str(), "system");
    assert_eq!(
        rawjson::string(one, "messages.1.content.0.text"),
        "You are a coding agent."
    );
}

/// Failed tool restoration must not publish successful token usage, even after
/// receiving the upstream body.
#[tokio::test]
async fn unrestorable_tool_alias_publishes_no_tokens() {
    use axum::response::IntoResponse;
    let router = axum::Router::new().fallback(|body: String| async move {
        // The client tool `other` went upstream as mcp__<virtual server>__<word>_other;
        // `query` under that server matches both passthrough MCP tools.
        let sent: Value = serde_json::from_str(&body).unwrap();
        let alias = sent["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .find(|n| n.ends_with("_other"))
            .unwrap()
            .to_owned();
        let server = alias.split("__").nth(1).unwrap();
        let reply = serde_json::json!({
            "id": "msg_r", "type": "message", "role": "assistant", "model": "claude-upstream",
            "content": [{"type": "tool_use", "id": "toolu_1", "name": format!("mcp__{server}__query"), "input": {}}],
            "stop_reason": "tool_use", "usage": {"input_tokens": 11, "output_tokens": 3},
        });
        ([(http::header::CONTENT_TYPE, "application/json")], reply.to_string()).into_response()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let mut credential = harness_credential();
    credential.attributes.insert("base_url".into(), base);
    let body = Bytes::from_static(
        br#"{"model":"claude-sonnet-4-6","max_tokens":8,"messages":[{"role":"user","content":"hi"}],"tools":[{"name":"mcp__srv1__query","input_schema":{"type":"object"}},{"name":"mcp__srv2__query","input_schema":{"type":"object"}},{"name":"other","input_schema":{"type":"object"}}]}"#,
    );
    let usage = Arc::new(Usage::default());
    let req = enrich(ExecRequest {
        operation: Operation::Generate,
        source_format: Format::Claude,
        response_format: Format::Claude,
        requested_model: "claude-sonnet-4-6".into(),
        model: "claude-sonnet-4-6".into(),
        original_body: body.clone(),
        body,
        stream: false,
        alt: None,
        session: None,
        execution_session: None,
        derived_session: None,
        resolved_model: None,
        usage: cpa_core::exec::UsageSink::new(usage.clone()),
        request_path: String::new(),
        headers: Default::default(),
        caller: Caller {
            principal: "fake-client".into(),
            source: "authorization",
            provider: String::new(),
        },
    });
    let cfg = Config::parse("").unwrap();
    let error = executor.execute(&credential, req, &cfg).await.err().unwrap();
    let message = String::from_utf8_lossy(&error.body).into_owned();
    assert!(
        message.starts_with("restore Claude OAuth tool name from response: "),
        "{message}"
    );
    assert_eq!(
        usage.kinds(),
        ["request", "round_trip_started", "first_byte", "body", "publish_failure"]
    );
    let reports = usage.0.lock().unwrap();
    assert_eq!(
        reports.last().unwrap().2,
        format!("0 {message}"),
        "restoration failed before an HTTP response could be delivered"
    );
}

/// Usage events bracket the round trip and publish only observed tokens.
/// Compaction publishes the summary turn's usage.
#[tokio::test]
async fn usage_reports_bracket_requests_and_publish_only_observed_usage() {
    use axum::response::IntoResponse;
    let router = axum::Router::new().fallback(|body: String| async move {
        let model = rawjson::get(&body, "model").str().to_owned();
        if rawjson::get(&body, "stream").bool() {
            let events = format!(
                "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_u\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"{model}\",\"content\":[],\"usage\":{{\"input_tokens\":3,\"output_tokens\":1}}}}}}\n\n\
                 event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":2}}}}\n\n\
                 event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
            );
            ([(http::header::CONTENT_TYPE, "text/event-stream")], events).into_response()
        } else {
            let reply = format!(
                r#"{{"id":"msg_u","type":"message","role":"assistant","model":"{model}","content":[{{"type":"text","text":"ok"}}],"stop_reason":"end_turn","usage":{{"input_tokens":3,"output_tokens":2}}}}"#
            );
            ([(http::header::CONTENT_TYPE, "application/json")], reply).into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let mut credential = Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/claude.json"),
        serde_json::json!({"type":"claude"}).as_object().unwrap().clone(),
    )
    .unwrap();
    credential
        .attributes
        .insert("api_key".into(), "fake-gateway-key".into());
    credential.attributes.insert("base_url".into(), base);
    let cfg = Config::parse("").unwrap();
    let run = |source: Format, stream: bool, alt: Option<&str>| {
        let usage = Arc::new(Usage::default());
        let body = if source == Format::Claude {
            Bytes::from_static(
                br#"{"model":"claude-sonnet-4-6","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}"#,
            )
        } else {
            Bytes::from_static(br#"{"model":"claude-sonnet-4-6","messages":[{"role":"user","content":"hi"}]}"#)
        };
        let req = ExecRequest {
            operation: Operation::Generate,
            source_format: source,
            response_format: source,
            requested_model: "claude-sonnet-4-6".into(),
            model: "claude-sonnet-4-6".into(),
            original_body: body.clone(),
            body,
            stream,
            alt: alt.map(str::to_owned),
            session: None,
            execution_session: None,
            derived_session: None,
            resolved_model: None,
            usage: cpa_core::exec::UsageSink::new(usage.clone()),
            request_path: String::new(),
            headers: Default::default(),
            caller: Caller {
                principal: "fake-client".into(),
                source: "x-api-key",
                provider: String::new(),
            },
        };
        let (executor, credential, cfg) = (&executor, &credential, &cfg);
        async move {
            let response = executor.execute(credential, req, cfg).await;
            if let Ok(ExecResponse {
                body: ResponseBody::Stream(events),
                ..
            }) = response
            {
                let _: Vec<_> = events.collect().await;
            }
            usage
        }
    };
    let without_lines = |usage: &Usage| usage.kinds().into_iter().filter(|k| *k != "line").collect::<Vec<_>>();
    // Native Execute: reporter.Publish(ParseClaudeUsage(data)).
    let usage = run(Format::Claude, false, None).await;
    assert_eq!(
        usage.kinds(),
        ["request", "round_trip_started", "first_byte", "body", "publish"]
    );
    // Translated Execute: streamUsage.Publish only, so usage is required.
    let usage = run(Format::OpenAI, false, None).await;
    assert_eq!(
        without_lines(&usage),
        ["request", "round_trip_started", "first_byte", "usage_required"]
    );
    assert!(usage.kinds().contains(&"line"));
    // ExecuteStream, native and translated: deferred streamUsage.Publish only.
    for source in [Format::Claude, Format::OpenAI] {
        let usage = run(source, true, None).await;
        assert_eq!(
            without_lines(&usage),
            ["request", "round_trip_started", "first_byte", "usage_required"],
            "{source:?}"
        );
    }
    // responses/compact: executeClaudeCompaction returns before its own reporter and
    // the summary turn publishes like a native Execute.
    let usage = run(Format::OpenAIResponse, false, Some("responses/compact")).await;
    assert_eq!(
        usage.kinds(),
        ["request", "round_trip_started", "first_byte", "body", "publish"]
    );
}

/// Owned copies of the capture events an executor records.
#[derive(Default)]
pub(super) struct Captured(pub std::sync::Mutex<Vec<String>>);

impl cpa_core::exec::CaptureObserver for Captured {
    fn record(&self, event: cpa_core::exec::CaptureEvent<'_>, _: &cpa_core::exec::SentCredentials) {
        use cpa_core::exec::CaptureEvent::*;
        let line = match event {
            Request(r) => {
                let names: Vec<&str> = r.headers.iter().map(|(k, _)| k.as_str()).collect();
                format!(
                    "request {} {} provider={} id={} label={} type={} value={} body={} headers={}",
                    r.method,
                    r.url.split_once("//").unwrap().1.split_once('/').unwrap().1,
                    r.provider,
                    r.auth_id,
                    r.auth_label,
                    r.auth_type,
                    r.auth_value,
                    !r.body.is_empty(),
                    names.join(","),
                )
            }
            ResponseMetadata(status, headers) => {
                let names: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
                format!("metadata {status} {}", names.join(","))
            }
            ResponseChunk(chunk) => format!("chunk {}", String::from_utf8_lossy(chunk)),
            ResponseError(error) => format!("error {error}"),
            _ => "websocket".into(),
        };
        self.0.lock().unwrap().push(line);
    }
}

/// Capture requests before sending, response metadata before body bytes, and
/// transport errors through the shared capture observer.
#[tokio::test]
async fn capture_events_cover_requests_responses_and_transport_failures() {
    use axum::response::IntoResponse;
    let wire: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let seen = wire.clone();
    let router = axum::Router::new().fallback(move |uri: http::Uri, sent: http::HeaderMap, body: String| async move {
        *seen.lock().unwrap() = sent.keys().map(|k| k.as_str().to_owned()).collect();
        let mut headers = http::HeaderMap::new();
        headers.insert(http::header::CONTENT_TYPE, "application/json".parse().unwrap());
        headers.append("x-dup", "one".parse().unwrap());
        headers.append("x-dup", "two".parse().unwrap());
        if uri.path().ends_with("count_tokens") {
            return (headers, r#"{"input_tokens":4}"#.to_owned()).into_response();
        }
        if body.contains("rate me") {
            return (
                http::StatusCode::TOO_MANY_REQUESTS,
                headers,
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow"}}"#.to_owned(),
            )
                .into_response();
        }
        if rawjson::get(&body, "stream").bool() {
            let events = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"usage\":{\"input_tokens\":1}}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
            return ([(http::header::CONTENT_TYPE, "text/event-stream")], events.to_owned()).into_response();
        }
        (headers, r#"{"id":"m","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#.to_owned()).into_response()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let closed = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", probe.local_addr().unwrap())
    };
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let credential = |base_url: &str| {
        let mut c = Credential::from_file(
            Path::new("/fake"),
            Path::new("/fake/claude.json"),
            serde_json::json!({"type":"claude"}).as_object().unwrap().clone(),
        )
        .unwrap();
        c.label = "fixture-label".into();
        c.attributes.insert("api_key".into(), " fake-gateway-key ".into());
        c.attributes.insert("base_url".into(), base_url.to_owned());
        c
    };
    let cfg = Config::parse("").unwrap();
    let run = |base_url: String, text: &'static str, operation: Operation, stream: bool| {
        let captured = Arc::new(Captured::default());
        let body = Bytes::from(format!(
            r#"{{"model":"claude-sonnet-4-6","max_tokens":8,"stream":{stream},"messages":[{{"role":"user","content":"{text}"}}]}}"#
        ));
        let req = ExecRequest {
            operation,
            source_format: Format::Claude,
            response_format: Format::Claude,
            requested_model: "claude-sonnet-4-6".into(),
            model: "claude-sonnet-4-6".into(),
            original_body: body.clone(),
            body,
            stream,
            alt: None,
            session: None,
            execution_session: None,
            derived_session: None,
            resolved_model: None,
            usage: cpa_core::exec::UsageSink::default()
                .with_capture(cpa_core::exec::CaptureSink::new(captured.clone())),
            request_path: String::new(),
            headers: Default::default(),
            caller: Caller {
                principal: "fake-client".into(),
                source: "x-api-key",
                provider: String::new(),
            },
        };
        let (executor, cfg) = (&executor, &cfg);
        async move {
            let credential = credential(&base_url);
            if let Ok(ExecResponse {
                body: ResponseBody::Stream(events),
                ..
            }) = executor.execute(&credential, req, cfg).await
            {
                let _: Vec<_> = events.collect().await;
            }
            std::mem::take(&mut *captured.0.lock().unwrap())
        }
    };
    let request = |path: &str, provider: &str| {
        format!(
            "request POST {path}?beta=true provider={provider} id=claude.json label=fixture-label type=api_key value=fake-gateway-key body=true headers="
        )
    };
    // The logged headers are exactly the executor's own headers on the wire: everything
    // the upstream received except what the transport adds (Host, Content-Length).
    let logged_match_wire = |event: &str| {
        let mut logged: Vec<String> = event
            .rsplit_once("headers=")
            .unwrap()
            .1
            .split(',')
            .map(str::to_ascii_lowercase)
            .collect();
        let mut sent: Vec<String> = wire
            .lock()
            .unwrap()
            .iter()
            .filter(|k| !matches!(k.as_str(), "host" | "content-length"))
            .cloned()
            .collect();
        logged.sort();
        sent.sort();
        assert_eq!(logged, sent, "{event}");
    };
    let metadata = "metadata 200 Content-Type,X-Dup,X-Dup,Content-Length,Date";
    // Execute: the whole decoded body as one chunk.
    let events = run(base.clone(), "hi", Operation::Generate, false).await;
    assert_eq!(events.len(), 3, "{events:?}");
    assert!(events[0].starts_with(&request("v1/messages", "claude")), "{events:?}");
    logged_match_wire(&events[0]);
    assert_eq!(events[1], metadata);
    assert!(events[2].starts_with(r#"chunk {"id":"m""#), "{events:?}");
    // ExecuteStream: every scanner line, blank lines included, up to message_stop.
    let events = run(base.clone(), "hi", Operation::Generate, true).await;
    assert!(events[0].starts_with(&request("v1/messages", "claude")), "{events:?}");
    logged_match_wire(&events[0]);
    assert!(events[1].starts_with("metadata 200 Content-Type"), "{events:?}");
    assert_eq!(
        events[2..],
        [
            "chunk event: message_start",
            r#"chunk data: {"type":"message_start","message":{"id":"m","usage":{"input_tokens":1}}}"#,
            "chunk ",
            "chunk event: message_stop",
            r#"chunk data: {"type":"message_stop"}"#,
            "chunk ",
        ]
    );
    // An upstream error: its metadata, then the error body.
    let events = run(base.clone(), "rate me", Operation::Generate, false).await;
    assert_eq!(events[1], metadata.replace("200", "429"));
    assert!(events[2].starts_with(r#"chunk {"type":"error""#), "{events:?}");
    // A transport failure: the request, then the error.
    let events = run(closed, "hi", Operation::Generate, false).await;
    assert_eq!(events.len(), 2, "{events:?}");
    assert!(events[1].starts_with("error "), "{events:?}");
}

/// A compatibility gateway may omit thinking blocks when it returns tool history.
/// Store them after success, restore only on matching history, retain them after local
/// validation failure, and clear an applied replay after an upstream rejection.
#[tokio::test]
async fn compat_replay_restores_history_and_only_upstream_rejections_clear_it() {
    use axum::response::IntoResponse;
    let planning = serde_json::json!({
        "id":"msg_plan", "type":"message", "role":"assistant", "model":"claude-sonnet-4-6",
        "content":[{"type":"thinking","thinking":"private plan","signature":"sig-private-plan"},
            {"type":"text","text":"Calling."},{"type":"tool_use","id":"tool_local","name":"read","input":{"path":"a"}}],
        "stop_reason":"tool_use","usage":{"input_tokens":3,"output_tokens":1}
    })
    .to_string();
    let finished = serde_json::json!({
        "id":"msg_done","type":"message","role":"assistant","model":"claude-sonnet-4-6",
        "content":[{"type":"text","text":"done"}],"stop_reason":"end_turn",
        "usage":{"input_tokens":3,"output_tokens":1}
    })
    .to_string();
    let rejected = serde_json::json!({"type":"error","error":{"type":"invalid_request_error","message":"thinking signature rejected"}}).to_string();
    let replies = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from([
        (200, planning.clone()),
        (200, finished.clone()),
        (200, planning),
        (400, rejected),
        (200, finished),
    ])));
    let captured: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let seen = captured.clone();
    let router = axum::Router::new().fallback(move |body: String| {
        let (replies, seen) = (replies.clone(), seen.clone());
        async move {
            seen.lock().unwrap().push(body);
            let (status, reply) = replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected upstream request");
            (
                http::StatusCode::from_u16(status).unwrap(),
                [(http::header::CONTENT_TYPE, "application/json")],
                reply,
            )
                .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let root = std::env::temp_dir().join(format!("cpa-claude-contract-replay-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("auth")).unwrap();
    let executor = ClaudeExecutor::with_client(wreq::Client::builder().no_proxy().build().unwrap(), DEFAULT_BASE_URL);
    let usage = Arc::new(Usage::default());
    let first =
        serde_json::json!({"model":"claude-sonnet-4-6","max_tokens":64,"messages":[{"role":"user","content":"hello"}]});
    let continuation = serde_json::json!({"model":"claude-sonnet-4-6","max_tokens":64,"messages":[
        {"role":"user","content":"hello"},
        {"role":"assistant","content":[{"type":"text","text":"Calling."},{"type":"tool_use","id":"tool_local","name":"read","input":{"path":"a"}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"tool_local","content":"result"}]}
    ]});
    for step in 0..6 {
        let cloak = if step >= 2 {
            "\n          cloak:\n            mode: always"
        } else {
            ""
        };
        let cfg = Config::parse(&format!("config-version: 8\noauth:\n  auth-dir: {}\napi-keys:\n  claude:\n    - base-url: {base}\n      keys:\n        - api-key: gateway-local-only{cloak}\n", root.join("auth").display())).unwrap();
        let credential = cpa_core::config::credentials::load(&cfg)
            .into_iter()
            .find(|c| c.provider == "claude")
            .unwrap();
        let mut body = if matches!(step, 0 | 2) {
            first.clone()
        } else {
            continuation.clone()
        };
        if step == 3 {
            body["system"] =
                serde_json::json!([{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}}]);
        }
        let mut headers = http::HeaderMap::new();
        headers.insert(
            "x-claude-code-session-id",
            "5a1f3c2e-1111-4222-8333-444455556666".parse().unwrap(),
        );
        let mut req = enrich(client_request(body.to_string(), headers));
        req.resolved_model = Some(cpa_core::exec::ResolvedModel {
            info: cpa_core::registry::ModelInfo::from_raw(
                serde_json::json!({"id":"claude-sonnet-4-6","is_compat":true})
                    .as_object()
                    .unwrap()
                    .clone(),
            )
            .unwrap(),
            source: cpa_core::exec::ResolvedSource::ApiKey,
        });
        req.usage = cpa_core::exec::UsageSink::new(usage.clone());
        usage.0.lock().unwrap().clear();
        let before = captured.lock().unwrap().len();
        let result = executor.execute(&credential, req, &cfg).await;
        if matches!(step, 3 | 4) {
            assert_eq!(
                result.err().expect("expected request rejection").status,
                400,
                "step {step}"
            );
        } else {
            assert_eq!(result.unwrap().status, 200, "step {step}");
        }
        let sent = captured.lock().unwrap().get(before).cloned();
        assert_eq!(
            sent.is_some(),
            step != 3,
            "local validation must precede any upstream request"
        );
        if let Some(sent) = &sent {
            assert_eq!(
                sent.contains("sig-private-plan"),
                matches!(step, 1 | 4),
                "step {step}: replay eligibility"
            );
            let parsed: Value = serde_json::from_str(sent).unwrap();
            assert_eq!(parsed["max_tokens"], 64);
            assert!(sent.contains("hello"));
            if !matches!(step, 0 | 2) {
                assert!(sent.contains("tool_local"));
                assert!(sent.contains("result"));
            }
        }
        let reports = usage.0.lock().unwrap().clone();
        if let Some(sent) = sent {
            assert_eq!(reports[0], ("request", Format::Claude, sent));
            assert_eq!(reports[1].0, "round_trip_started");
            assert_eq!(reports[2].0, "first_byte");
            assert_eq!(reports.iter().any(|(kind, _, _)| *kind == "publish"), step != 4);
        } else {
            assert!(reports.is_empty());
        }
    }
    assert_eq!(captured.lock().unwrap().len(), 5);
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

/// Records what the executor reports to the usage queue.
#[derive(Default)]
pub(super) struct Usage(pub std::sync::Mutex<Vec<(&'static str, Format, String)>>);

impl cpa_core::exec::UsageObserver for Usage {
    fn response_body(&self, format: Format, body: &[u8]) {
        let body = String::from_utf8_lossy(body).into_owned();
        self.0.lock().unwrap().push(("body", format, body));
    }
    fn response_line(&self, format: Format, line: &[u8]) {
        let line = String::from_utf8_lossy(line).into_owned();
        self.0.lock().unwrap().push(("line", format, line));
    }
    fn request(&self, format: Format, payload: &[u8]) {
        let payload = String::from_utf8_lossy(payload).into_owned();
        self.0.lock().unwrap().push(("request", format, payload));
    }
    fn upstream_model(&self, model: &str) {
        self.push("upstream_model", model);
    }
    fn round_trip_started(&self) {
        self.push("round_trip_started", "");
    }
    fn first_byte(&self) {
        self.push("first_byte", "");
    }
    fn publish(&self) {
        self.push("publish", "");
    }
    fn publish_failure(&self, status: u16, body: &str) {
        self.push("publish_failure", &format!("{status} {body}"));
    }
    fn usage_required(&self) {
        self.push("usage_required", "");
    }
    fn discard(&self) {
        self.push("discard", "");
    }
}

impl Usage {
    fn push(&self, kind: &'static str, value: &str) {
        self.0.lock().unwrap().push((kind, Format::Claude, value.to_owned()));
    }

    fn kinds(&self) -> Vec<&'static str> {
        self.0.lock().unwrap().iter().map(|(kind, _, _)| *kind).collect()
    }
}

/// The refused-version line is written only when the baseline identity really replaces
/// the caller's: an API key without cloaking forwards the caller's own User-Agent, so
/// it never logs; an OAuth account sends the baseline identity, so it does.
#[tokio::test]
async fn refused_versions_log_only_when_the_identity_is_replaced() {
    let router = axum::Router::new().fallback(|| async {
        (
            [("content-type", "application/json")],
            r#"{"id":"msg_q","type":"message","role":"assistant","model":"m","content":[],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#,
        )
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let credential = |file: &str, meta: Value, key: Option<&str>| {
        let mut c = Credential::from_file(
            Path::new("/fake"),
            &Path::new("/fake").join(file),
            meta.as_object().unwrap().clone(),
        )
        .unwrap();
        if let Some(key) = key {
            c.attributes.insert("api_key".into(), key.into());
        }
        c.attributes.insert("base_url".into(), base.clone());
        c
    };
    let request = || {
        let body = Bytes::from_static(br#"{"model":"m","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}"#);
        let mut headers = http::HeaderMap::new();
        headers.insert("user-agent", "claude-cli/2.1.220 (external, cli)".parse().unwrap());
        ExecRequest {
            operation: Operation::Generate,
            source_format: Format::Claude,
            response_format: Format::Claude,
            requested_model: "m".into(),
            model: "m".into(),
            original_body: body.clone(),
            body,
            stream: false,
            alt: None,
            session: None,
            execution_session: None,
            derived_session: None,
            resolved_model: None,
            usage: Default::default(),
            request_path: String::new(),
            headers,
            caller: Caller {
                principal: "fake-client".into(),
                source: "x-api-key",
                provider: String::new(),
            },
        }
    };
    let cfg = Config::parse("").unwrap();
    let api_key = credential(
        "refused-api-key.json",
        serde_json::json!({"type":"claude"}),
        Some("fake-gateway-key"),
    );
    executor.execute(&api_key, request(), &cfg).await.unwrap();
    assert!(
        !profile::refused_logged(&api_key.id),
        "the caller's own User-Agent went upstream"
    );
    let mut meta = Value::Object(harness_credential().metadata.clone());
    meta["access_token"] = "sk-ant-oat01-FAKE".into();
    let oauth = credential("refused-oauth.json", meta, None);
    executor.execute(&oauth, request(), &cfg).await.unwrap();
    assert!(
        profile::refused_logged(&oauth.id),
        "the baseline identity went upstream"
    );
}

/// `upstream.claude.model-level-cooling` (Go 52d5507d) in every spelling, seen through
/// the API-key view: off, a shared-window 429 cools the credential; on, the model.
#[test]
fn shared_window_429_follows_model_level_cooling() {
    for (yaml, scope) in [
        ("{}\n", FailureScope::Credential),
        (
            "upstream: {claude: {model-level-cooling: false}}\n",
            FailureScope::Credential,
        ),
        ("upstream: {claude: {model-level-cooling: true}}\n", FailureScope::Model),
        (
            "oauth: {providers: {claude: {model-level-cooling: true}}}\n",
            FailureScope::Model,
        ),
        ("claude: {model-level-cooling: true}\n", FailureScope::Model),
    ] {
        let cfg = Config::parse(yaml).unwrap();
        let settings = settings::Settings::from_config(&cfg.for_api_key());
        let mut headers = http::HeaderMap::new();
        headers.insert("anthropic-ratelimit-unified-5h-status", "rejected".parse().unwrap());
        let error = upstream_error(
            429,
            headers,
            Bytes::from_static(b"{}"),
            false,
            settings.model_level_cooling,
        );
        assert_eq!(error.scope, scope, "{yaml}");
    }
}

/// Retry policy distinguishes a short per-minute limit from an account quota
/// window. The fixed clock makes deadlines exact without waiting for real time.
#[test]
fn per_minute_limits_use_a_single_valid_retry_hint_without_a_quota_claim() {
    let now = std::time::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let window = Duration::from_secs(5 * 3600);
    let headers = |retry: Option<&str>| {
        let mut h = http::HeaderMap::new();
        h.insert("anthropic-ratelimit-unified-reset", "1800018000".parse().unwrap());
        if let Some(retry) = retry {
            h.insert("retry-after", retry.parse().unwrap());
        }
        h
    };
    let body = |resource: &str| {
        serde_json::json!({"type":"error","error":{"type":"rate_limit_error","message":format!("This request would exceed the {resource} per minute rate limit. Please try again later.")}}).to_string()
    };
    for resource in ["requests", "input tokens", "output tokens"] {
        let message = body(resource);
        assert_eq!(
            crate::quota::reset_for_error(&headers(Some("30")), message.as_bytes(), now, Duration::ZERO),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            crate::quota::reset(&headers(Some("30")), now, Duration::ZERO),
            Some(window)
        );
    }
    let message = body("requests");
    let date = httpdate::fmt_http_date(now + Duration::from_secs(45));
    for (hint, expected) in [
        (Some("12.5"), Duration::from_millis(12_500)),
        (Some(date.as_str()), Duration::from_secs(45)),
        (Some("608400"), Duration::from_secs(608_400)),
        (Some("608401"), window),
        (Some("0"), window),
        (Some("soon"), window),
        (None, window),
    ] {
        assert_eq!(
            crate::quota::reset_for_error(&headers(hint), message.as_bytes(), now, Duration::ZERO),
            Some(expected),
            "{hint:?}"
        );
    }
    let mut repeated = headers(Some("30"));
    repeated.append("retry-after", "30".parse().unwrap());
    assert_eq!(
        crate::quota::reset_for_error(&repeated, message.as_bytes(), now, Duration::ZERO),
        Some(window)
    );
    for invalid in [
        "",
        "not json",
        "[]",
        r#"{"error":"rate_limit_error"}"#,
        r#"{"error":{"type":"rate_limit_error","message":null}}"#,
        r#"{"error":{"type":"rate_limit_error","message":7}}"#,
        r#"{"error":{"type":"api_error","message":"This request would exceed the requests per minute rate limit. Please try again later."}}"#,
        r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#,
        r#"{"error":{"type":"rate_limit_error","message":"This request would not exceed the requests per minute rate limit. Please try again later."}}"#,
        r#"{"error":{"type":"rate_limit_error","message":"This request would exceed the requests per minute rate limit. Please try again later. Your weekly quota is exhausted."}}"#,
        r#"{"error":{"type":"rate_limit_error","message":"This request would exceed the requests per minute rate limit. Please try again later."}}trailing"#,
        r#"{"error":{"type":"rate_limit_error","message":"This request would exceed the requests per minute rate limit. Please try again later."},"error":null}"#,
    ] {
        assert_eq!(
            crate::quota::reset_for_error(&headers(Some("30")), invalid.as_bytes(), now, Duration::ZERO),
            Some(window),
            "{invalid}"
        );
    }
    assert_eq!(
        crate::quota::reset_for_error(&http::HeaderMap::new(), message.as_bytes(), now, Duration::ZERO),
        None
    );
    for field in [
        "anthropic-ratelimit-unified-status",
        "anthropic-ratelimit-unified-overage-status",
        "anthropic-ratelimit-unified-7d_oi-status",
    ] {
        for value in ["", " "] {
            let mut h = headers(Some("30"));
            h.insert(
                http::HeaderName::from_bytes(field.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
            assert_eq!(
                crate::quota::reset_for_error(&h, message.as_bytes(), now, Duration::ZERO),
                Some(Duration::from_secs(30))
            );
        }
    }
}

#[test]
fn shared_quota_windows_cool_the_credential_and_model_override_remains_explicit() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    for (five, seven, expected_seconds) in [
        (Some(7200), None, 7200),
        (None, Some(86400), 86400),
        (Some(7200), Some(172800), 172800),
        (Some(90000), Some(86400), 90000),
    ] {
        let mut headers = http::HeaderMap::new();
        headers.insert("retry-after", "30".parse().unwrap());
        for (window, seconds) in [("5h", five), ("7d", seven)] {
            if let Some(seconds) = seconds {
                headers.insert(
                    http::HeaderName::from_bytes(format!("anthropic-ratelimit-unified-{window}-status").as_bytes())
                        .unwrap(),
                    "rejected".parse().unwrap(),
                );
                headers.insert(
                    http::HeaderName::from_bytes(format!("anthropic-ratelimit-unified-{window}-reset").as_bytes())
                        .unwrap(),
                    (now + seconds).to_string().parse().unwrap(),
                );
            }
        }
        assert!(crate::quota::shared_rejection(&headers));
        assert_eq!(
            crate::quota::reset(
                &headers,
                std::time::UNIX_EPOCH + Duration::from_secs(now),
                Duration::ZERO
            ),
            Some(Duration::from_secs(expected_seconds))
        );
        for fast in [false, true] {
            for model_override in [false, true] {
                let error = upstream_error(429, headers.clone(), Bytes::from_static(b"{}"), fast, model_override);
                assert_eq!(
                    error.scope,
                    if model_override && !fast {
                        FailureScope::Model
                    } else {
                        FailureScope::Credential
                    }
                );
                assert!(error.retry_after.is_some());
            }
        }
    }
    let body = Bytes::from_static(
        br#"{"error":{"type":"rate_limit_error","message":"Usage credits are required for fast mode"}}"#,
    );
    let headers = [("retry-after", "30")]
        .into_iter()
        .map(|(k, v)| (k.parse().unwrap(), v.parse().unwrap()))
        .collect::<http::HeaderMap>();
    for fast in [false, true] {
        let error = upstream_error(429, headers.clone(), body.clone(), fast, false);
        assert_eq!(error.scope, FailureScope::Request);
        assert!(error.retry_after.is_some());
    }
    let headers = [
        ("retry-after", "30"),
        ("anthropic-ratelimit-unified-status", "rejected"),
        ("anthropic-ratelimit-unified-5h-status", "allowed"),
        ("anthropic-ratelimit-unified-7d-status", "allowed"),
        ("anthropic-ratelimit-unified-overage-status", "rejected"),
    ]
    .into_iter()
    .map(|(k, v)| (k.parse().unwrap(), v.parse().unwrap()))
    .collect::<http::HeaderMap>();
    assert!(
        !crate::quota::shared_rejection(&headers),
        "overage billing refusal is not account quota exhaustion"
    );
    let error = upstream_error(429, headers, Bytes::from_static(b"{}"), false, false);
    assert_eq!(error.scope, FailureScope::Model);
    assert_eq!(error.retry_after, None);
}

/// A compaction trigger or `responses/compact` becomes one non-stream Claude summary
/// turn sealed into
/// a single compaction item; replayed CPA capsules expand, foreign ones are dropped and
/// broken ones fail with 400 before any upstream call.
#[tokio::test]
async fn responses_compaction_seals_summaries_and_validates_replayed_capsules() {
    use axum::response::IntoResponse;
    const PROMPT: &str = "Please provide a concise and comprehensive summary";
    const SUMMARY: &str = "Sealed summary of the build.";
    let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let reply = Arc::new(std::sync::Mutex::new(String::new()));
    let router = {
        let (seen, reply) = (seen.clone(), reply.clone());
        axum::Router::new().fallback(move |body: String| {
            let (seen, reply) = (seen.clone(), reply.clone());
            async move {
                let stream = rawjson::get(&body, "stream").bool();
                seen.lock().unwrap().push(body);
                if stream {
                    let events = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_c\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-haiku-4-5-20251001\",\"content\":[],\"usage\":{\"input_tokens\":11,\"output_tokens\":0}}}\n\n\
                                  event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n\
                                  event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
                    return ([(http::header::CONTENT_TYPE, "text/event-stream")], events).into_response();
                }
                let usage = reply.lock().unwrap().clone();
                // "empty": a 200 without any text block.
                let content = if usage == "empty" { String::new() } else { format!(r#"{{"type":"text","text":"{SUMMARY}"}}"#) };
                let usage = if usage.is_empty() || usage == "empty" { r#"{"input_tokens":11,"output_tokens":7}"#.to_owned() } else { usage };
                let body = format!(
                    r#"{{"id":"msg_compact","type":"message","role":"assistant","model":"claude-haiku-4-5-20251001","content":[{content}],"stop_reason":"end_turn","usage":{usage}}}"#
                );
                ([(http::header::CONTENT_TYPE, "application/json")], body).into_response()
            }
        })
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let mut credential = Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/claude.json"),
        serde_json::json!({"type":"claude"}).as_object().unwrap().clone(),
    )
    .unwrap();
    credential.attributes.insert("api_key".into(), "key-compaction".into());
    credential.attributes.insert("base_url".into(), base);
    let run = |payload: String, original: Option<&str>, stream: bool, alt: Option<&str>, cfg: Config| {
        let body = Bytes::from(payload);
        let usage = Arc::new(Usage::default());
        let req = ExecRequest {
            operation: Operation::Generate,
            source_format: Format::OpenAIResponse,
            response_format: Format::OpenAIResponse,
            requested_model: "claude-haiku-4-5-20251001".into(),
            model: "claude-haiku-4-5-20251001".into(),
            original_body: original.map_or_else(|| body.clone(), |o| Bytes::from(o.to_owned())),
            body,
            stream,
            alt: alt.map(str::to_owned),
            session: None,
            execution_session: None,
            derived_session: None,
            resolved_model: None,
            usage: cpa_core::exec::UsageSink::new(usage.clone()),
            request_path: String::new(),
            headers: Default::default(),
            caller: Caller {
                principal: "fake-client".into(),
                source: "x-api-key",
                provider: String::new(),
            },
        };
        seen.lock().unwrap().clear();
        let (executor, credential, seen) = (&executor, &credential, seen.clone());
        async move {
            let result = executor.execute(credential, req, &cfg).await;
            let body = match result {
                Ok(ExecResponse {
                    body: ResponseBody::Buffered(b),
                    ..
                }) => Ok(String::from_utf8(b.to_vec()).unwrap()),
                Ok(ExecResponse {
                    body: ResponseBody::Stream(events),
                    ..
                }) => {
                    let chunks: Vec<_> = events.collect().await;
                    Ok(chunks
                        .into_iter()
                        .map(|c| String::from_utf8(c.unwrap().to_vec()).unwrap())
                        .collect())
                }
                Err(e) => Err(e),
            };
            let upstream = seen.lock().unwrap().clone();
            (body, upstream, usage.kinds())
        }
    };
    let none = || Config::parse("").unwrap();
    let input = |items: &str, extra: &str| {
        format!(r#"{{"model":"claude-haiku-4-5-20251001","stream":false{extra},"input":[{items}]}}"#)
    };
    let user = r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"Check the build next."}]}"#;
    let assistant = r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"OK, I will check the build."}]}"#;
    let tools = r#","tools":[{"type":"function","name":"check_build","description":"Check the build","parameters":{"type":"object","properties":{}}}]"#;
    let calls = r#"{"type":"function_call","call_id":"call_1","name":"check_build","arguments":"{\"target\":\"api\"}"},{"type":"function_call_output","call_id":"call_1","output":"build ok"}"#;
    let trigger = r#"{"type":"compaction_trigger"}"#;
    let capsule = |summary: &str| {
        let sealed = crate::compaction_capsule::seal(summary, "claude-haiku-4-5-20251001").unwrap();
        format!(r#"{{"type":"compaction","encrypted_content":"{sealed}"}}"#)
    };
    // The single compaction item, its capsule and the usage of the answer.
    let compaction = |payload: &str, usage: (i64, i64, i64, i64)| {
        let item = rawjson::get(payload, "output");
        let items = item.array();
        assert_eq!(items.len(), 1, "{payload}");
        assert_eq!(items[0].get("type").str(), "compaction");
        let sealed = items[0].get("encrypted_content").str().to_owned();
        assert_eq!(crate::compaction_capsule::unseal(&sealed).unwrap(), SUMMARY);
        let got = rawjson::get(payload, "usage");
        assert_eq!(
            (
                got.get("input_tokens").i64(),
                got.get("output_tokens").i64(),
                got.get("total_tokens").i64(),
                got.get("input_tokens_details.cached_tokens").i64()
            ),
            usage,
            "{payload}"
        );
    };
    let summary_request = |upstream: &[String]| -> String {
        assert_eq!(upstream.len(), 1, "one summary turn");
        let body = upstream[0].clone();
        assert!(body.contains(PROMPT), "{body}");
        assert!(!body.contains("compaction_trigger"), "{body}");
        assert!(!rawjson::get(&body, "stream").bool(), "{body}");
        body
    };

    // CompactionTriggerReturnsSingleCompactionItem, and the responses/compact alt.
    for alt in [None, Some("responses/compact")] {
        let items = if alt.is_some() {
            format!("{user},{assistant}")
        } else {
            format!("{user},{assistant},{trigger}")
        };
        let (body, upstream, _) = run(input(&items, ""), None, false, alt, none()).await;
        compaction(&body.unwrap(), (11, 7, 18, 0));
        summary_request(&upstream);
    }
    // CompactionTriggerStreamReturnsSingleCompactionItem: the summary turn stays
    // non-stream; the client gets the five Responses events.
    let (body, upstream, _) = run(
        input(&format!("{user},{assistant},{trigger}"), ""),
        None,
        true,
        None,
        none(),
    )
    .await;
    let body = body.unwrap();
    assert_eq!(body.matches("event: ").count(), 5, "{body}");
    let completed = body
        .split("\n\n")
        .find(|f| f.starts_with("event: response.completed"))
        .unwrap();
    compaction(
        rawjson::get(completed.split_once("data: ").unwrap().1, "response").json(),
        (11, 7, 18, 0),
    );
    summary_request(&upstream);
    // ReplayedCompactionCapsuleIsExpanded: an ordinary turn, with the summary as context.
    let continue_ = r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"continue"}]}"#;
    let (body, upstream, _) = run(
        input(&format!("{},{continue_}", capsule(SUMMARY)), ""),
        None,
        false,
        None,
        none(),
    )
    .await;
    assert!(body.is_ok());
    assert!(
        upstream[0].contains(SUMMARY) && !upstream[0].contains(r#""type":"compaction""#),
        "{}",
        upstream[0]
    );
    assert!(!upstream[0].contains(PROMPT));
    // CompactionTriggerKeepsToolsForToolHistory.
    let (body, upstream, _) = run(
        input(&format!("{user},{calls},{trigger}"), tools),
        None,
        false,
        None,
        none(),
    )
    .await;
    compaction(&body.unwrap(), (11, 7, 18, 0));
    let sent = summary_request(&upstream);
    assert!(
        sent.contains("check_build")
            && sent.contains(r#""type":"tool_use""#)
            && sent.contains(r#""type":"tool_result""#),
        "{sent}"
    );
    assert_eq!(rawjson::get(&sent, "tool_choice.type").str(), "none", "{sent}");
    // CompactionTriggerFlattensToolHistoryWithoutTools.
    let (_, upstream, _) = run(
        input(&format!("{user},{calls},{trigger}"), ""),
        None,
        false,
        None,
        none(),
    )
    .await;
    let sent = summary_request(&upstream);
    assert!(
        !sent.contains(r#""type":"tool_use""#) && !sent.contains(r#""type":"tool_result""#),
        "{sent}"
    );
    assert!(
        sent.contains(r#"Tool call check_build (call_1): {\"target\":\"api\"}"#),
        "{sent}"
    );
    assert!(sent.contains("Tool result call_1: build ok"), "{sent}");
    assert!(!rawjson::get(&sent, "tool_choice").exists(), "{sent}");
    // CompactionSummaryPayloadRuleIsLast: payload rules see the finalized body.
    let ruled = Config::parse(
        "payload:\n  override:\n    - models: [{name: claude-haiku-4-5-20251001}]\n      params: {max_tokens: 1234, tool_choice: {type: auto}}\n",
    )
    .unwrap();
    let (_, upstream, _) = run(
        input(&format!("{user},{calls},{trigger}"), tools),
        None,
        false,
        None,
        ruled,
    )
    .await;
    let sent = summary_request(&upstream);
    assert_eq!(rawjson::get(&sent, "max_tokens").i64(), 1234, "{sent}");
    assert_eq!(rawjson::get(&sent, "tool_choice.type").str(), "auto", "{sent}");
    // CompactionTriggerIncludesPreviousCapsule.
    let earlier = capsule("Earlier sealed summary.");
    let (body, upstream, _) = run(
        input(&format!("{earlier},{continue_},{assistant},{trigger}"), ""),
        None,
        false,
        None,
        none(),
    )
    .await;
    compaction(&body.unwrap(), (11, 7, 18, 0));
    assert!(summary_request(&upstream).contains("Earlier sealed summary."));
    // ForeignCompactionCapsuleIsDropped.
    let foreign = r#"{"type":"compaction","encrypted_content":"gAAAAA-openai-native"}"#;
    let (body, upstream, _) = run(
        input(&format!("{foreign},{continue_},{trigger}"), ""),
        None,
        false,
        None,
        none(),
    )
    .await;
    compaction(&body.unwrap(), (11, 7, 18, 0));
    assert!(!summary_request(&upstream).contains("gAAAAA-openai-native"));
    // CompactionUsageMatchesResponsesAccounting: cache writes and reads count as input.
    *reply.lock().unwrap() =
        r#"{"input_tokens":11,"output_tokens":7,"cache_creation_input_tokens":3,"cache_read_input_tokens":100}"#.into();
    let (body, _, _) = run(input(&format!("{user},{trigger}"), ""), None, false, None, none()).await;
    compaction(&body.unwrap(), (114, 7, 121, 100));
    reply.lock().unwrap().clear();
    // InvalidCompactionCapsuleReturns400, before any upstream call.
    let broken = r#"{"type":"compaction","encrypted_content":"cpa-ag-compact-v1:not-valid-base64"}"#;
    // Validate before sending or publishing usage, including when only the
    // original request carries the capsule.
    let plain = input(continue_, "");
    let broken = input(&format!("{broken},{continue_}"), "");
    for (body, original) in [(broken.clone(), None), (plain, Some(broken.as_str()))] {
        let (body, upstream, usage) = run(body, original, false, None, none()).await;
        let error = body.unwrap_err();
        assert_eq!(error.status, 400);
        assert!(String::from_utf8_lossy(&error.body).starts_with("invalid compaction capsule: "));
        assert!(upstream.is_empty());
        assert_eq!(usage, ["discard"]);
    }
    // An empty summary has no delivered HTTP status or transport fault. It cools
    // the model on the trigger path and is neutral on the
    // compact alt, without a retry round (cpa-server classify tests); the summary turn
    // itself published its usage.
    *reply.lock().unwrap() = "empty".into();
    for (items, alt) in [
        (format!("{user},{trigger}"), None),
        (user.to_owned(), Some("responses/compact")),
    ] {
        let (body, upstream, usage) = run(input(&items, ""), None, false, alt, none()).await;
        let error = body.unwrap_err();
        assert_eq!((error.status, error.scope), (0, FailureScope::Model), "{alt:?}");
        assert_eq!(
            String::from_utf8_lossy(&error.body),
            "extract summary: no summary text found in upstream response"
        );
        summary_request(&upstream);
        assert!(usage.contains(&"publish"), "{usage:?}");
    }
    reply.lock().unwrap().clear();
}

/// #71: Anthropic's server-side refusal fallback is opt-in (Go v8.0.67 fa11a573).
/// Without `upstream.claude.server-side-fallback` a cloaked request that names no
/// `fallbacks` reaches Anthropic without them (and without the fallback beta); with it,
/// Opus 5.5 and Fable 5.1 get Claude Code's chain. A caller's own `fallbacks` (empty,
/// populated or `"default"`, a documented `BetaFallbacksParam` value) reach the
/// upstream byte for byte either way.
#[tokio::test]
async fn server_side_fallback_is_opt_in_and_keeps_callers_fallbacks() {
    use axum::response::IntoResponse;
    let sent = Arc::new(std::sync::Mutex::new(Vec::<(String, String)>::new()));
    let seen = sent.clone();
    let router = axum::Router::new().fallback(move |headers: http::HeaderMap, body: String| {
        let seen = seen.clone();
        async move {
            let beta = headers
                .get("anthropic-beta")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            seen.lock().unwrap().push((body, beta));
            let message = serde_json::json!({"id": "msg_fb", "type": "message", "role": "assistant",
                "model": "claude-opus-5-5", "content": [], "stop_reason": "end_turn",
                "usage": {"input_tokens": 3, "output_tokens": 1}});
            ([(http::header::CONTENT_TYPE, "application/json")], message.to_string()).into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let mut credential = harness_credential();
    credential.id = "fallback-opt-in.json".into();
    credential.attributes.insert("base_url".into(), base);
    let off = Config::parse("").unwrap();
    let explicit_off = Config::parse("upstream:\n  claude:\n    server-side-fallback: false\n").unwrap();
    let on = Config::parse("upstream:\n  claude:\n    server-side-fallback: true\n").unwrap();
    let legacy_on = Config::parse("claude:\n  server-side-fallback: true\n").unwrap();
    let oauth_on = Config::parse("oauth:\n  providers:\n    claude:\n      server-side-fallback: true\n").unwrap();
    let oauth_off_over_flat = Config::parse(
        "claude:\n  server-side-fallback: true\noauth:\n  providers:\n    claude:\n      server-side-fallback: false\n",
    )
    .unwrap();
    let callers = [
        None,
        Some("[]"),
        Some(r#"[{"model":"claude-sonnet-4-6"}]"#),
        Some(r#""default""#),
    ];
    for (model, synthesized) in [
        ("claude-opus-5-5", r#"[{"model":"claude-opus-4-8"}]"#),
        ("claude-fable-5-1", r#"[{"model":"claude-opus-5"}]"#),
    ] {
        for caller in callers {
            for (label, cfg, enabled) in [
                ("absent", &off, false),
                ("false", &explicit_off, false),
                ("true", &on, true),
                ("legacy true", &legacy_on, true),
                ("oauth true", &oauth_on, true),
                ("oauth false over flat true", &oauth_off_over_flat, false),
            ] {
                let fallbacks = caller.map(|f| format!(r#","fallbacks":{f}"#)).unwrap_or_default();
                let body = format!(
                    r#"{{"model":"{model}","max_tokens":16{fallbacks},"messages":[{{"role":"user","content":"hi"}}]}}"#
                );
                drain(&executor, &credential, cfg, client_request(body, Default::default())).await;
                let (upstream, beta) = sent.lock().unwrap().pop().unwrap();
                let got = rawjson::get(&upstream, "fallbacks");
                let case = format!("{model} caller {caller:?} setting {label}");
                match (caller, enabled) {
                    (Some(raw), _) => assert_eq!(got.json(), raw, "{case}: caller fallbacks kept"),
                    (None, true) => assert_eq!(got.json(), synthesized, "{case}: synthesized"),
                    (None, false) => assert!(!got.exists(), "{case}: none synthesized: {upstream}"),
                }
                let fallback_beta = beta.split(',').any(|b| b.trim() == betas::SERVER_SIDE_FALLBACK);
                assert_eq!(fallback_beta, got.exists(), "{case}: beta {beta}");
            }
        }
    }
    // Models without a Claude Code chain get none, whatever the setting.
    let body = r#"{"model":"claude-opus-4-6","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#;
    drain(
        &executor,
        &credential,
        &on,
        client_request(body.into(), Default::default()),
    )
    .await;
    let (upstream, _) = sent.lock().unwrap().pop().unwrap();
    assert!(!rawjson::get(&upstream, "fallbacks").exists(), "{upstream}");
}

/// Claude Code 2.1.325's client forms (testdata/claude_code_2_1_325_clients.json) are
/// prepared exactly like the same request from native `cli` Claude Code (on an OAuth
/// account that binds the user ID and signs billing, as for any native caller), and
/// keep the caller's own identity headers, including the client-app field and
/// X-Client-App. T3 Code's entrypoint does so once configured in `native-entrypoints`;
/// unlisted, it is cloaked under the baseline identity.
#[tokio::test]
async fn claude_code_2_1_325_client_forms_pass_through() {
    let doc = include_str!("testdata/claude_code_2_1_325_clients.json");
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let credential = harness_credential();
    let configured =
        Config::parse("upstream:\n  claude:\n    header-defaults:\n      native-entrypoints: [T3-Code]\n").unwrap();
    let default = Config::parse("").unwrap();
    let prepare = |headers: &Value, body: &str| {
        let case = serde_json::json!({"request_body": body, "request_headers": headers, "path": "/v1/messages"});
        let mut req = request(&case);
        let model = rawjson::string(body, "model");
        (req.model, req.requested_model) = (model.clone(), model);
        req
    };
    let run = async |req: &ExecRequest, cfg: &Config| {
        let ctx = Ctx::new(&executor, &credential, req, cfg);
        let translated = translate::request(
            req,
            ctx.codex,
            &ctx.base_model,
            ctx.is_compat,
            ctx.tool_search(),
            ctx.system_turns(req, false),
        )
        .unwrap();
        let original = translate::original(
            req,
            &translated,
            ctx.codex,
            &ctx.base_model,
            ctx.is_compat,
            ctx.tool_search(),
            ctx.system_turns(req, false),
        )
        .unwrap();
        ctx.prepare_messages(req, &translated, &original, req.stream)
            .await
            .unwrap()
    };
    for capture in rawjson::get(doc, "captures").array() {
        let name = capture.get("name").str().to_owned();
        let headers: Value = serde_json::from_str(capture.get("headers").json()).unwrap();
        let body = capture.get("body").json().to_owned();
        // The same request from interactive native Claude Code.
        let native: Value = headers
            .as_array()
            .unwrap()
            .iter()
            .filter(|h| h[0] != "X-Client-App")
            .map(|h| match h[0].as_str().unwrap() {
                "User-Agent" => serde_json::json!(["User-Agent", "claude-cli/2.1.325 (external, cli)"]),
                "x-app" => serde_json::json!(["x-app", "cli"]),
                _ => h.clone(),
            })
            .collect();
        let req = prepare(&headers, &body);
        let cfg = if name.starts_with("t3-code") {
            &configured
        } else {
            &default
        };
        let prepared = run(&req, cfg).await;
        let reference = run(&prepare(&native, &body), &default).await;
        assert_eq!(prepared.body, reference.body, "{name}: body");
        let sent = |n: &str| {
            prepared
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(n))
                .map(|(_, v)| v.as_str())
        };
        for n in [
            "User-Agent",
            "x-app",
            "X-Client-App",
            "X-Stainless-Package-Version",
            "X-Stainless-Runtime-Version",
            "X-Stainless-OS",
            "X-Stainless-Arch",
        ] {
            let caller = req.headers.get(n).map(|v| v.to_str().unwrap());
            assert_eq!(sent(n), caller, "{name}: {n}");
        }
        // The OAuth account's betas join the caller's, as for native cli.
        let beta = |h: &[(String, String)]| h.iter().find(|(k, _)| k == "anthropic-beta").map(|(_, v)| v.clone());
        assert_eq!(
            beta(&prepared.headers),
            beta(&reference.headers),
            "{name}: anthropic-beta"
        );
        if name.starts_with("t3-code") {
            let unlisted = run(&req, &default).await;
            assert_ne!(unlisted.body, reference.body, "{name}: cloaked without the setting");
            let ua = unlisted.headers.iter().find(|(k, _)| k == "User-Agent").unwrap();
            assert_eq!(ua.1, profile::BASELINE.user_agent, "{name}");
        }
    }
}

/// A translated request that only asks for reasoning summaries, on a model with manual
/// thinking only, while the caller forces a tool: the proxy must not turn on the thinking
/// the summary would need, because forced tool use and manual thinking are refused
/// together (platform docs, "Thinking with tool use"). The caller's tool choice stays.
/// The same holds for thinking the translation derives from a reasoning effort: under a
/// forced tool the translated route sends neither thinking nor effort, as Go does.
#[tokio::test]
async fn summary_only_thinking_is_not_synthesized_under_forced_tool_use() {
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let credential = harness_credential();
    let cfg = Config::parse("").unwrap();
    let tools = r#"[{"type":"function","name":"lookup","parameters":{"type":"object"}}]"#;
    for (choice, forced) in [
        (r#""required""#, true),
        (r#"{"type":"function","name":"lookup"}"#, true),
        (r#""auto""#, false),
    ] {
        let body = format!(
            r#"{{"model":"claude-haiku-4-5-20251001","input":"hi","max_output_tokens":4096,"reasoning":{{"summary":"auto"}},"tools":{tools},"tool_choice":{choice}}}"#
        );
        let effort = format!(
            r#"{{"model":"claude-opus-4-6","input":"hi","max_output_tokens":4096,"reasoning":{{"effort":"high"}},"tools":{tools},"tool_choice":{choice}}}"#
        );
        for body in [body, effort] {
            let case = serde_json::json!({"request_body": body, "request_headers": [], "path": "/v1/responses"});
            let mut req = request(&case);
            req.source_format = Format::OpenAIResponse;
            req.response_format = Format::OpenAIResponse;
            let model = rawjson::string(&body, "model");
            (req.model, req.requested_model) = (model.clone(), model);
            let ctx = Ctx::new(&executor, &credential, &req, &cfg);
            let translated = translate::request(
                &req,
                ctx.codex,
                &ctx.base_model,
                ctx.is_compat,
                ctx.tool_search(),
                ctx.system_turns(&req, false),
            )
            .unwrap();
            let original = translate::original(
                &req,
                &translated,
                ctx.codex,
                &ctx.base_model,
                ctx.is_compat,
                ctx.tool_search(),
                ctx.system_turns(&req, false),
            )
            .unwrap();
            let prepared = ctx.prepare_messages(&req, &translated, &original, false).await.unwrap();
            let sent = &prepared.body;
            let kind = rawjson::string(sent, "tool_choice.type");
            assert_eq!(matches!(kind.as_str(), "any" | "tool"), forced, "{choice}: {sent}");
            let thinking = rawjson::string(sent, "thinking.type");
            if forced {
                assert_eq!(thinking, "", "{choice}: {sent}");
                assert!(!rawjson::get(sent, "output_config.effort").exists(), "{choice}: {sent}");
            } else {
                assert!(matches!(thinking.as_str(), "enabled" | "adaptive"), "{choice}: {sent}");
            }
        }
    }
}

/// A Claude client's own thinking and tool choice reach Claude as sent, whatever their
/// combination: the provider decides. Adaptive thinking with forced tool use is valid on
/// most models; manual thinking with it, and forced tool use on Opus 5.5, Sonnet 5.5 and
/// the Fable/Mythos 5.1 and 5.2 models, are refused upstream (platform docs, "Thinking
/// with tool use"). `display`, including the beta `updates`, is the caller's too. A forced
/// choice prepares exactly like `auto` apart from the choice itself.
#[tokio::test]
async fn claude_clients_keep_their_thinking_with_forced_tools() {
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let credential = harness_credential();
    let cfg = Config::parse("").unwrap();
    let tools = r#"[{"name":"lookup","input_schema":{"type":"object"}}]"#;
    let prepare = async |model: &str, thinking: &str, choice: &str| {
        let body = format!(
            r#"{{"model":"{model}","max_tokens":4096,"thinking":{thinking},"output_config":{{"effort":"high"}},"tools":{tools},"tool_choice":{choice},"messages":[{{"role":"user","content":"hi"}}]}}"#
        );
        let case = serde_json::json!({"request_body": body, "request_headers": [], "path": "/v1/messages"});
        let mut req = request(&case);
        (req.model, req.requested_model) = (model.into(), model.into());
        let ctx = Ctx::new(&executor, &credential, &req, &cfg);
        let translated = translate::request(
            &req,
            ctx.codex,
            &ctx.base_model,
            ctx.is_compat,
            ctx.tool_search(),
            ctx.system_turns(&req, false),
        )
        .unwrap();
        let original = translate::original(
            &req,
            &translated,
            ctx.codex,
            &ctx.base_model,
            ctx.is_compat,
            ctx.tool_search(),
            ctx.system_turns(&req, false),
        )
        .unwrap();
        ctx.prepare_messages(&req, &translated, &original, false)
            .await
            .unwrap()
            .body
    };
    let field = |b: &str, p: &str| rawjson::get(b, p).json().to_owned();
    for (model, thinking) in [
        ("claude-opus-4-6", r#"{"type":"adaptive"}"#),
        ("claude-opus-4-6", r#"{"type":"adaptive","display":"summarized"}"#),
        ("claude-opus-5-5", r#"{"type":"adaptive","display":"updates"}"#),
        ("claude-fable-5-2", r#"{"type":"adaptive","display":"omitted"}"#),
        ("claude-sonnet-4-6", r#"{"type":"enabled","budget_tokens":2048}"#),
        ("claude-sonnet-4-6", r#"{"type":"disabled"}"#),
    ] {
        let auto = prepare(model, thinking, r#"{"type":"auto"}"#).await;
        assert_eq!(field(&auto, "thinking"), thinking, "{model}: auto");
        for choice in [r#"{"type":"any"}"#, r#"{"type":"tool","name":"lookup"}"#] {
            let sent = prepare(model, thinking, choice).await;
            // Tool names may be aliased by the OAuth cloak; the choice itself is the caller's.
            assert_eq!(
                field(&sent, "tool_choice.type"),
                field(choice, "type"),
                "{model} {thinking} {choice}"
            );
            for path in ["thinking", "output_config", "context_management"] {
                assert_eq!(
                    field(&sent, path),
                    field(&auto, path),
                    "{model} {thinking} {choice}: {path}"
                );
            }
        }
    }
}

/// Thinking the proxy writes for a Claude client, from a model suffix, still gives way to
/// a forced tool choice: manual thinking with forced tool use is refused on every model.
/// A client that sends no thinking but its own effort keeps the effort.
#[tokio::test]
async fn proxy_written_thinking_gives_way_to_forced_tools_on_claude_clients() {
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let credential = harness_credential();
    let cfg = Config::parse("").unwrap();
    let prepare = async |model: &str, extra: &str, choice: &str| {
        let body = format!(
            r#"{{"model":"{model}","max_tokens":4096,{extra}"tools":[{{"name":"lookup","input_schema":{{"type":"object"}}}}],"tool_choice":{choice},"messages":[{{"role":"user","content":"hi"}}]}}"#
        );
        let case = serde_json::json!({"request_body": body, "request_headers": [], "path": "/v1/messages"});
        let mut req = request(&case);
        (req.model, req.requested_model) = (model.into(), model.into());
        let ctx = Ctx::new(&executor, &credential, &req, &cfg);
        let translated = translate::request(
            &req,
            ctx.codex,
            &ctx.base_model,
            ctx.is_compat,
            ctx.tool_search(),
            ctx.system_turns(&req, false),
        )
        .unwrap();
        let original = translate::original(
            &req,
            &translated,
            ctx.codex,
            &ctx.base_model,
            ctx.is_compat,
            ctx.tool_search(),
            ctx.system_turns(&req, false),
        )
        .unwrap();
        ctx.prepare_messages(&req, &translated, &original, false)
            .await
            .unwrap()
            .body
    };
    let suffixed = "claude-haiku-4-5-20251001(2048)";
    let auto = prepare(suffixed, "", r#"{"type":"auto"}"#).await;
    assert_eq!(
        rawjson::string(&auto, "thinking.type"),
        "enabled",
        "the suffix writes thinking: {auto}"
    );
    let forced = prepare(suffixed, "", r#"{"type":"any"}"#).await;
    assert!(!rawjson::get(&forced, "thinking").exists(), "{forced}");
    // The thinking pipeline re-encodes this object; it is still the caller's.
    let spaced = prepare(
        "claude-sonnet-4-6",
        r#""thinking": {"type": "disabled"},"#,
        r#"{"type":"any"}"#,
    )
    .await;
    assert_eq!(rawjson::string(&spaced, "thinking.type"), "disabled", "{spaced}");
    let effort = prepare(
        "claude-opus-4-6",
        r#""output_config":{"effort":"low"},"#,
        r#"{"type":"any"}"#,
    )
    .await;
    assert_eq!(
        rawjson::get(&effort, "output_config.effort").json(),
        r#""low""#,
        "{effort}"
    );
    assert!(!rawjson::get(&effort, "thinking").exists(), "{effort}");
}

/// One Responses request through the full Messages preparation, on `cfg`: whether it
/// kept mid-session system items in place, and the body sent.
async fn prepare_responses(credential: &Credential, cfg: &Config, body: &Value) -> (bool, String) {
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let body = body.to_string();
    let case = serde_json::json!({"request_body": body, "request_headers": [], "path": "/v1/responses"});
    let mut req = request(&case);
    req.source_format = Format::OpenAIResponse;
    req.response_format = Format::OpenAIResponse;
    let model = rawjson::string(&body, "model");
    (req.model, req.requested_model) = (model.clone(), model);
    let ctx = Ctx::new(&executor, credential, &req, cfg);
    let system_turns = ctx.system_turns(&req, false);
    let translated = translate::request(
        &req,
        ctx.codex,
        &ctx.base_model,
        ctx.is_compat,
        ctx.tool_search(),
        system_turns,
    )
    .unwrap();
    let original = translate::original(
        &req,
        &translated,
        ctx.codex,
        &ctx.base_model,
        ctx.is_compat,
        ctx.tool_search(),
        system_turns,
    )
    .unwrap();
    let prepared = ctx.prepare_messages(&req, &translated, &original, false).await.unwrap();
    (system_turns, prepared.body)
}

/// What the prompt cache keys on, without what moves between turns by design: cache
/// breakpoints and the billing block. String content is its one text block.
fn cached_prefix(body: &str) -> (Vec<Value>, Vec<Value>) {
    fn strip(value: &mut Value) {
        match value {
            Value::Object(map) => {
                map.remove("cache_control");
                if let Some(Value::String(text)) = map.get("content") {
                    let text = text.clone();
                    map.insert("content".into(), serde_json::json!([{"type": "text", "text": text}]));
                }
                map.values_mut().for_each(strip);
            }
            Value::Array(items) => items.iter_mut().for_each(strip),
            _ => {}
        }
    }
    let mut body: Value = serde_json::from_str(body).unwrap();
    strip(&mut body);
    let system = body["system"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|b| {
            !b["text"]
                .as_str()
                .unwrap_or_default()
                .starts_with(cloak::BILLING_PREFIX)
        })
        .collect();
    (system, body["messages"].as_array().cloned().unwrap_or_default())
}

/// #183: a developer item a Responses client adds partway through a cloaked conversation
/// stays where the client put it, so the next request reads the earlier turns from the
/// prompt cache instead of writing them again.
#[tokio::test]
async fn mid_session_developer_item_keeps_the_cached_prefix() {
    let credential = harness_credential();
    let cfg = Config::parse("").unwrap();
    let first = serde_json::json!({"model": "claude-opus-4-8", "instructions": "base", "input": [
        {"type": "message", "role": "developer", "content": "leading"},
        {"type": "message", "role": "user", "content": "one"},
        {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "two"}]},
        {"type": "message", "role": "user", "content": "three"},
    ]});
    let mut second = first.clone();
    second["input"].as_array_mut().unwrap().extend([
        serde_json::json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "four"}]}),
        serde_json::json!({"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "mid"}]}),
        serde_json::json!({"type": "message", "role": "user", "content": "five"}),
    ]);
    let (system1, messages1) = cached_prefix(&prepare_responses(&credential, &cfg, &first).await.1);
    let (_, sent) = prepare_responses(&credential, &cfg, &second).await;
    let (system2, messages2) = cached_prefix(&sent);
    assert_eq!(system1, system2, "{sent}");
    assert_eq!(messages1[..], messages2[..messages1.len()], "{sent}");
    // The mid-session item follows the user message after it, the only place Claude
    // takes a system turn.
    assert_eq!(
        messages2[messages1.len()..],
        [
            serde_json::json!({"role": "assistant", "content": [{"type": "text", "text": "four"}]}),
            serde_json::json!({"role": "user", "content": [{"type": "text", "text": "five"}]}),
            serde_json::json!({"role": "system", "content": [{"type": "text", "text": "mid"}]}),
        ],
        "{sent}"
    );
}

/// #183: a mid-session item keeps the top-level path wherever the cloaked wire would not
/// send its turn as is: strict cloaking drops caller system text, a legacy model refuses
/// the turn (the preparation would fail), `rebuild-mid-system-message` folds turns into
/// `system`, and an uncloaked request keeps the caller's shape. A Claude client's
/// request is never translated.
#[tokio::test]
async fn mid_session_items_stay_top_level_unless_the_cloaked_wire_sends_them() {
    let cfg = Config::parse("").unwrap();
    for (name, attribute, model, kept) in [
        ("cloaked", None, "claude-opus-4-8", true),
        ("strict", Some(("cloak_strict_mode", "true")), "claude-opus-4-8", false),
        ("legacy", None, "claude-sonnet-4-6", false),
        (
            "rebuild",
            Some(("rebuild_mid_system_message", "true")),
            "claude-opus-4-8",
            false,
        ),
        ("uncloaked", Some(("cloak_mode", "never")), "claude-opus-4-8", false),
    ] {
        let mut credential = harness_credential();
        if let Some((key, value)) = attribute {
            credential.attributes.insert(key.into(), value.into());
        }
        let body = serde_json::json!({"model": model, "input": [
            {"role": "user", "content": "one"},
            {"role": "assistant", "content": [{"type": "output_text", "text": "two"}]},
            {"role": "developer", "content": "mid"},
            {"role": "user", "content": "three"},
        ]});
        let (system_turns, sent) = prepare_responses(&credential, &cfg, &body).await;
        assert_eq!(system_turns, kept, "{name}");
        let sent: Value = serde_json::from_str(&sent).unwrap();
        let last = sent["messages"].as_array().unwrap().last().unwrap();
        if kept {
            assert_eq!(last["role"], "system", "{name}: {sent}");
            assert_eq!(last["content"][0]["text"], "mid", "{name}: {sent}");
        } else {
            assert_eq!(last["role"], "user", "{name}: {sent}");
        }
    }
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let credential = harness_credential();
    let case = serde_json::json!({"request_body": r#"{"model":"claude-opus-4-8","messages":[]}"#, "request_headers": [], "path": "/v1/messages"});
    let req = request(&case);
    assert!(!Ctx::new(&executor, &credential, &req, &cfg).system_turns(&req, false));
}

/// A top-level `cache_control` (automatic caching) takes one of the four breakpoint
/// slots on the last cacheable block, which must not carry a marker with another TTL,
/// and 1h breakpoints must precede 5m ones (platform docs, Prompt caching). When CPA
/// owns a request's breakpoints it places that one itself, so the limit and the TTL
/// order cover it.
#[tokio::test]
async fn top_level_cache_control_becomes_the_last_breakpoint_when_cpa_owns_them() {
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let credential = harness_credential();
    let cfg = Config::parse("").unwrap();
    for top in [r#"{"type":"ephemeral"}"#, r#"{"type":"ephemeral","ttl":"1h"}"#] {
        let body = format!(
            r#"{{"model":"claude-sonnet-4-6","max_tokens":64,"cache_control":{top},"system":"Be brief.","tools":[{{"name":"t","input_schema":{{"type":"object"}}}}],"messages":[{{"role":"user","content":"one"}},{{"role":"assistant","content":"two"}},{{"role":"user","content":"three"}}]}}"#
        );
        let case = serde_json::json!({"request_body": body, "request_headers": [], "path": "/v1/messages"});
        let mut req = request(&case);
        (req.model, req.requested_model) = ("claude-sonnet-4-6".into(), "claude-sonnet-4-6".into());
        let ctx = Ctx::new(&executor, &credential, &req, &cfg);
        let translated = translate::request(
            &req,
            ctx.codex,
            &ctx.base_model,
            ctx.is_compat,
            ctx.tool_search(),
            ctx.system_turns(&req, false),
        )
        .unwrap();
        let original = translate::original(
            &req,
            &translated,
            ctx.codex,
            &ctx.base_model,
            ctx.is_compat,
            ctx.tool_search(),
            ctx.system_turns(&req, false),
        )
        .unwrap();
        let sent = ctx
            .prepare_messages(&req, &translated, &original, false)
            .await
            .unwrap()
            .body;
        assert!(!rawjson::get(&sent, "cache_control").exists(), "{top}: {sent}");
        let marks: Vec<(String, String)> = signals::blocks(&sent)
            .into_iter()
            .filter(|(_, b)| cloak::valid_cache_control(&rawjson::get(b, "cache_control")))
            .map(|(path, b)| (path, rawjson::string(&b, "cache_control.ttl")))
            .collect();
        assert!(marks.len() <= 4, "{top}: {marks:?}");
        let last = marks.iter().rfind(|(p, _)| p.starts_with("messages.")).unwrap();
        assert!(
            last.0.starts_with("messages.2."),
            "{top}: the last cacheable block: {marks:?}"
        );
        let order: Vec<&str> = marks.iter().map(|(_, ttl)| ttl.as_str()).collect();
        assert!(
            !order.windows(2).any(|w| w[0] != "1h" && w[1] == "1h"),
            "{top}: 1h after 5m: {marks:?}"
        );
    }
}

/// On an API key without the Claude Code profile, a top-level 1h breakpoint still goes
/// out as 1h, and the markers CPA adds before it join it rather than voiding it.
#[tokio::test]
async fn top_level_one_hour_survives_cpa_markers_on_api_keys() {
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let mut credential = Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/claude-api-key.json"),
        serde_json::json!({"type": "claude"}).as_object().unwrap().clone(),
    )
    .unwrap();
    credential
        .attributes
        .insert("api_key".into(), "fake-gateway-key".into());
    let cfg = Config::parse("").unwrap();
    let body = r#"{"model":"claude-sonnet-4-6","max_tokens":64,"cache_control":{"type":"ephemeral","ttl":"1h"},"system":"Be brief.","messages":[{"role":"user","content":"one"}]}"#;
    let case = serde_json::json!({"request_body": body, "request_headers": [], "path": "/v1/messages"});
    let mut req = request(&case);
    (req.model, req.requested_model) = ("claude-sonnet-4-6".into(), "claude-sonnet-4-6".into());
    let ctx = Ctx::new(&executor, &credential, &req, &cfg);
    let translated = translate::request(
        &req,
        ctx.codex,
        &ctx.base_model,
        ctx.is_compat,
        ctx.tool_search(),
        ctx.system_turns(&req, false),
    )
    .unwrap();
    let original = translate::original(
        &req,
        &translated,
        ctx.codex,
        &ctx.base_model,
        ctx.is_compat,
        ctx.tool_search(),
        ctx.system_turns(&req, false),
    )
    .unwrap();
    let sent = ctx
        .prepare_messages(&req, &translated, &original, false)
        .await
        .unwrap()
        .body;
    assert!(!rawjson::get(&sent, "cache_control").exists(), "{sent}");
    let ttls: Vec<String> = signals::blocks(&sent)
        .into_iter()
        .filter(|(_, b)| cloak::valid_cache_control(&rawjson::get(b, "cache_control")))
        .map(|(_, b)| rawjson::string(&b, "cache_control.ttl"))
        .collect();
    assert!(!ttls.is_empty() && ttls.iter().all(|t| t == "1h"), "{ttls:?}: {sent}");
}
