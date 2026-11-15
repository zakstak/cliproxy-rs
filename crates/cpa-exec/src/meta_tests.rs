//! Muse execution and OAuth behavior. Historical peer inputs remain useful, but
//! no recorded request, downstream output, diagnostic or usage record is an oracle.
//! Assertions use HTTP semantics, typed credential storage, scripted peer events,
//! local routing policy and the independently captured first-party Muse identity.

use std::time::Duration;

use serde_json::{Value, json};

use super::*;
use crate::device_fixture::{Captured, Mock, request};
use crate::meta_auth::{
    self, MetaAuth, TokenStorage, credential_file_name, encode_token_file, go_map_from_json, login_with,
};
use cpa_common::json as gj;

/// A private directory under the system temp dir, removed on drop.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("cpa-meta-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn cfg() -> Config {
    Config {
        auth_dir: std::env::temp_dir().join(format!("cpa-meta-private-{}", uuid::Uuid::new_v4())),
        ..Default::default()
    }
}

/// The scripted peer origin, `http://127.0.0.1:<port>`, found anywhere in a fixture.
fn peer_origin(fx: &Value) -> Option<String> {
    let text = fx.to_string();
    let start = text.find("http://127.0.0.1:")?;
    let rest = &text[start + "http://127.0.0.1:".len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    Some(format!("http://127.0.0.1:{digits}"))
}

fn replace_origin(value: &Value, origin: &str, url: &str) -> Value {
    match value {
        Value::String(s) => Value::String(s.replace(origin, url)),
        Value::Array(items) => Value::Array(items.iter().map(|v| replace_origin(v, origin, url)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), replace_origin(v, origin, url)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The credential Go executed with: metadata and attributes exactly as recorded, the
/// capture server's URL swapped for the Rust mock's.
fn meta_credential(fx: &Value, origin: &str, url: &str) -> Credential {
    let metadata = replace_origin(&fx["credential"], origin, url)
        .as_object()
        .cloned()
        .unwrap_or_default();
    let mut c = Credential {
        id: "meta-fixture.json".into(),
        provider: PROVIDER.into(),
        source: Source::File("/fixture/meta-fixture.json".into()),
        disabled: false,
        label: String::new(),
        attributes: Default::default(),
        metadata,
        revision: 0,
    };
    c.refresh_derived();
    c.attributes = replace_origin(&fx["attributes"], origin, url)
        .as_object()
        .map(|attrs| {
            attrs
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned()))
                .collect()
        })
        .unwrap_or_default();
    // cpa-core's config loader labels config API keys `<provider>-apikey`.
    if c.attributes.get("source").is_some_and(|s| s.starts_with("config:")) {
        c.label = format!("{PROVIDER}-apikey");
    }
    c
}

struct Run {
    usage: std::sync::Arc<crate::device_fixture::UsageLog>,
    capture: std::sync::Arc<crate::device_fixture::CaptureLog>,
    fx: Value,
    origin: String,
    url: String,
    credential: Credential,
    captured: Vec<Captured>,
    body: Option<String>,
    chunks: Vec<String>,
    error: Option<ExecError>,
}

/// The `User-Agent` the official Muse Code 1.4.15-R8339.1 linux-x86_64 build sent to a
/// loopback mock (SHA-256 `abe0a728…3d5`, as Meta's release manifest lists it).
const MUSE_CODE_USER_AGENT: &str =
    "muse-build/1.4.15 (non-interactive; linux-x86_64; build c7a95b745e7eb209c8446981803d89c497d7cae0)";

fn assert_session_headers(name: &str, sent: &Captured) -> String {
    let get = |n: &str| {
        sent.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(n))
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| panic!("{name}: {n} missing"))
    };
    let session = get("X-Tbh-Session-Id");
    let parsed = uuid::Uuid::parse_str(&session).unwrap_or_else(|_| panic!("{name}: {session:?}"));
    assert_eq!(parsed.get_version_num(), 7, "{name}: {session}");
    assert_eq!(parsed.get_variant(), uuid::Variant::RFC4122, "{name}: {session}");
    assert_eq!(parsed.hyphenated().to_string(), session, "{name}: lowercase hyphenated");
    for n in [
        "X-Meta-Ai-Gateway-Session-Id",
        "X-Tbh-Thread-Id",
        "X-Tbh-Root-Session-Id",
    ] {
        assert_eq!(get(n), session, "{name}: {n}");
    }
    assert_eq!(get("X-Tbh-Agent-Role"), "main", "{name}");
    session
}

fn fixture(provider: &str, name: &str) -> Value {
    assert_eq!(provider, "meta");
    let inputs: Value = serde_json::from_str(include_str!("../tests/fixtures/meta_peer_inputs.json")).unwrap();
    inputs
        .get(name)
        .unwrap_or_else(|| panic!("unknown peer input {name}"))
        .clone()
}

async fn run(name: &str) -> Run {
    let fx = fixture("meta", name);
    let origin = peer_origin(&fx).expect("fixture names its capture server");
    let mock = Mock::start_replacing(&fx["responses"], Some(&origin)).await;
    let credential = meta_credential(&fx, &origin, &mock.url);
    let exec = MetaExecutor::with_client(default_client()).with_mint_url(&format!("{}/muse-code/key", mock.url));
    let (mut body, mut chunks, mut error) = (None, Vec::new(), None);
    let mut cfg = Config::parse(fx["request"]["config"].as_str().unwrap_or_default()).unwrap();
    cfg.auth_dir = std::env::temp_dir().join(format!("cpa-meta-private-{}", uuid::Uuid::new_v4()));
    let usage = std::sync::Arc::new(crate::device_fixture::UsageLog::default());
    let capture = std::sync::Arc::new(crate::device_fixture::CaptureLog::default());
    let mut req = request(&fx, "");
    req.usage = usage.sink().with_capture(capture.sink());
    match exec.execute(&credential, req, &cfg).await {
        Err(e) => error = Some(e),
        Ok(response) => match response.body {
            ResponseBody::Buffered(b) => body = Some(String::from_utf8(b.to_vec()).unwrap()),
            ResponseBody::Stream(mut s) => {
                while let Some(item) = s.next().await {
                    match item {
                        Ok(chunk) => chunks.push(String::from_utf8(chunk.to_vec()).unwrap()),
                        Err(e) => error = Some(e),
                    }
                }
            }
        },
    }
    Run {
        usage,
        capture,
        captured: mock.captured(),
        url: mock.url,
        fx,
        origin,
        credential,
        body,
        chunks,
        error,
    }
}

fn peer_events(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| {
            let data = line.strip_prefix("data:")?.trim();
            serde_json::from_str(data).ok()
        })
        .collect()
}

/// Assert every field supplied by the peer, allowing the explicitly generated
/// usage-detail defaults. Generated member order and escaping have no meaning.
fn assert_fields(expected: &Value, actual: &Value, at: &str) {
    match (expected, actual) {
        (Value::Object(fields), Value::Object(found)) => {
            for (key, value) in fields {
                if key == "output" {
                    continue;
                } // Rebuilt output items are checked separately.
                assert_fields(value, found.get(key).unwrap_or(&Value::Null), &format!("{at}.{key}"));
            }
        }
        (Value::Null, Value::Number(n)) if at.contains("usage") && n.as_i64() == Some(0) => {}
        (Value::Null, Value::Object(fields)) if at.ends_with("output_tokens_details") => {
            assert_eq!(fields.get("reasoning_tokens"), Some(&json!(0)));
        }
        _ => assert_eq!(actual, expected, "{at}"),
    }
}

fn assert_upstream(r: &Run, name: &str) {
    let responses = r.fx["responses"].as_array().cloned().unwrap_or_default();
    assert_eq!(
        r.captured.len(),
        responses.len(),
        "{name}: one request per scripted peer response"
    );
    let client: Value = serde_json::from_str(r.fx["request"]["body"].as_str().unwrap_or("{}")).unwrap_or(Value::Null);
    let model = r.fx["request"]["model"]
        .as_str()
        .unwrap_or_default()
        .split('(')
        .next()
        .unwrap();
    for sent in &r.captured {
        assert_eq!(sent.method, "POST", "{name}");
        if sent.target.ends_with("/muse-code/key") {
            assert!(
                sent_header(sent, "authorization").is_some_and(|v| v.contains("dca:")),
                "{name}: mint authenticates with DCA identity"
            );
            continue;
        }
        assert!(sent.target.ends_with("/responses"), "{name}: supported endpoint");
        let body: Value = serde_json::from_slice(&sent.body).unwrap();
        assert_eq!(body["model"], model, "{name}: selected upstream model");
        assert_eq!(body["stream"], true, "{name}: Muse upstream always streams");
        assert_session_headers(name, sent);
        let authorization = sent_header(sent, "authorization").expect("upstream authentication");
        assert!(
            authorization.starts_with("Bearer ") && authorization.len() > 7,
            "{name}"
        );
        assert!(
            !authorization.contains("dca:"),
            "{name}: DCA identity is never an inference API key"
        );
        if let Some(retention) = client["prompt_cache_retention"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
        {
            assert_eq!(
                body["prompt_cache_retention"],
                retention.trim(),
                "{name}: explicit cache policy survives"
            );
        }
        if name.starts_with("apply-patch") {
            assert!(
                body["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tool| tool["type"] == "function" && tool["name"] == "apply_patch")
            );
        }
        if name == "payload-rules-and-headers" {
            // The Responses conversion removes temperature before defaults are evaluated.
            assert_eq!(body["temperature"], 0.7);
            assert_eq!(body["metadata_hint"], "d");
            assert!(body.get("wrong_protocol").is_none());
            assert!(body.get("parallel_tool_calls").is_none());
        }
    }
}

fn assert_downstream(r: &Run, name: &str) {
    let status = match name {
        "compact-rejected" => Some(501),
        "config-apikey-dca-only" => Some(401),
        "error-429-subscription-quota" | "error-429-plain" => Some(429),
        "error-404-model" | "error-404-resets" => Some(404),
        "stream-error-event" | "stream-error-after-data-only" | "stream-completed-then-error" => Some(503),
        "nonstream-response-failed" => Some(502),
        "nonstream-disconnect-before-completed" => Some(408),
        "remint-failure" => Some(500),
        "apply-patch-invalid-stream" | "apply-patch-invalid-nonstream" | "apply-patch-eof-stream" => Some(502),
        _ => None,
    };
    assert_eq!(r.error.as_ref().map(|e| e.status), status, "{name}: public outcome");
    if let Some(error) = &r.error {
        assert!(!error.body.is_empty());
        if name.starts_with("error-") {
            let peer = r.fx["responses"][0]["body"].as_str().unwrap();
            assert_eq!(
                error.body.as_ref(),
                peer.as_bytes(),
                "{name}: upstream error body retained"
            );
        }
        return;
    }
    if name == "count-tokens" {
        let body: Value = serde_json::from_str(r.body.as_ref().unwrap()).unwrap();
        assert!(
            body["usage"]["input_tokens"].as_u64().unwrap_or(0) > 0
                || body["input_tokens"].as_u64().unwrap_or(0) > 0
                || body["response"]["usage"]["input_tokens"].as_u64().unwrap_or(0) > 0
        );
        return;
    }
    let responses = r.fx["responses"].as_array().unwrap();
    let last = responses.last().unwrap()["body"].as_str().unwrap();
    let upstream = peer_events(last);
    let output = r.body.clone().unwrap_or_else(|| r.chunks.concat());
    if r.fx["request"]["source"] == "claude" {
        let events = peer_events(&output);
        assert_eq!(events.first().unwrap()["type"], "message_start");
        assert!(events.iter().any(|e| e["type"] == "message_stop"));
        assert!(events.iter().any(|e| e["delta"]["text"] == "hi there"));
        let final_usage = events.iter().find(|e| e["type"] == "message_delta").unwrap();
        assert_eq!(final_usage["usage"]["input_tokens"], 5);
        assert_eq!(final_usage["usage"]["output_tokens"], 2);
        return;
    }
    let downstream = if r.body.is_some() {
        vec![serde_json::from_str::<Value>(&output).unwrap()]
    } else {
        peer_events(&output)
    };
    if name.starts_with("apply-patch") {
        let complete = downstream
            .iter()
            .find_map(|e| e.get("response").or_else(|| e.get("output").map(|_| e)))
            .unwrap_or_else(|| panic!("{name}: response"));
        // Tool conversion preserves the raw patch string and call identity.
        let final_response = downstream
            .iter()
            .rev()
            .find_map(|e| {
                e.get("response")
                    .filter(|r| r.get("output").is_some())
                    .or_else(|| e.get("output").map(|_| e))
            })
            .unwrap_or(complete);
        let call = final_response["output"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["type"] == "custom_tool_call")
            .unwrap();
        assert_eq!(call["call_id"], "c1");
        assert_eq!(call["name"], "apply_patch");
        assert_eq!(call["input"], "*** Begin Patch\n+中😀\n*** End Patch\n");
        if r.body.is_none() {
            let deltas: String = downstream
                .iter()
                .filter(|e| e["type"] == "response.custom_tool_call_input.delta")
                .filter_map(|e| e["delta"].as_str())
                .collect();
            if !deltas.is_empty() {
                assert_eq!(deltas, call["input"].as_str().unwrap());
            }
        }
        return;
    }
    if r.body.is_some() {
        let expected = upstream
            .iter()
            .find(|e| matches!(e["type"].as_str(), Some("response.completed" | "response.incomplete")))
            .map(|e| e["response"].clone())
            .unwrap_or_else(|| serde_json::from_str(last).unwrap());
        assert_fields(&expected, &downstream[0], name);
        let mut items: Vec<Value> = upstream
            .iter()
            .filter(|e| e["type"] == "response.output_item.done")
            .map(|e| e["item"].clone())
            .collect();
        if items.is_empty() {
            items = expected["output"].as_array().cloned().unwrap_or_default();
        }
        let actual = downstream[0]["output"].as_array().cloned().unwrap_or_default();
        assert_eq!(actual.len(), items.len(), "{name}: complete output items");
        for item in items {
            assert!(actual.contains(&item), "{name}: missing output item {item}");
        }
    } else {
        // Preserve event semantics independently of transport chunking.
        assert_eq!(downstream.len(), upstream.len(), "{name}: every upstream data event");
        for (expected, actual) in upstream.iter().zip(&downstream) {
            assert_fields(expected, actual, name);
        }
    }
}

#[tokio::test]
async fn execution_preserves_typed_response_error_and_request_contracts() {
    for name in [
        "responses-nonstream-collected",
        "responses-stream",
        "responses-nonstream-json-body",
        "error-429-subscription-quota",
        "error-429-plain",
        "error-404-model",
        "error-404-resets",
        "stream-error-event",
        "stream-error-after-data-only",
        "nonstream-response-failed",
        "nonstream-incomplete-fallback-items",
        "nonstream-disconnect-before-completed",
        "stream-plain-json-lines",
        "compact-rejected",
        "count-tokens",
        "config-apikey-headers",
        "payload-rules-and-headers",
        "config-apikey-dca-only",
        "remint-from-dca",
        "remint-access-token-dca-minted-base",
        "remint-failure",
        "apply-patch-stream",
        "apply-patch-nonstream",
        "apply-patch-invalid-stream",
        "apply-patch-invalid-nonstream",
        "apply-patch-eof-stream",
        "stream-completed-then-error",
        "stream-done-only",
        "apply-patch-stream-event-lines",
        "stream-tier-merge",
        "stream-nbsp-data",
        "claude-stream-input-estimate",
    ] {
        let r = run(name).await;
        assert_upstream(&r, name);
        assert_downstream(&r, name);
    }
}

#[test]
fn error_signals_use_explicit_status_scope_and_reset_policy() {
    let future = chrono::Utc::now().timestamp() + 600;
    for (status, body, scope, retry) in [
        (
            429,
            json!({"error":{"message":"Subscription quota exhausted","resets_at":future}}),
            FailureScope::Credential,
            true,
        ),
        (
            429,
            json!({"error":{"message":"slow down"}}),
            FailureScope::Model,
            false,
        ),
        (
            404,
            json!({"error":{"message":"model not found"}}),
            FailureScope::Model,
            false,
        ),
        (
            404,
            json!({"error":{"message":"not yet","resets_at":future}}),
            FailureScope::Model,
            true,
        ),
        (
            500,
            json!({"error":{"message":"server error"}}),
            FailureScope::Credential,
            false,
        ),
    ] {
        let before = chrono::Utc::now().timestamp();
        let bytes = serde_json::to_vec(&body).unwrap();
        let error = upstream_error(status, &bytes);
        let after = chrono::Utc::now().timestamp();
        assert_eq!(error.status, status);
        assert_eq!(error.scope, scope);
        assert_eq!(error.body.as_ref(), bytes);
        if retry {
            let seconds = error.retry_after.unwrap().as_secs() as i64;
            assert!((future - after - 1..=future - before).contains(&seconds));
        } else if status == 404 {
            assert_eq!(error.retry_after, Some(Duration::from_secs(300)));
        } else {
            assert!(error.retry_after.is_none());
        }
    }
}

/// The minted key the fixture's mint endpoint answered with.
fn minted(r: &Run, index: usize) -> MintedKey {
    let body = replace_origin(&r.fx["responses"][index]["body"], &r.origin, &r.url);
    MintedKey::decode(body.as_str().unwrap().as_bytes()).unwrap()
}

fn assert_metadata_after(r: &Run, name: &str, patch: &MetadataPatch) {
    let mut after = r.credential.metadata.clone();
    patch.apply(&mut after);
    let key = minted(r, 0);
    assert_eq!(after["api_key"], key.api_key, "{name}");
    assert_eq!(after["access_token"], key.api_key);
    assert!(chrono::DateTime::parse_from_rfc3339(after["last_refresh"].as_str().unwrap()).is_ok());
    assert!(
        after.get("expired").is_none(),
        "the newly minted key has no inherited token expiry"
    );
    for (field, value) in [("email", key.user_email), ("name", key.user_full_name)] {
        if !value.is_empty() {
            assert_eq!(after[field], value);
        } else if let Some(before) = r.credential.metadata.get(field) {
            assert_eq!(&after[field], before);
        }
    }
    for (field, value) in [
        ("subs_tier_name", key.subs_tier_name),
        ("subs_tier_id", key.subs_tier_id),
    ] {
        if value.is_empty() {
            assert!(after.get(field).is_none());
        } else {
            assert_eq!(after[field], value);
        }
    }
    assert_eq!(after["is_subs_active"], key.is_subs_active);
    assert_eq!(after["has_payment_method"], key.has_payment_method);
    for (field, value) in &r.credential.metadata {
        if ![
            "api_key",
            "access_token",
            "base_url",
            "last_refresh",
            "expired",
            "email",
            "name",
            "subs_tier_name",
            "subs_tier_id",
            "dca_token",
            "type",
            "is_subs_active",
            "has_payment_method",
        ]
        .contains(&field.as_str())
        {
            assert_eq!(&after[field], value, "{name}: preserved {field}");
        }
    }
}

#[tokio::test]
async fn remint_patch_replaces_key_and_retains_unrelated_metadata() {
    for name in ["remint-from-dca", "remint-access-token-dca-minted-base"] {
        let r = run(name).await;
        let dca = dca_token(&r.credential).unwrap();
        let patch = refresh_patch(&r.credential, &dca, &minted(&r, 0), "2026-10-02T00:00:00Z".into());
        assert_metadata_after(&r, name, &patch);
    }
}

#[tokio::test]
async fn preparation_remints_through_the_local_peer() {
    let fx = fixture("meta", "refresh-direct");
    let origin = peer_origin(&fx).unwrap();
    let mock = Mock::start_replacing(&fx["responses"], Some(&origin)).await;
    let credential = meta_credential(&fx, &origin, &mock.url);
    let exec = MetaExecutor::with_client(default_client()).with_mint_url(&format!("{}/muse-code/key", mock.url));
    assert!(
        !exec.needs_prepare(&credential, &cfg()),
        "a usable key and no refresh interval: nothing to prepare (RefreshLead nil)"
    );
    let patch = exec.prepare(&credential, &cfg()).await.unwrap();
    let r = Run {
        usage: Default::default(),
        capture: Default::default(),
        captured: mock.captured(),
        url: mock.url,
        fx,
        origin,
        credential,
        body: None,
        chunks: Vec::new(),
        error: None,
    };
    assert_upstream(&r, "refresh-direct");
    assert_metadata_after(&r, "refresh-direct", &patch);
}

#[test]
fn preparation_requires_a_refresh_source_and_due_credentials() {
    // Explicit credential variants cover usable keys, DCA-only credentials and expiry.
    let cred = |meta: Value, attrs: &[(&str, &str)]| {
        let mut c = Credential::from_file(
            std::path::Path::new("/a"),
            std::path::Path::new("/a/meta.json"),
            meta.as_object().unwrap().clone(),
        )
        .unwrap();
        c.provider = PROVIDER.into();
        c.attributes = attrs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        c
    };
    let exec = MetaExecutor::default();
    let cfg = cfg();
    assert!(!exec.needs_prepare(&cred(json!({"type":"meta"}), &[("api_key", "exists")]), &cfg));
    assert!(!exec.needs_prepare(&cred(json!({"type":"meta"}), &[]), &cfg));
    let config_dca = cred(
        json!({"type":"meta"}),
        &[("api_key", "dca:invalid-for-config"), ("source", "config:meta[0]")],
    );
    assert!(is_config_api_key(&config_dca));
    // Go `AuthSourceKind` folds with strings.ToLower and EqualFold, which map U+0130 to
    // `i` (outputs of Go's IsConfigAPIKeyAuth at ae3ee3be).
    for (attrs, want) in [
        (&[("auth_kind", "apikey"), ("source_backend", "CONFİG")][..], true),
        (&[("auth_kind", "apikey"), ("source", "CONFİG:meta[0]")][..], true),
        (&[("auth_kind", "apikey"), ("source", " Config ")][..], true),
        (
            &[("auth_kind", "apikey"), ("source", "config"), ("runtime_only", "TRUE")][..],
            false,
        ),
        (
            &[
                ("auth_kind", "apikey"),
                ("source", "config:meta[0]"),
                ("source_backend", "Fİle"),
            ][..],
            false,
        ),
    ] {
        assert_eq!(
            is_config_api_key(&cred(json!({"type":"meta"}), attrs)),
            want,
            "{attrs:?}"
        );
    }
    assert!(!exec.needs_prepare(&config_dca, &cfg));
    assert!(exec.needs_prepare(
        &cred(
            json!({"type":"meta","dca_token":"dca:valid","auth_kind":"oauth"}),
            &[("base_url", "http://x")]
        ),
        &cfg
    ));
    // A due refresh_interval re-mints even with a key (shouldRefresh's preferred interval).
    assert!(exec.needs_prepare(
        &cred(
            json!({"type":"meta","api_key":"k","dca_token":"dca:v","refresh_interval":60,"last_refresh":"2020-01-01T00:00:00Z"}),
            &[]
        ),
        &cfg
    ));
}

#[test]
fn credentials_resolve_operator_overrides_before_saved_defaults() {
    // Operator attributes win; DCA identity tokens are only mint sources.
    let mut c = Credential::from_file(
        std::path::Path::new("/a"),
        std::path::Path::new("/a/meta.json"),
        json!({"type":"meta"}).as_object().unwrap().clone(),
    )
    .unwrap();
    c.attributes.insert("api_key".into(), "attr-key".into());
    c.attributes
        .insert("base_url".into(), "https://custom.meta.com/v1".into());
    assert_eq!(creds(&c), ("https://custom.meta.com/v1".into(), "attr-key".into()));
    c.attributes.clear();
    c.metadata.insert("access_token".into(), "meta-oauth-token".into());
    assert_eq!(creds(&c), (DEFAULT_API_BASE_URL.into(), "meta-oauth-token".into()));
    // DCA tokens are never used as keys but are the mint source.
    c.metadata.insert("access_token".into(), " dca:x ".into());
    c.metadata.insert("api_base_url".into(), "https://alt/v1".into());
    assert_eq!(creds(&c), ("https://alt/v1".into(), String::new()));
    assert_eq!(dca_token(&c).as_deref(), Some("dca:x"));
}

#[test]
fn retry_hint_follows_resets_at() {
    // TestMetaExecutor_ParseRetryAfter: a future resets_at about 45 minutes out.
    let future = chrono::Utc::now().timestamp() + 45 * 60;
    let body = format!(
        r#"{{"error":{{"code":"rate_limit_exceeded","message":"Subscription quota exhausted.","resets_at":{future},"type":"rate_limit_error"}}}}"#
    );
    let e = upstream_error(429, body.as_bytes());
    let secs = e.retry_after.unwrap().as_secs();
    assert!((40 * 60..=50 * 60).contains(&secs));
    assert_eq!(e.scope, FailureScope::Credential);
    let past = chrono::Utc::now().timestamp() - 600;
    let e = upstream_error(429, format!(r#"{{"error":{{"resets_at":{past}}}}}"#).as_bytes());
    assert_eq!(e.retry_after, None);
    assert_eq!(e.scope, FailureScope::Model);
    assert_eq!(upstream_error(400, body.as_bytes()).retry_after, None);
}

#[test]
fn credential_writer_preserves_settings_replaces_tokens_and_writes_private_files() {
    for (email, subject) in [
        ("a@example.test", "s"),
        ("../strange/name@example.test", ""),
        ("", "account/1"),
        ("", ""),
    ] {
        let name = credential_file_name(email, subject);
        assert!(name.starts_with("meta-") && name.ends_with(".json"));
        assert!(!name.contains('/') && !name.contains('\\'));
        assert_eq!(credential_file_name(email, subject), name);
    }
    assert_ne!(credential_file_name("a+b@x", ""), credential_file_name("a/b@x", ""));
    let dir = TempDir::new();
    let storage: TokenStorage = serde_json::from_value(
        json!({"access_token":"fresh-key","dca_token":"dca:fresh","api_key":"fresh-key","email":"new@example.test"}),
    )
    .unwrap();
    let disk = br#"{"type":"meta","access_token":"stale","api_key":"stale","prefix":"team","note":"<kept> & safe","disabled":true,"custom":{"nested":42}}"#;
    let snapshot = go_map_from_json(br#"{"prefix":"snapshot","disabled":false,"custom":{"snapshot":true}}"#)
        .unwrap()
        .0;
    for metadata in [None, Some(&snapshot)] {
        let output = encode_token_file(&storage, metadata, Some(disk));
        let value: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(value["access_token"], "fresh-key");
        assert_eq!(value["api_key"], "fresh-key");
        assert_eq!(value["dca_token"], "dca:fresh");
        assert_eq!(value["email"], "new@example.test");
        assert_eq!(value["type"], "meta");
        assert_eq!(value["auth_kind"], "oauth");
        if metadata.is_some() {
            assert_eq!(value["prefix"], "snapshot");
            assert_eq!(value["disabled"], false);
            assert_eq!(value["custom"], json!({"snapshot":true}));
        } else {
            assert_eq!(value["prefix"], "team");
            assert_eq!(value["disabled"], true);
            assert_eq!(value["note"], "<kept> & safe");
            assert_eq!(value["custom"], json!({"nested":42}));
        }
        let path = dir.path().join(if metadata.is_some() {
            "snapshot.json"
        } else {
            "disk.json"
        });
        std::fs::write(&path, disk).unwrap();
        meta_auth::save_token_file(&path, &storage, metadata).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&std::fs::read(&path).unwrap()).unwrap(),
            value
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
    assert!(
        std::fs::read_dir(dir.path())
            .unwrap()
            .all(|e| !e.unwrap().file_name().to_string_lossy().starts_with('.'))
    );
}

fn login_auth(url: &str) -> MetaAuth {
    MetaAuth::new(default_client())
        .with_auth_host(url)
        .with_mint_url(&format!("{url}/muse-code/key"))
        .with_poll_unit(Duration::from_millis(10))
}

#[tokio::test]
async fn device_login_persists_minted_or_dca_only_private_credentials() {
    for name in ["login", "login-no-mint"] {
        let fx = fixture("meta", name);
        let mock = Mock::start(&fx["responses"]).await;
        let dir = TempDir::new();
        let (email, dca, key) = if name == "login" {
            ("Some.User+tag@Example.com", "dca:login-token", "mk-login")
        } else {
            ("", "dca:nomint", "")
        };
        let filename = credential_file_name(email, dca);
        if let Some(stale) = fx["request"]["stale_file"].as_str().filter(|s| !s.is_empty()) {
            std::fs::write(dir.path().join(&filename), stale).unwrap();
        }
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-02T00:00:00Z")
            .unwrap()
            .to_utc();
        let outcome = login_with(login_auth(&mock.url).with_fixed_now(now), dir.path(), true)
            .await
            .unwrap();
        assert_eq!(outcome.path, dir.path().join(filename));
        assert_eq!(outcome.label, if email.is_empty() { "Meta" } else { email });
        let saved: Value = serde_json::from_slice(&std::fs::read(&outcome.path).unwrap()).unwrap();
        assert_eq!(saved["type"], "meta");
        assert_eq!(saved["auth_kind"], "oauth");
        assert_eq!(saved["access_token"], if key.is_empty() { dca } else { key });
        assert_eq!(saved["dca_token"], dca);
        assert_eq!(saved["last_refresh"], "2026-10-02T00:00:00Z");
        if !key.is_empty() {
            assert_eq!(saved["api_key"], key);
            assert_eq!(saved["email"], email);
            assert_eq!(saved["name"], "Some <User>");
            assert_eq!(saved["prefix"], "team");
            assert_eq!(saved["custom_setting"], "from-disk");
            assert_eq!(saved["disabled"], true);
        } else {
            assert!(saved.get("api_key").is_none());
            assert!(saved.get("expired").is_some());
        }
        let requests = mock.captured();
        assert_eq!(requests.len(), fx["responses"].as_array().unwrap().len());
        for request in requests {
            assert_eq!(request.method, "POST");
            if request.target.ends_with("/muse-code/key") {
                assert!(sent_header(&request, "authorization").unwrap().contains(dca));
            } else {
                let form: std::collections::HashMap<_, _> =
                    url::form_urlencoded::parse(&request.body).into_owned().collect();
                assert!(!form.is_empty());
                assert!(form.contains_key("client_id") || form.contains_key("device_code"));
            }
        }
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&outcome.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[tokio::test]
async fn failed_login_mint_logs_no_response_body() {
    // login-no-mint's flow, with a mint endpoint whose error body carries key material.
    let fx = fixture("meta", "login-no-mint");
    let mut responses = fx["responses"].clone();
    let secret = "sk-mint-SECRET-0123456789";
    responses[2]["body"] = json!(format!(r#"{{"error":"bad","api_key":"{secret}"}}"#));
    let mock = Mock::start(&responses).await;
    let dir = TempDir::new();
    let capture = crate::device_fixture::LogCapture::default();
    let guard = capture.install();
    let outcome = login_with(login_auth(&mock.url), dir.path(), true).await;
    drop(guard);
    assert!(outcome.is_ok(), "a failed mint does not fail the login");
    let logs = capture.0.lock().unwrap().clone();
    assert!(
        logs.iter()
            .any(|l| l.contains("could not mint api_key from dca_token: meta auth: mint key failed (HTTP 500)")),
        "the mint failure is logged: {logs:?}"
    );
    for line in &logs {
        assert!(
            !line.contains(secret) && !line.contains("\"bad\""),
            "body logged: {line}"
        );
    }
}

#[tokio::test]
async fn device_flow_handles_denial_pending_slow_down_and_deadline() {
    let device = json!({"status":200,"body":r#"{"device_code":"d","user_code":"u","interval":1,"expires_in":600}"#});
    let poll = |status: u16, body: &str| json!({"status":status,"body":body});
    let cases = [
        (
            poll(400, r#"{"error":"access_denied"}"#),
            "meta auth: access was denied by user",
        ),
        (
            poll(400, r#"{"error":"expired_token"}"#),
            "meta auth: device code has expired",
        ),
        (
            poll(400, r#"{"error":"invalid_client","error_description":"bad client"}"#),
            "meta auth: error from authorization server: invalid_client: bad client",
        ),
        (
            poll(200, r#"{"token_type":"bearer"}"#),
            "meta auth: response missing access_token",
        ),
    ];
    for (answer, message) in cases {
        let mock = Mock::start(&json!([device, answer])).await;
        let auth = login_auth(&mock.url);
        let code = auth.start_device_flow().await.unwrap();
        assert_eq!(auth.wait_for_authorization(&code).await.unwrap_err(), message);
    }
    // slow_down adds five server seconds; unknown non-JSON answers keep polling.
    let mock = Mock::start(&json!([
        device,
        poll(400, r#"{"error":"slow_down"}"#),
        poll(502, "bad gateway"),
        poll(200, r#"{"access_token":"dca:t","expires_in":0}"#),
        poll(500, "mint down"),
    ]))
    .await;
    let auth = login_auth(&mock.url);
    let code = auth.start_device_flow().await.unwrap();
    let started = tokio::time::Instant::now();
    let bundle = auth.wait_for_authorization(&code).await.unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(10 + 60 + 60),
        "slow_down widened the interval"
    );
    assert_eq!(bundle.token.access_token, "dca:t");
    assert_eq!(bundle.minted, None, "a failed mint keeps the DCA-only bundle");
    // The deadline is the device code's expires_in.
    let mock = Mock::start(&json!([
        {"status":200,"body":r#"{"device_code":"d","user_code":"u","interval":1,"expires_in":3}"#},
        poll(400, r#"{"error":"authorization_pending"}"#),
        poll(400, r#"{"error":"authorization_pending"}"#),
        poll(400, r#"{"error":"authorization_pending"}"#),
    ]))
    .await;
    let auth = login_auth(&mock.url);
    let code = auth.start_device_flow().await.unwrap();
    assert_eq!(
        auth.wait_for_authorization(&code).await.unwrap_err(),
        "meta auth: authorization timed out or canceled: context deadline exceeded"
    );
    // Device authorization failures.
    let mock = Mock::start(&json!([
        poll(400, " nope "),
        poll(200, r#"{"device_code":"d","user_code":" "}"#),
        poll(200, r#"{"device_code":"d","user_code":"u","interval":1.5}"#),
    ]))
    .await;
    let auth = login_auth(&mock.url);
    let error = auth.start_device_flow().await.unwrap_err();
    assert!(error.contains("HTTP 400"));
    assert!(!error.contains("nope"), "peer response body stays private");
    assert_eq!(
        auth.start_device_flow().await.unwrap_err(),
        "meta device flow: response missing required device_code or user_code"
    );
    assert!(
        auth.start_device_flow()
            .await
            .unwrap_err()
            .starts_with("meta device flow: parse response: ")
    );
}

#[test]
fn auth_records_decode_typed_device_token_and_mint_fields() {
    let device = br#"{"device_code":"d","user_code":"u","verification_uri":"https://meta.example/device","verification_uri_complete":"https://meta.example/device?code=u","expires_in":600,"interval":5,"future":{"opaque":true}}"#;
    let decoded = meta_auth::DeviceCode::decode_for_test(device).unwrap();
    assert_eq!(
        (
            decoded.device_code.as_str(),
            decoded.user_code.as_str(),
            decoded.expires_in,
            decoded.interval
        ),
        ("d", "u", 600, 5)
    );
    assert_eq!(decoded.verification_uri, "https://meta.example/device");
    assert_eq!(decoded.verification_uri_complete, "https://meta.example/device?code=u");
    let (token, result) = meta_auth::TokenData::decode_for_test(br#"{"access_token":"dca:test","token_type":"bearer","expires_in":600,"expires_at":2000000000,"error":"","error_description":"","future":true}"#);
    result.unwrap();
    assert_eq!(
        (
            token.access_token.as_str(),
            token.token_type.as_str(),
            token.expires_in,
            token.expires_at
        ),
        ("dca:test", "bearer", 600, 2000000000)
    );
    let minted = MintedKey::decode(br#"{"api_key":"test-key","base_url":"https://api.example/v1","user_email":"a@example.test","user_full_name":"Example","subs_tier_name":"pro","subs_tier_id":"tier-1","is_subs_active":true,"has_payment_method":false,"require_payment":false,"can_subscribe":true}"#).unwrap();
    assert_eq!(
        (
            minted.api_key.as_str(),
            minted.base_url.as_str(),
            minted.user_email.as_str(),
            minted.user_full_name.as_str()
        ),
        ("test-key", "https://api.example/v1", "a@example.test", "Example")
    );
    assert_eq!(
        (minted.subs_tier_name.as_str(), minted.subs_tier_id.as_str()),
        ("pro", "tier-1")
    );
    assert!(minted.is_subs_active && minted.can_subscribe);
    assert!(!minted.has_payment_method && !minted.require_payment);
    for raw in [
        &br#"{"interval":"5"}"#[..],
        br#"{"expires_in":1.5}"#,
        br#"{"device_code":true}"#,
    ] {
        assert!(
            meta_auth::DeviceCode::decode_for_test(raw).is_err(),
            "invalid typed device field: {raw:?}"
        );
    }
    for raw in [
        &br#"{"access_token":1}"#[..],
        br#"{"expires_in":1.5}"#,
        b"[]",
        b"not JSON",
    ] {
        assert!(
            meta_auth::TokenData::decode_for_test(raw).1.is_err(),
            "invalid token field: {raw:?}"
        );
    }
    for raw in [
        &br#"{"api_key":1}"#[..],
        br#"{"is_subs_active":"yes"}"#,
        b"[]",
        b"not JSON",
    ] {
        assert!(MintedKey::decode(raw).is_err(), "invalid mint field: {raw:?}");
    }
}

#[test]
fn single_byte_json_validation_matches_json_grammar() {
    for byte in 0..=255u8 {
        let raw = [byte];
        let expected = serde_json::from_slice::<Value>(&raw).is_ok();
        assert_eq!(crate::meta_wire::check_valid(&raw).is_ok(), expected, "byte {byte}");
    }
}

#[tokio::test]
async fn authorization_errors_and_missing_mint_keys_are_refused() {
    let mock = Mock::start(&json!([
        {"status":200,"body":r#"{"device_code":"d","user_code":"u","interval":1}"#},
        {"status":400,"body":r#"{"error":"access_denied"}"#},
    ]))
    .await;
    let auth = login_auth(&mock.url);
    let code = auth.start_device_flow().await.unwrap();
    let error = auth.wait_for_authorization(&code).await.unwrap_err();
    assert!(error.contains("denied"));
    let mock = Mock::start(&json!([
        {"status":200,"body":"null"}, {"status":200,"body":r#"{"api_key":"test-key"}"#}
    ]))
    .await;
    let auth = login_auth(&mock.url);
    assert!(
        auth.mint_api_key("dca:n")
            .await
            .unwrap_err()
            .to_string()
            .contains("api_key")
    );
    assert_eq!(auth.mint_api_key("dca:d").await.unwrap().api_key, "test-key");
}

#[tokio::test]
async fn failed_error_body_read_is_not_classified_by_status() {
    // Go returns io.ReadAll's error before wrapMetaUpstreamError: a truncated 429 body
    // must not become a credential-wide quota cooldown.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 65536];
        let _ = socket.read(&mut buf).await;
        let body = r#"{"error":{"message":"subscription quota exhausted","resets_at":4102444800}}"#;
        let head = format!(
            "HTTP/1.1 429 Too Many Requests\r\nContent-Length: {}\r\n\r\n",
            body.len() + 100
        );
        let _ = socket.write_all(head.as_bytes()).await;
        let _ = socket.write_all(body.as_bytes()).await;
    });
    let fx = fixture("meta", "error-429-subscription-quota");
    let origin = peer_origin(&fx).unwrap();
    let credential = meta_credential(&fx, &origin, &url);
    let error = MetaExecutor::with_client(default_client())
        .execute(&credential, request(&fx, ""), &cfg())
        .await
        .err()
        .unwrap();
    assert_ne!(error.status, 429);
    assert_eq!(error.scope, FailureScope::Transport);
    assert_eq!(error.retry_after, None);
}

#[tokio::test]
async fn empty_original_request_falls_back_to_the_payload() {
    // prepareResponsesRequest: originalPayload is req.Payload when OriginalRequest is empty,
    // so response.created carries the client model with its suffix.
    let fx = fixture("meta", "responses-stream");
    let origin = peer_origin(&fx).unwrap();
    let mock = Mock::start(&json!([{"status":200,"headers":[["Content-Type","text/event-stream"]],"body":"data: {\"type\":\"response.created\",\"response\":{}}\n\n"}])).await;
    let credential = meta_credential(&fx, &origin, &mock.url);
    let mut req = request(&fx, "");
    req.original_body = Bytes::new();
    let response = MetaExecutor::with_client(default_client())
        .execute(&credential, req, &cfg())
        .await
        .unwrap();
    let ResponseBody::Stream(mut stream) = response.body else {
        panic!("stream expected");
    };
    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(
        String::from_utf8(first.to_vec()).unwrap(),
        "data: {\"type\":\"response.created\",\"response\":{\"model\":\"muse-spark-1.3(high)\"}}\n\n"
    );
}

#[test]
fn login_does_not_merge_unrepresentable_saved_metadata() {
    // Manager.Login ignores an existing file whose map decode fails (float64 overflow);
    // the standalone writer keeps Go's partial map with null.
    let dir = TempDir::new();
    let bundle = meta_auth::Bundle {
        token: meta_auth::TokenData {
            access_token: "dca:x".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    let name = credential_file_name("", "dca:x");
    std::fs::write(dir.path().join(&name), r#"{"type":"meta","custom":1e400,"prefix":"p"}"#).unwrap();
    let now = chrono::DateTime::parse_from_rfc3339("2026-10-02T00:00:00Z")
        .unwrap()
        .to_utc();
    let outcome = meta_auth::save_login(dir.path(), &bundle, now).unwrap();
    let written = std::fs::read_to_string(&outcome.path).unwrap();
    assert!(serde_json::from_str::<Value>(&written).is_ok(), "valid JSON: {written}");
    assert!(!written.contains("prefix") && !written.contains("custom"), "{written}");
    let storage = TokenStorage {
        access_token: "a".into(),
        ..Default::default()
    };
    assert_eq!(
        encode_token_file(&storage, None, Some(br#"{"custom":1e400,"n":1.0}"#)),
        "{\n  \"access_token\": \"a\",\n  \"auth_kind\": \"oauth\",\n  \"custom\": null,\n  \"n\": 1,\n  \"type\": \"meta\"\n}\n"
    );
}

#[tokio::test]
async fn login_skips_existing_file_with_an_overwritten_overflow() {
    let fx = fixture("meta", "login-existing-overflow");
    let mock = Mock::start(&fx["responses"]).await;
    let dir = TempDir::new();
    let filename = credential_file_name("Some.User+tag@Example.com", "dca:ov");
    std::fs::write(
        dir.path().join(&filename),
        fx["request"]["stale_file"].as_str().unwrap(),
    )
    .unwrap();
    let now = chrono::DateTime::parse_from_rfc3339("2026-10-02T00:00:00Z")
        .unwrap()
        .to_utc();
    let outcome = login_with(login_auth(&mock.url).with_fixed_now(now), dir.path(), true)
        .await
        .unwrap();
    assert_eq!(outcome.path, dir.path().join(filename));
    let written: Value = serde_json::from_slice(&std::fs::read(&outcome.path).unwrap()).unwrap();
    assert_eq!(written["access_token"], "mk-ov");
    assert_eq!(written["dca_token"], "dca:ov");
    assert_eq!(written["disabled"], false);
    assert!(
        written.get("prefix").is_none() && written.get("x").is_none(),
        "unclean stale metadata cannot enter the new login"
    );
}

#[test]
fn strict_error_body_read_surfaces_errors_past_the_kept_prefix() {
    let chunks: Vec<Result<Bytes, ExecError>> = vec![
        Ok(Bytes::from_static(b"0123456789")),
        Ok(Bytes::from_static(b"abcdef")),
        Err(ExecError::local(502, FailureScope::Transport, "unexpected EOF")),
    ];
    let body = futures_util::stream::iter(chunks).boxed();
    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let error = rt.block_on(crate::helpers::read_all_strict(body, 4)).unwrap_err();
    assert_eq!(error.scope, FailureScope::Transport);
    let ok = futures_util::stream::iter(vec![Ok(Bytes::from_static(b"0123456789"))]).boxed();
    assert_eq!(rt.block_on(crate::helpers::read_all_strict(ok, 4)).unwrap(), "0123");
}

#[tokio::test]
async fn mint_failures_keep_safe_context_and_withhold_upstream_content() {
    let mock = Mock::start(&json!([
        {"status":200,"body":r#"{"api_key":""}"#},
        {"status":200,"body":r#"{"api_key":"k","is_subs_active":"yes"}"#},
        {"status":403,"body":" denied "},
    ]))
    .await;
    let auth = login_auth(&mock.url);
    assert_eq!(
        auth.mint_api_key("dca:1").await.unwrap_err().to_string(),
        "meta auth: mint response missing api_key"
    );
    assert!(
        auth.mint_api_key("dca:2")
            .await
            .unwrap_err()
            .to_string()
            .starts_with("meta auth: parse mint response: ")
    );
    let e = auth.mint_api_key("dca:3").await.unwrap_err();
    assert!(e.to_string().contains("HTTP 403"));
    assert!(!e.to_string().contains("denied"), "peer response body stays private");
    assert_eq!(e.redacted(), e.to_string());
    assert!(!format!("{e:?}").contains("denied"));
    assert_eq!(
        auth.mint_api_key("  ").await.unwrap_err().to_string(),
        "meta auth: missing dca token"
    );
}

#[tokio::test]
async fn apply_patch_stream_errors_stop_translation_and_emit_one_terminal_failure() {
    // Go's Meta loop: StopApplyPatchStream after each translated line; at EOF only the
    // translator's flush (no FinalizeApplyPatchStream, no [DONE]).
    async fn run(lines: Vec<&'static str>, fail_on_finalize: bool) -> (Vec<String>, Vec<String>) {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let translator = Box::new(crate::device_fixture::PatchProbe {
            failed: false,
            fail_on_finalize,
            log: log.clone(),
        });
        let upstream =
            futures_util::stream::iter(lines.into_iter().map(|l| Ok(Bytes::from_static(l.as_bytes())))).boxed();
        let inactive = cpa_translate::apply_patch_responses::State::new(Format::OpenAIResponse, b"{}", b"{}");
        let out: Vec<String> = stream_events(
            upstream,
            translator,
            crate::claude_input_tokens::ClaudeInputTokens::new(
                Format::OpenAIResponse,
                Format::Codex,
                Format::OpenAIResponse,
                Bytes::new(),
            ),
            false,
            inactive,
            Default::default(),
            Default::default(),
        )
        .map(|item| match item {
            Ok(b) => String::from_utf8(b.to_vec()).unwrap(),
            Err(e) => format!("ERR {} {}", e.status, String::from_utf8_lossy(&e.body)),
        })
        .collect()
        .await;
        let log = log.lock().unwrap().clone();
        (out, log)
    }
    let patch = format!("ERR 502 {}", cpa_translate::APPLY_PATCH_UPSTREAM_ERROR);
    let (out, log) = run(
        vec!["data: {\"a\":1}", "data: {\"BAD_PATCH\":1}", "data: {\"c\":1}"],
        false,
    )
    .await;
    assert_eq!(
        out,
        [
            "frame data: {\"a\":1}",
            "frame data: {\"BAD_PATCH\":1}",
            "flushed",
            patch.as_str()
        ]
    );
    assert_eq!(log.len(), 2);
    let (out, log) = run(vec!["data: {\"a\":1}"], true).await;
    assert_eq!(out, ["frame data: {\"a\":1}", "finished"]);
    assert_eq!(log, ["event data: {\"a\":1}", "finish"]);
}

#[tokio::test]
async fn usage_reports_retain_peer_counts_and_failure_outcomes() {
    use crate::device_fixture::UsageEvent;
    for name in [
        "responses-nonstream-collected",
        "responses-stream",
        "responses-nonstream-json-body",
        "stream-error-event",
        "nonstream-response-failed",
        "nonstream-incomplete-fallback-items",
        "payload-rules-and-headers",
        "remint-from-dca",
        "error-429-plain",
        "apply-patch-stream",
        "apply-patch-nonstream",
        "apply-patch-invalid-stream",
        "apply-patch-invalid-nonstream",
        "apply-patch-eof-stream",
        "stream-completed-then-error",
        "stream-done-only",
        "apply-patch-stream-event-lines",
        "stream-tier-merge",
        "stream-nbsp-data",
        "claude-stream-input-estimate",
    ] {
        let r = run(name).await;
        let events = r.usage.events();
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(
                    e,
                    UsageEvent::Publish | UsageEvent::PublishFailure(..) | UsageEvent::PublishFailureWithUsage(..)
                ))
                .count(),
            usize::from(events.iter().any(|e| matches!(
                e,
                UsageEvent::Publish | UsageEvent::PublishFailure(..) | UsageEvent::PublishFailureWithUsage(..)
            ))),
            "{name}: at most one terminal publication"
        );
        assert!(
            events.iter().any(|e| matches!(e, UsageEvent::RoundTripStarted)),
            "{name}: an upstream attempt starts timing"
        );
        let payloads: Vec<Value> = r
            .usage
            .payloads()
            .iter()
            .filter_map(|(_, raw)| serde_json::from_slice(raw).ok())
            .collect();
        if name == "stream-completed-then-error" {
            assert_eq!(r.error.as_ref().unwrap().status, 503);
            assert!(
                payloads.iter().any(
                    |v| v["response"]["usage"]["input_tokens"] == 5 && v["response"]["usage"]["output_tokens"] == 2
                )
            );
        }
        let peers = r.fx["responses"].as_array().unwrap();
        let body = peers.last().unwrap()["body"].as_str().unwrap();
        let mut messages = peer_events(body);
        if let Ok(value) = serde_json::from_str::<Value>(body) {
            messages.push(value);
        }
        if r.error.is_none() && !name.starts_with("error-") {
            for message in messages.iter().filter(|v| {
                (v.get("type").is_none() || v["type"] == "response.completed")
                    && (v.get("usage").is_some() || v["response"].get("usage").is_some())
            }) {
                let expected = message.get("usage").unwrap_or(&message["response"]["usage"]);
                assert!(
                    payloads.iter().any(|actual| {
                        let actual = actual.get("usage").unwrap_or(&actual["response"]["usage"]);
                        ["input_tokens", "output_tokens", "total_tokens"]
                            .iter()
                            .all(|key| expected.get(key).is_none() || actual.get(key) == expected.get(key))
                    }),
                    "{name}: peer token counts survive reporting: {expected}"
                );
            }
        }
    }
}

#[tokio::test]
async fn request_capture_reports_actual_attempt_bytes_and_response_metadata() {
    use crate::device_fixture::CaptureRecord;
    let inputs: Value = serde_json::from_str(include_str!("../tests/fixtures/meta_peer_inputs.json")).unwrap();
    for (name, input) in inputs.as_object().unwrap() {
        if input["request"].get("login").is_some() || name == "refresh-direct" {
            continue;
        }
        let r = run(name).await;
        let records = r.capture.records();
        let actual: Vec<_> = r
            .captured
            .iter()
            .filter(|request| request.target.ends_with("/responses"))
            .collect();
        let requests: Vec<_> = records
            .iter()
            .filter_map(|record| match record {
                CaptureRecord::Request {
                    url,
                    method,
                    headers,
                    body,
                    auth,
                } => Some((url, method, headers, body, auth)),
                _ => None,
            })
            .collect();
        assert_eq!(
            requests.len(),
            actual.len(),
            "{name}: one capture per inference request"
        );
        for ((url, method, headers, body, auth), sent) in requests.iter().zip(actual) {
            assert!(url.starts_with(&r.url));
            assert_eq!(*method, &sent.method);
            assert_eq!(*body, &sent.body);
            assert_eq!(auth[0], "meta");
            assert_eq!(auth[1], r.credential.id);
            assert!(headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("content-type")));
        }
        let statuses: Vec<_> = records
            .iter()
            .filter_map(|r| {
                if let CaptureRecord::Metadata(status, headers) = r {
                    assert!(headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("content-type")));
                    Some(*status)
                } else {
                    None
                }
            })
            .collect();
        if !requests.is_empty() {
            assert_eq!(statuses.len(), 1, "{name}");
            assert_eq!(
                u64::from(statuses[0]),
                r.fx["responses"].as_array().unwrap().last().unwrap()["status"]
            );
        }
        for record in records {
            match record {
                CaptureRecord::Chunk(bytes) => {
                    let peer = r.fx["responses"].as_array().unwrap().last().unwrap()["body"]
                        .as_str()
                        .unwrap();
                    assert!(
                        peer.as_bytes().windows(bytes.len().max(1)).any(|part| part == bytes) || bytes.is_empty(),
                        "{name}: captured bytes originate at the peer"
                    );
                }
                CaptureRecord::Error(message) => assert!(!message.is_empty(), "{name}: error context"),
                _ => {}
            }
        }
    }
}

/// Go ignores a refusal only the original (baseline) body hits
/// (TranslateRequestWithAPIKeyModelCompatibility returns no error): the working body is
/// the one sent, and only its own refusal refuses the request.
#[tokio::test]
async fn a_refused_baseline_alone_does_not_refuse_the_request() {
    let fx = fixture("meta", "responses-nonstream-collected");
    let origin = peer_origin(&fx).unwrap();
    let hi = br#"{"model":"muse-spark-1.3","max_tokens":16,"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#;
    let refused = br#"{"model":"muse-spark-1.3","max_tokens":16,"messages":[{"role":"user","content":[{"type":"container_upload","source":{"type":"file","file_id":"f1"}}]}]}"#;
    for baseline_only in [true, false] {
        let mock = Mock::start(&fx["responses"]).await;
        let credential = meta_credential(&fx, &origin, &mock.url);
        let mut req = request(&fx, "");
        req.source_format = Format::Claude;
        req.response_format = Format::Claude;
        let (body, original): (&'static [u8], &'static [u8]) =
            if baseline_only { (hi, refused) } else { (refused, hi) };
        req.body = Bytes::from_static(body);
        req.original_body = Bytes::from_static(original);
        let result = MetaExecutor::with_client(default_client())
            .execute(&credential, req, &cfg())
            .await;
        assert_eq!(result.is_ok(), baseline_only, "baseline_only={baseline_only}");
        let captured = mock.captured();
        assert_eq!(captured.len(), usize::from(baseline_only));
        if let Some(sent) = captured.first() {
            assert!(String::from_utf8_lossy(&sent.body).contains(r#""text":"hi""#));
        }
    }
}

/// The upstream request one Meta execution sends with a fixture's synthetic credential,
/// captured by a private loopback mock. `edit` adjusts the request and credential.
async fn sent_request(name: &str, edit: impl FnOnce(&mut ExecRequest, &mut Credential), cfg: &Config) -> Captured {
    let fx = fixture("meta", name);
    let origin = peer_origin(&fx).unwrap();
    let mock = Mock::start(&fx["responses"]).await;
    let mut credential = meta_credential(&fx, &origin, &mock.url);
    let mut req = request(&fx, "");
    edit(&mut req, &mut credential);
    let response = MetaExecutor::with_client(default_client())
        .execute(&credential, req, cfg)
        .await
        .unwrap_or_else(|e| panic!("{name}: {e:?}"));
    if let ResponseBody::Stream(mut s) = response.body {
        while s.next().await.is_some() {}
    }
    let mut captured = mock.captured();
    assert_eq!(captured.len(), 1, "{name}");
    captured.remove(0)
}

fn sent_header(sent: &Captured, name: &str) -> Option<String> {
    sent.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

/// An account login (`responses-nonstream-collected`: OAuth record with a DCA token) and
/// a config API key (`config-apikey-headers`).
const ACCOUNT: &str = "responses-nonstream-collected";
const API_KEY: &str = "config-apikey-headers";

/// #81: the identity the official Muse Code 1.4.15 `muse exec` sent to a loopback mock
/// (docs/DIFFERENCES-FROM-GO.md, "Muse Code client identity"). With an account login,
/// `X-Tbh-Contributor-Mode` is `1` exactly for a `-contributor` model and `0` otherwise;
/// with an API key it is absent. Headers the client sends only for side agents, tracing
/// or opt-in routing are not sent.
#[tokio::test]
async fn identity_follows_the_captured_muse_code_profile() {
    let cfg = cfg();
    let cases: [(&str, &str, Option<&str>); 6] = [
        (ACCOUNT, "muse-spark-1.3", Some("0")),
        (ACCOUNT, "muse-spark-1.3-contributor", Some("1")),
        (ACCOUNT, "muse-spark-1.3-contributor(high)", Some("1")),
        (ACCOUNT, "Muse-Spark-1.3-Contributor", Some("1")),
        (API_KEY, "muse-spark-1.3", None),
        (API_KEY, "muse-spark-1.3-contributor", None),
    ];
    for (fixture_name, model, contributor) in cases {
        let sent = sent_request(fixture_name, |req, _| req.model = model.into(), &cfg).await;
        let what = format!("{fixture_name} {model}");
        assert_eq!(
            sent_header(&sent, "User-Agent").as_deref(),
            Some(MUSE_CODE_USER_AGENT),
            "{what}"
        );
        assert_eq!(sent_header(&sent, "X-Client-Id").as_deref(), Some("tbh:exec"), "{what}");
        assert_eq!(
            sent_header(&sent, "X-Tbh-Contributor-Mode").as_deref(),
            contributor,
            "{what}"
        );
        assert_session_headers(&what, &sent);
        for absent in [
            "traceparent",
            "X-Tbh-Parent-Thread-Id",
            "X-FB-Routing-Control",
            "X-Tbh-Invocation-Kind",
        ] {
            assert_eq!(sent_header(&sent, absent), None, "{what}: {absent}");
        }
    }
}

/// #81: the Muse session id lives as long as the client's explicit session. Requests
/// without one, including those with only a guessed session (first messages, conversation
/// prefix), each get a fresh UUIDv7; requests naming the same session on the same
/// credential share one; another session or another credential gets another.
#[tokio::test]
async fn session_ids_follow_explicit_client_sessions() {
    let cfg = cfg();
    let unique = uuid::Uuid::new_v4().to_string();
    let named = |session: String| {
        move |req: &mut ExecRequest, _: &mut Credential| {
            req.headers.insert("Session-Id", session.parse().unwrap());
        }
    };
    let session = |sent: &Captured, what: &str| assert_session_headers(what, sent);

    let a1 = session(&sent_request(ACCOUNT, |_, _| {}, &cfg).await, "none 1");
    let a2 = session(&sent_request(ACCOUNT, |_, _| {}, &cfg).await, "none 2");
    assert_ne!(a1, a2, "no explicit session: one Muse session per request");
    for guessed in ["msg:0123456789abcdef", "lcp:v1:abc", "derived:abc"] {
        let g1 = session(
            &sent_request(ACCOUNT, |req, _| req.session = Some(guessed.into()), &cfg).await,
            guessed,
        );
        let g2 = session(
            &sent_request(ACCOUNT, |req, _| req.session = Some(guessed.into()), &cfg).await,
            guessed,
        );
        assert_ne!(g1, g2, "{guessed} is not an explicit session");
    }

    let s1 = session(
        &sent_request(ACCOUNT, named(format!("s1-{unique}")), &cfg).await,
        "s1 first",
    );
    let s1_again = session(
        &sent_request(ACCOUNT, named(format!("s1-{unique}")), &cfg).await,
        "s1 again",
    );
    assert_eq!(s1, s1_again, "the same explicit session keeps its Muse session id");
    let contributor_turn = sent_request(
        ACCOUNT,
        |req, c| {
            named(format!("s1-{unique}"))(req, c);
            req.model = "muse-spark-1.3-contributor".into();
        },
        &cfg,
    )
    .await;
    assert_eq!(
        session(&contributor_turn, "s1 contributor"),
        s1,
        "the model does not end a session"
    );
    let s2 = session(&sent_request(ACCOUNT, named(format!("s2-{unique}")), &cfg).await, "s2");
    assert_ne!(s1, s2, "another session, another id");
    let other_account = sent_request(
        ACCOUNT,
        |req, c| {
            named(format!("s1-{unique}"))(req, c);
            c.id = "meta-other-account.json".into();
        },
        &cfg,
    )
    .await;
    assert_ne!(
        session(&other_account, "s1 other credential"),
        s1,
        "never shared across credentials"
    );
}

/// A user can open unrelated conversations. Only an actual session signal, including
/// a transport-owned session when a plain user ID masks it, retains the Muse UUID.
#[tokio::test]
async fn user_identity_does_not_become_a_muse_conversation() {
    let cfg = cfg();
    let unique = uuid::Uuid::new_v4().to_string();
    let edit = |req: &mut ExecRequest, user: &str| {
        let mut body: Value = serde_json::from_slice(&req.body).unwrap();
        body["metadata"] = json!({"user_id": user});
        req.body = serde_json::to_vec(&body).unwrap().into();
        req.original_body = req.body.clone();
    };
    for user in [format!("plain-{unique}"), json!({"account_uuid": unique}).to_string()] {
        let first = sent_request(ACCOUNT, |req, _| edit(req, &user), &cfg).await;
        let next = sent_request(ACCOUNT, |req, _| edit(req, &user), &cfg).await;
        assert_ne!(
            assert_session_headers("user first", &first),
            assert_session_headers("user next", &next)
        );
    }
    let user = format!("user-{unique}");
    let transport = format!("transport-{unique}");
    let turn = || {
        sent_request(
            ACCOUNT,
            |req, _| {
                edit(req, &user);
                req.execution_session = Some(transport.clone());
            },
            &cfg,
        )
    };
    let first = turn().await;
    let next = turn().await;
    let id = assert_session_headers("transport first", &first);
    assert_eq!(id, assert_session_headers("transport next", &next));
    let other = sent_request(
        ACCOUNT,
        |req, _| {
            edit(req, &user);
            req.execution_session = Some(format!("other-{transport}"));
        },
        &cfg,
    )
    .await;
    assert_ne!(id, assert_session_headers("another transport", &other));

    let named = || {
        sent_request(
            ACCOUNT,
            |req, _| {
                edit(req, &user);
                req.headers.insert("Session-Id", transport.parse().unwrap());
            },
            &cfg,
        )
    };
    assert_eq!(
        assert_session_headers("named first", &named().await),
        assert_session_headers("named next", &named().await),
    );
}

/// Plain user identities must not hide a named conversation, including the legacy
/// root/envelope spellings. Body conversations also outrank transport fallback.
#[tokio::test]
async fn plain_user_ids_do_not_shadow_explicit_conversation_fields() {
    let cfg = cfg();
    let unique = uuid::Uuid::new_v4().to_string();
    for nested in [false, true] {
        for path in [
            "conversation_id",
            "conversationId",
            "chat_id",
            "chatId",
            "metadata.conversation_id",
            "extra_body.conversation_id",
        ] {
            let turn = |conversation: String, transport: String, user: &str| {
                let mut fields = json!({"metadata": {"user_id": user}});
                if let Some((root, key)) = path.split_once('.') {
                    if fields.get(root).is_none() {
                        fields[root] = json!({});
                    }
                    fields[root][key] = conversation.into();
                } else {
                    fields[path] = conversation.into();
                }
                let fields = if nested { json!({"request": fields}) } else { fields };
                sent_request(
                    ACCOUNT,
                    move |req, _| {
                        let mut body: Value = serde_json::from_slice(&req.body).unwrap();
                        body.as_object_mut()
                            .unwrap()
                            .extend(fields.as_object().unwrap().clone());
                        req.body = serde_json::to_vec(&body).unwrap().into();
                        req.original_body = req.body.clone();
                        req.execution_session = (!transport.is_empty()).then_some(transport);
                    },
                    &cfg,
                )
            };
            let conversation = format!("{unique}-{nested}-{path}");
            let first = turn(conversation.clone(), String::new(), "user-a").await;
            let next = turn(conversation.clone(), "transport-b".into(), "user-b").await;
            let id = assert_session_headers(path, &first);
            assert_eq!(id, assert_session_headers(path, &next), "{nested}: {path}");
            let other = turn(format!("other-{conversation}"), "transport-b".into(), "user-b").await;
            assert_ne!(id, assert_session_headers(path, &other), "{nested}: {path}");
        }
    }

    // A session encoded in Claude metadata retains its higher priority.
    for user in [
        json!({"session_id": unique}).to_string(),
        format!("user_account_session_{unique}"),
    ] {
        let turn = |conversation: String| {
            let user = user.clone();
            sent_request(
                ACCOUNT,
                move |req, _| {
                    let mut body: Value = serde_json::from_slice(&req.body).unwrap();
                    body["metadata"] = json!({"user_id": user});
                    body["conversation_id"] = conversation.into();
                    req.body = serde_json::to_vec(&body).unwrap().into();
                    req.original_body = req.body.clone();
                },
                &cfg,
            )
        };
        assert_eq!(
            assert_session_headers("encoded first", &turn("first".into()).await),
            assert_session_headers("encoded next", &turn("next".into()).await),
        );
    }
}

/// Session retention follows the authorization root, not the reusable filename or a
/// short-lived minted key. Caller namespaces also keep equal session names apart.
#[tokio::test]
async fn muse_sessions_are_fenced_by_authorization_and_caller() {
    let cfg = cfg();
    let unique = uuid::Uuid::new_v4().to_string();
    async fn turn(fixture: &str, unique: &str, root: &str, key: &str, caller: &str, cfg: &Config) -> Captured {
        sent_request(
            fixture,
            |req, c| {
                req.headers.insert("Session-Id", unique.parse().unwrap());
                req.caller.principal = caller.into();
                c.id = format!("same-file-{unique}.json");
                if !root.is_empty() {
                    c.metadata.insert("dca_token".into(), root.into());
                }
                c.metadata.insert("api_key".into(), key.into());
                c.metadata.insert("access_token".into(), key.into());
                c.attributes.remove("api_key");
                c.attributes.remove("access_token");
            },
            cfg,
        )
        .await
    }
    let first = turn(
        ACCOUNT,
        &unique,
        "dca:account-a",
        "fake-minted-a",
        "fake-caller-a",
        &cfg,
    )
    .await;
    let refreshed = turn(
        ACCOUNT,
        &unique,
        "dca:account-a",
        "fake-minted-a-new",
        "fake-caller-a",
        &cfg,
    )
    .await;
    let id = assert_session_headers("account a", &first);
    assert_eq!(id, assert_session_headers("account a refreshed", &refreshed));
    for (root, key, caller) in [
        ("dca:account-b", "fake-minted-b", "fake-caller-a"),
        ("dca:recreated-account-c", "fake-minted-c", "fake-caller-a"),
        ("dca:account-a", "fake-minted-a", "fake-caller-b"),
    ] {
        let replaced = turn(ACCOUNT, &unique, root, key, caller, &cfg).await;
        assert_ne!(id, assert_session_headers("replaced account or caller", &replaced));
    }
    let first_key = turn(API_KEY, &unique, "", "fake-api-a", "fake-caller-a", &cfg).await;
    let same_key = turn(API_KEY, &unique, "", "fake-api-a", "fake-caller-a", &cfg).await;
    let other_key = turn(API_KEY, &unique, "", "fake-api-b", "fake-caller-a", &cfg).await;
    let id = assert_session_headers("api a", &first_key);
    assert_eq!(id, assert_session_headers("api a again", &same_key));
    assert_ne!(id, assert_session_headers("api b", &other_key));
}

async fn assert_authorization_override_session_fence(dynamic: bool) {
    let cfg = cfg();
    let unique = uuid::Uuid::new_v4().to_string();
    for fixture in [ACCOUNT, API_KEY] {
        let turn = |bearer: &'static str, shadow: &'static str| {
            let session = unique.clone();
            sent_request(
                fixture,
                move |req, credential| {
                    req.headers.insert("Session-Id", session.parse().unwrap());
                    // Case variants are applied in attribute order, last wins.
                    credential
                        .attributes
                        .insert("header:Authorization".into(), shadow.into());
                    credential.attributes.insert(
                        "header:authorization".into(),
                        if dynamic { "$X-Upstream-Auth" } else { bearer }.into(),
                    );
                    req.headers.insert("X-Upstream-Auth", bearer.parse().unwrap());
                },
                &cfg,
            )
        };
        let first = turn("Bearer fake-override-a", "Bearer ignored-a").await;
        let next = turn("Bearer fake-override-a", "Bearer ignored-b").await;
        let other = turn("Bearer fake-override-b", "Bearer ignored-b").await;
        assert_eq!(
            sent_header(&first, "Authorization").as_deref(),
            Some("Bearer fake-override-a")
        );
        assert_eq!(
            sent_header(&next, "Authorization").as_deref(),
            Some("Bearer fake-override-a")
        );
        assert_eq!(
            sent_header(&other, "Authorization").as_deref(),
            Some("Bearer fake-override-b")
        );
        let id = assert_session_headers("override first", &first);
        assert_eq!(
            id,
            assert_session_headers("override next", &next),
            "the winning bearer is unchanged"
        );
        assert_ne!(
            id,
            assert_session_headers("override replaced", &other),
            "{fixture}: effective bearer changed"
        );
    }
}

#[tokio::test]
async fn muse_sessions_follow_literal_authorization_overrides() {
    assert_authorization_override_session_fence(false).await;
}

#[tokio::test]
async fn muse_sessions_follow_referenced_authorization_overrides() {
    assert_authorization_override_session_fence(true).await;
}

/// An unresolved custom header does not replace the credential's default bearer.
#[tokio::test]
async fn missing_authorization_reference_keeps_default_session_scope() {
    let cfg = cfg();
    let unique = uuid::Uuid::new_v4().to_string();
    for custom in [false, true] {
        let turn = || {
            sent_request(
                ACCOUNT,
                |req, credential| {
                    req.headers.insert("Session-Id", unique.parse().unwrap());
                    if custom {
                        credential
                            .attributes
                            .insert("header:Authorization".into(), "$X-Not-Sent".into());
                    }
                },
                &cfg,
            )
        };
        let first = turn().await;
        let next = sent_request(
            ACCOUNT,
            |req, _| {
                req.headers.insert("Session-Id", unique.parse().unwrap());
            },
            &cfg,
        )
        .await;
        assert_eq!(
            sent_header(&first, "Authorization"),
            sent_header(&next, "Authorization")
        );
        assert_eq!(
            assert_session_headers("default first", &first),
            assert_session_headers("default next", &next)
        );
    }
}

/// #81, Go v8.0.67 c2f402b4: contributor mode follows the model the prepared body sends,
/// after alias resolution and payload rules, not the client's requested name.
#[tokio::test]
async fn contributor_mode_follows_the_final_model() {
    let rule = |from: &str, to: &str| {
        Config::parse(&format!(
            "payload:\n  override:\n    - models: [{{name: \"{from}\", protocol: meta}}]\n      params: {{model: \"{to}\"}}\n"
        ))
        .unwrap()
    };
    let plain = cfg();
    let to_contributor = rule("muse-spark-1.3", "muse-spark-1.3-contributor");
    let to_plain = rule("muse-spark-1.3-contributor", "muse-spark-1.3");
    let cases: [(&str, &str, &Config, &str, &str); 4] = [
        ("my-contributor-alias", "muse-spark-1.3", &plain, "muse-spark-1.3", "0"),
        (
            "fast",
            "muse-spark-1.3-contributor",
            &plain,
            "muse-spark-1.3-contributor",
            "1",
        ),
        (
            "muse-spark-1.3",
            "muse-spark-1.3",
            &to_contributor,
            "muse-spark-1.3-contributor",
            "1",
        ),
        (
            "muse-spark-1.3-contributor",
            "muse-spark-1.3-contributor",
            &to_plain,
            "muse-spark-1.3",
            "0",
        ),
    ];
    for (requested, upstream, cfg, sent_model, mode) in cases {
        let sent = sent_request(
            ACCOUNT,
            |req, _| {
                req.requested_model = requested.into();
                req.model = upstream.into();
            },
            cfg,
        )
        .await;
        let what = format!("{requested} -> {upstream}");
        assert_eq!(&*gj::get(&sent.body, "model").str(), sent_model, "{what}");
        assert_eq!(
            sent_header(&sent, "X-Tbh-Contributor-Mode").as_deref(),
            Some(mode),
            "{what}"
        );
    }
}

/// #81: a caller cannot set the identity headers (they are not forwarded), while the
/// operator's credential headers still replace them, as every Meta header policy did.
#[tokio::test]
async fn caller_headers_cannot_set_the_identity_and_operator_headers_still_win() {
    let cfg = cfg();
    let caller = sent_request(
        ACCOUNT,
        |req, _| {
            for (name, value) in [
                ("User-Agent", "caller-agent"),
                ("X-Client-Id", "tbh:tui"),
                ("X-Tbh-Session-Id", "caller-session"),
                ("X-Meta-Ai-Gateway-Session-Id", "caller-session"),
                ("X-Tbh-Contributor-Mode", "1"),
                ("X-Tbh-Agent-Role", "reminder:goal-reminder"),
                ("traceparent", "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"),
            ] {
                req.headers.insert(name, value.parse().unwrap());
            }
        },
        &cfg,
    )
    .await;
    assert_eq!(
        sent_header(&caller, "User-Agent").as_deref(),
        Some(MUSE_CODE_USER_AGENT)
    );
    assert_eq!(sent_header(&caller, "X-Client-Id").as_deref(), Some("tbh:exec"));
    assert_eq!(sent_header(&caller, "X-Tbh-Contributor-Mode").as_deref(), Some("0"));
    assert_eq!(sent_header(&caller, "traceparent"), None);
    assert_ne!(assert_session_headers("caller", &caller), "caller-session");

    let operator = sent_request(
        ACCOUNT,
        |_, c| {
            c.attributes.insert("header:User-Agent".into(), "operator-agent".into());
            c.attributes
                .insert("header:X-Tbh-Agent-Role".into(), "operator-role".into());
        },
        &cfg,
    )
    .await;
    assert_eq!(sent_header(&operator, "User-Agent").as_deref(), Some("operator-agent"));
    assert_eq!(
        sent_header(&operator, "X-Tbh-Agent-Role").as_deref(),
        Some("operator-role")
    );
    assert_eq!(sent_header(&operator, "X-Client-Id").as_deref(), Some("tbh:exec"));
}

/// The body the executor sends upstream for `body` from a `source` client, captured by a
/// private loopback mock with the fixture's synthetic credential.
async fn sent_body(source: Format, body: &str, stream: bool, cfg: &Config) -> Vec<u8> {
    let fx = fixture("meta", "responses-nonstream-collected");
    let origin = peer_origin(&fx).unwrap();
    let mock = Mock::start(&fx["responses"]).await;
    let credential = meta_credential(&fx, &origin, &mock.url);
    let mut req = request(&fx, "");
    req.source_format = source;
    req.response_format = source;
    req.stream = stream;
    req.body = Bytes::from(body.to_owned());
    req.original_body = req.body.clone();
    let response = MetaExecutor::with_client(default_client())
        .execute(&credential, req, cfg)
        .await
        .unwrap_or_else(|e| panic!("{source:?} {body}: {e:?}"));
    if let ResponseBody::Stream(mut s) = response.body {
        while s.next().await.is_some() {}
    }
    let captured = mock.captured();
    assert_eq!(captured.len(), 1, "{source:?} {body}");
    captured[0].body.clone()
}

/// One client body per format Meta serves (a Codex-format client is refused with 501
/// before anything is sent), with `{}` standing for the `prompt_cache_retention` member
/// (or nothing) between the model and the rest.
const RETENTION_BODIES: [(Format, &str); 3] = [
    (
        Format::OpenAIResponse,
        r#"{"model":"muse-spark-1.3"{},"input":"hello","temperature":0.2,"metadata":{"k":"v"}}"#,
    ),
    (
        Format::OpenAI,
        r#"{"model":"muse-spark-1.3"{},"messages":[{"role":"user","content":"hello"}],"temperature":0.2}"#,
    ),
    (
        Format::Claude,
        r#"{"model":"muse-spark-1.3"{},"max_tokens":16,"messages":[{"role":"user","content":"hello"}]}"#,
    ),
];

/// M2 (#82), Go v8.0.67 f77a3c10: an explicit string `prompt_cache_retention` that is not
/// blank reaches Meta trimmed; absent, empty, blank, null and non-string values are not
/// sent, and nothing invents a default. Every other byte of the prepared body is the one
/// the same request without the member produces.
#[tokio::test]
async fn prompt_cache_retention_passes_through_only_as_an_explicit_string() {
    let cfg = cfg();
    let cases: [(&str, Option<&str>); 11] = [
        (r#","prompt_cache_retention":"24h""#, Some("24h")),
        (r#","prompt_cache_retention":"in_memory""#, Some("in_memory")),
        // Unknown values are the upstream's to judge.
        (r#","prompt_cache_retention":"7d""#, Some("7d")),
        // Go's strings.TrimSpace, then set as a string.
        (r#","prompt_cache_retention":" 24h\t""#, Some("24h")),
        (r#","prompt_cache_retention":"""#, None),
        (r#","prompt_cache_retention":"   ""#, None),
        (r#","prompt_cache_retention":null"#, None),
        (r#","prompt_cache_retention":24"#, None),
        (r#","prompt_cache_retention":true"#, None),
        (r#","prompt_cache_retention":{"ttl":"24h"}"#, None),
        (r#","prompt_cache_retention":["24h"]"#, None),
    ];
    for (source, template) in RETENTION_BODIES {
        for stream in [false, true] {
            let absent = sent_body(source, &template.replace("{}", ""), stream, &cfg).await;
            assert!(
                !gj::get(&absent, "prompt_cache_retention").exists(),
                "{source:?} stream={stream}: no default when absent: {}",
                String::from_utf8_lossy(&absent)
            );
            for (member, want) in cases {
                let body = template.replace("{}", member);
                let mut sent = sent_body(source, &body, stream, &cfg).await;
                let got = gj::get(&sent, "prompt_cache_retention");
                let shown = String::from_utf8_lossy(&sent).into_owned();
                match want {
                    Some(want) => {
                        assert_eq!(got.kind, Kind::String, "{source:?} stream={stream} {member}: {shown}");
                        assert_eq!(
                            &*got.bytes(),
                            want.as_bytes(),
                            "{source:?} stream={stream} {member}: {shown}"
                        );
                    }
                    None => assert!(!got.exists(), "{source:?} stream={stream} {member}: {shown}"),
                }
                gj::delete(&mut sent, "prompt_cache_retention");
                assert_eq!(
                    String::from_utf8_lossy(&sent),
                    String::from_utf8_lossy(&absent),
                    "{source:?} stream={stream} {member}: other bytes unchanged"
                );
            }
        }
    }
}

/// The payload rules stay the last change: a `default` rule fills only an omitted
/// retention (its baseline carries the client's value), `override` replaces it and
/// `filter` removes it.
#[tokio::test]
async fn payload_rules_decide_prompt_cache_retention_after_the_client() {
    let rule = |kind: &str, params: &str| {
        Config::parse(&format!(
            "payload:\n  {kind}:\n    - models: [{{name: \"muse-*\", protocol: meta}}]\n      params: {params}\n"
        ))
        .unwrap()
    };
    let default = rule("default", r#"{prompt_cache_retention: "24h"}"#);
    let override_ = rule("override", r#"{prompt_cache_retention: "24h"}"#);
    let filter = rule("filter", "[prompt_cache_retention]");
    let explicit = r#"{"model":"muse-spark-1.3","prompt_cache_retention":"in_memory","input":"hello"}"#;
    let omitted = r#"{"model":"muse-spark-1.3","input":"hello"}"#;
    let blank = r#"{"model":"muse-spark-1.3","prompt_cache_retention":" ","input":"hello"}"#;
    let cases: [(&Config, &str, Option<&str>); 6] = [
        (&default, omitted, Some("24h")),
        (&default, blank, Some("24h")),
        (&default, explicit, Some("in_memory")),
        (&override_, explicit, Some("24h")),
        (&override_, omitted, Some("24h")),
        (&filter, explicit, None),
    ];
    for (i, (cfg, body, want)) in cases.into_iter().enumerate() {
        for stream in [false, true] {
            let sent = sent_body(Format::OpenAIResponse, body, stream, cfg).await;
            let got = gj::get(&sent, "prompt_cache_retention");
            let shown = String::from_utf8_lossy(&sent).into_owned();
            match want {
                Some(want) => assert_eq!(&*got.bytes(), want.as_bytes(), "case {i} stream={stream}: {shown}"),
                None => assert!(!got.exists(), "case {i} stream={stream}: {shown}"),
            }
        }
    }
}

/// U1 (#99): a Muse stream marks TTFT at the first event with output (here the completed
/// message item), after the first packet and `response.created` were only frames.
#[tokio::test]
async fn stream_ttft_is_marked_at_the_first_output_event() {
    let r = run("responses-stream").await;
    let marks: Vec<bool> = r
        .usage
        .events()
        .into_iter()
        .filter_map(|e| match e {
            crate::device_fixture::UsageEvent::Token(token) => Some(token),
            _ => None,
        })
        .collect();
    // The first packet, then response.created, the message item and response.completed.
    assert_eq!(marks, [false, false, true, true]);
}

/// #168: a Claude client of Muse reads the same thinking blocks streamed and buffered:
/// summary parts joined with a blank line, and no thinking block for a whitespace-only
/// summary (a signed one keeps its signature-only block).
#[tokio::test]
async fn claude_clients_read_the_same_reasoning_streamed_and_buffered() {
    let fx = fixture("meta", "responses-nonstream-collected");
    let origin = peer_origin(&fx).unwrap();
    for stream in [true, false] {
        let responses = json!([{
            "status": 200,
            "headers": [["Content-Type", "text/event-stream"]],
            "body": crate::reasoning_testkit::answer("muse-spark-1.3"),
        }]);
        let mock = Mock::start(&responses).await;
        let credential = meta_credential(&fx, &origin, &mock.url);
        let mut req = request(&fx, "");
        req.source_format = Format::Claude;
        req.response_format = Format::Claude;
        req.stream = stream;
        req.body = Bytes::from(format!(
            r#"{{"model":"muse-spark-1.3","max_tokens":16,"stream":{stream},"messages":[{{"role":"user","content":"hi"}}]}}"#
        ));
        req.original_body = req.body.clone();
        let response = MetaExecutor::with_client(default_client())
            .execute(&credential, req, &cfg())
            .await
            .unwrap_or_else(|e| panic!("stream={stream}: {e:?}"));
        let answer = match response.body {
            ResponseBody::Buffered(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            ResponseBody::Stream(mut s) => {
                let mut text = String::new();
                while let Some(item) = s.next().await {
                    text.push_str(&String::from_utf8_lossy(&item.unwrap()));
                }
                text
            }
        };
        assert_eq!(
            crate::reasoning_testkit::blocks(&answer),
            crate::reasoning_testkit::want(),
            "stream={stream}: {answer}"
        );
    }
}
