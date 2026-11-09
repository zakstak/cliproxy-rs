//! Native Codex Responses requests through the real router (#2, #23): on the `chatgpt` and
//! `api-key` profiles every Responses client's body reaches the upstream byte for byte,
//! with only the contract's permitted edits, and the upstream's answer reaches the client
//! byte for byte (`docs/codex/CONTRACT.md` "Native HTTP route"). Sign in with ChatGPT
//! takes the same route (#28). Loopback upstreams, fake tokens and a private `auth-dir`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::http::HeaderMap;
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_exec::Executors;

type Seen = Arc<Mutex<Vec<(HeaderMap, Bytes)>>>;

const SSE: &str = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n";
const CODEX_UA: &str = "codex_cli_rs/0.160.0 (Linux; x86_64) xterm-256color";
const REPLAY: &str = r#"["reasoning.encrypted_content","output.encrypted_content"]"#;

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

/// A private, empty `auth-dir`, removed when dropped: credential loading never falls back
/// to `~/.cli-proxy-api`.
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

fn credential(siwc: bool, base_url: &str) -> Credential {
    let mut metadata = serde_json::json!({
        "type": "codex",
        "access_token": "at-FAKE",
        "account_id": "acct-FAKE",
        "expired": "2099-01-01T00:00:00Z",
    });
    if siwc {
        metadata["use_siwc"] = true.into();
        metadata["siwc"] = serde_json::json!({"access_token": "siwc-at-FAKE", "client_id": "client-FAKE"});
    }
    let mut credential = Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/codex.json"),
        metadata.as_object().unwrap().clone(),
    )
    .unwrap();
    for (key, value) in [("base_url", base_url), ("plan_type", "plus")] {
        credential.attributes.insert(key.into(), value.into());
    }
    credential
}

struct Proxy {
    url: String,
    seen: Seen,
    _auth_dir: AuthDir,
}

async fn proxy(siwc: bool) -> Proxy {
    let seen: Seen = Arc::default();
    let record = seen.clone();
    let upstream = serve(axum::Router::new().fallback(move |headers: HeaderMap, body: Bytes| {
        let record = record.clone();
        async move {
            record.lock().unwrap().push((headers, body));
            ([("content-type", "text/event-stream")], SSE)
        }
    }))
    .await;
    let auth_dir = AuthDir::new();
    let cfg = Config::parse(&format!(
        "auth-dir: {}\naccess:\n  api-keys: [client-key]\n",
        auth_dir.0.display()
    ))
    .unwrap();
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        meta: Default::default(),
        xai: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(
        cfg,
        vec![credential(siwc, &upstream)],
        executors,
    ));
    Proxy {
        url: serve(cpa_server::router(rt)).await,
        seen,
        _auth_dir: auth_dir,
    }
}

/// A request in current Codex's field order (core/src/client.rs `build_request`). The
/// spaced `"text" :` key marks bytes that only an untouched pass-through keeps.
fn body(parallel: Option<bool>, include: &str) -> String {
    let parallel = parallel
        .map(|p| format!(r#","parallel_tool_calls":{p}"#))
        .unwrap_or_default();
    format!(
        r#"{{"model":"gpt-5.5","instructions":"be brief","input":[{{"type":"message","role":"user","content":[{{"type":"input_text","text":"hi"}}]}}],"tools":[{{"type":"function","name":"f","description":"d","strict":false,"parameters":{{"type":"object","properties":{{}}}}}}],"tool_choice":"auto"{parallel},"reasoning":{{"effort":"low"}},"store":false,"stream":true,"include":{include},"prompt_cache_key":"pck-1","prompt_cache_options":{{"ttl":"30m"}},  "text" : {{"verbosity":"low"}}}}"#
    )
}

/// Sends `body` with `headers` and returns what upstream received.
async fn send(proxy: &Proxy, headers: &[(&str, &str)], body: String) -> (HeaderMap, String) {
    let mut request = wreq::Client::new()
        .post(format!("{}/v1/responses", proxy.url))
        .header("authorization", "Bearer client-key")
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = request.body(body).send().await.unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    assert_eq!(status, 200, "{text}");
    let (headers, body) = proxy.seen.lock().unwrap().pop().expect("upstream request");
    (headers, String::from_utf8(body.to_vec()).unwrap())
}

/// A Sign in with ChatGPT credential takes the native route (#28): the client's body goes
/// upstream byte for byte, output replay and `prompt_cache_options` included. Go dropped
/// both, and forced its other SIWC body rules; no first-party source covers the profile,
/// so the proxy adds and removes nothing.
#[tokio::test]
async fn native_siwc_forwards_the_client_body() {
    let proxy = proxy(true).await;
    let sent_body = body(Some(false), REPLAY);
    let (headers, sent) = send(&proxy, &[("user-agent", CODEX_UA)], sent_body.clone()).await;
    assert_eq!(headers["authorization"], "Bearer siwc-at-FAKE");
    assert_eq!(sent, sent_body, "the client's bytes");
}

/// The Codex request boundary (#22) refuses an ambiguous or mistyped native body through
/// the real routes, with a client-safe 400, before anything is sent upstream.
#[tokio::test]
async fn codex_request_boundary_refuses_before_the_upstream_send() {
    let proxy = proxy(false).await;
    let cases = [
        (
            "/v1/responses",
            r#"{"model":"gpt-5.5","input":[{"type":"message","type":"message","role":"user","content":"hi"}]}"#,
            "request body repeats an object key at byte 46",
        ),
        (
            "/v1/responses",
            r#"{"model":"gpt-5.5","previous_response_id":7,"input":[]}"#,
            "request field `previous_response_id` must be a string or null",
        ),
        (
            "/v1/responses/compact",
            r#"{"model":"gpt-5.5","input":[],"input":"again"}"#,
            "request body repeats an object key at byte 30",
        ),
    ];
    for (path, body, message) in cases {
        let response = wreq::Client::new()
            .post(format!("{}{path}", proxy.url))
            .header("authorization", "Bearer client-key")
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap();
        assert_eq!(status, 400, "{path} {body}: {text}");
        assert!(text.contains(message), "{path} {body}: {text}");
    }
    assert!(
        proxy.seen.lock().unwrap().is_empty(),
        "a refused body reached the upstream"
    );
    // Compact used to delete a non-`true` `stream` before anything read it, so a mistyped
    // one was cleaned and accepted. The boundary reads the client's own bytes now.
    let response = wreq::Client::new()
        .post(format!("{}/v1/responses/compact", proxy.url))
        .header("authorization", "Bearer client-key")
        .body(r#"{"model":"gpt-5.5","stream":"yes","input":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 400);
    let text = response.text().await.unwrap();
    assert!(text.contains("request field `stream` must be a boolean"), "{text}");
    assert!(proxy.seen.lock().unwrap().is_empty());
    // A valid body on the same route still goes through.
    let (_, sent) = send(&proxy, &[], body(Some(false), REPLAY)).await;
    assert!(sent.contains(r#""model":"gpt-5.5""#), "{sent}");
}

/// The size rule through the real route (docs/codex/CONTRACT.md "Request boundary"): the
/// server's shared 64 MiB request limit (`cpa_server::MAX_REQUEST_BYTES`) reads the body
/// before any provider is known, so a body one byte over it gets the shared 400 with
/// axum's reason (docs/DIFFERENCES-FROM-GO.md "Request body limits"), never the
/// boundary's own 413, which only a caller of the executor that skips the server can
/// meet. A body of exactly 64 MiB passes that limit and meets the Codex boundary, here
/// for its repeated key. Neither reaches the upstream.
#[tokio::test]
async fn oversized_bodies_get_the_server_limit_before_the_boundary() {
    let proxy = proxy(false).await;
    let limit = cpa_server::MAX_REQUEST_BYTES;
    assert_eq!(limit, cpa_common::codex_body::MAX_BODY_BYTES);
    // A repeated key below the root: the shared ingress reads only `model` and `stream`.
    let head = br#"{"model":"gpt-5.5","input":[],"x":{"a":1,"a":2}"#;
    let mut exact = head.to_vec();
    exact.resize(limit - 1, b' ');
    exact.push(b'}');
    let mut over = exact.clone();
    over.insert(head.len(), b' ');
    for (body, status, message) in [
        (exact, 400, "request body repeats an object key at byte 41"),
        (
            over,
            400,
            "Invalid request: Failed to buffer the request body: length limit exceeded",
        ),
    ] {
        let len = body.len();
        let response = wreq::Client::new()
            .post(format!("{}/v1/responses", proxy.url))
            .header("authorization", "Bearer client-key")
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), status, "{len} bytes");
        let text = response.text().await.unwrap();
        assert!(text.contains(message), "{len} bytes: {text}");
    }
    assert!(proxy.seen.lock().unwrap().is_empty(), "nothing went upstream");
}

// The native HTTP route through the real router, with the shared capture upstream.

use cpa_exec::codex_contract::fixture;
use cpa_server::testing::codex_capture::{CaptureUpstream, Reply, Script};

struct Native {
    url: String,
    upstream: CaptureUpstream,
    rt: Arc<cpa_server::runtime::Runtime>,
    _auth_dir: AuthDir,
}

/// A proxy whose one Codex credential (`api_key` set for the `api-key` profile, else a
/// ChatGPT OAuth token) points at a capture upstream answering `script`.
async fn native(api_key: bool, script: Script, extra_config: &str) -> Native {
    let upstream = CaptureUpstream::start(script).await;
    let auth_dir = AuthDir::new();
    let cfg = Config::parse(&format!(
        "auth-dir: {}\naccess:\n  api-keys: [client-key]\n{extra_config}",
        auth_dir.0.display()
    ))
    .unwrap();
    let mut credential = credential(false, &upstream.base_url);
    if api_key {
        credential.attributes.insert("api_key".into(), "key-FAKE".into());
    }
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        meta: Default::default(),
        xai: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(cfg, vec![credential], executors));
    Native {
        url: serve(cpa_server::router(rt.clone())).await,
        upstream,
        rt,
        _auth_dir: auth_dir,
    }
}

/// A fixture body routed to a model the Codex catalog serves. The model value is the only
/// difference from the fixture.
fn routed(name: &str) -> String {
    String::from_utf8(fixture(name).bytes().to_vec()).unwrap().replacen(
        r#""model":"gpt-test""#,
        r#""model":"gpt-5.5""#,
        1,
    )
}

fn text_of(name: &str) -> String {
    String::from_utf8(fixture(name).bytes().to_vec()).unwrap()
}

async fn post(url: &str, path: &str, headers: &[(&str, &str)], body: String) -> (u16, HeaderMap, String) {
    let mut request = wreq::Client::new()
        .post(format!("{url}{path}"))
        .header("authorization", "Bearer client-key")
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = request.body(body).send().await.unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    (status, headers, response.text().await.unwrap())
}

const NATIVE_REQUESTS: [&str; 5] = [
    "http-request-full",
    "http-request-lite",
    "http-request-compaction",
    "http-request-minimal",
    "probe-request-unknown-fields",
];

/// Every Responses client, official or not, on both native profiles: the upstream gets
/// the client's bytes and the client gets the upstream's events, each byte for byte.
#[tokio::test]
async fn native_requests_and_events_pass_through_the_route_unchanged() {
    let codex = [("user-agent", CODEX_UA), ("originator", "codex_cli_rs")];
    let ordinary = [("user-agent", "OpenAI/Python 2.1.0"), ("originator", "")];
    for api_key in [false, true] {
        let replies = (0..NATIVE_REQUESTS.len() * 2)
            .map(|_| Reply::sse(fixture("sse-completed").bytes()))
            .collect();
        let proxy = native(
            api_key,
            Script {
                http: replies,
                ..Script::default()
            },
            "",
        )
        .await;
        for headers in [&codex[..], &ordinary[..1]] {
            for name in NATIVE_REQUESTS {
                let body = routed(name);
                let (status, response_headers, answer) = post(&proxy.url, "/v1/responses", headers, body.clone()).await;
                assert_eq!(status, 200, "{name}: {answer}");
                assert_eq!(response_headers["content-type"], "text/event-stream");
                assert_eq!(answer, text_of("sse-completed"), "{name}: client bytes");
                let sent = proxy.upstream.http().pop().unwrap();
                assert_eq!(String::from_utf8_lossy(&sent.body), body, "{name}: upstream bytes");
                assert_eq!(
                    sent.headers["authorization"],
                    if api_key { "Bearer key-FAKE" } else { "Bearer at-FAKE" },
                    "the client key never travels upstream"
                );
            }
        }
    }
}

/// Events the legacy framer dropped or rewrote for Codex clients reach them unchanged.
#[tokio::test]
async fn private_and_unknown_events_reach_codex_clients() {
    let stream = concat!(
        "event: codex.rate_limits\ndata: {\"type\":\"codex.rate_limits\",\"primary\":{\"used_percent\":1.50}}\n\n",
        "data: {\"type\":\"responsesapi.websocket_timing\",\"t\":1}\n\n",
        "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_9\"}}\n\n",
        "data: {\"type\":\"response.future_event\",\"n\":12345678901234567890}\n\n",
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_9\",\"output\":[]}}\n\n",
    );
    let proxy = native(
        false,
        Script {
            http: vec![Reply::sse(stream)],
            ..Script::default()
        },
        "",
    )
    .await;
    let (status, _, answer) = post(
        &proxy.url,
        "/v1/responses",
        &[("user-agent", CODEX_UA)],
        routed("http-request-full"),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(answer, stream);
}

/// A buffered client on the `chatgpt` profile: `stream: true` upstream (the only change)
/// and the terminal event's `response` object back, byte for byte. On an API key the
/// request and its JSON answer pass unchanged.
#[tokio::test]
async fn buffered_clients_get_the_destination_contract() {
    let full = routed("http-request-full");
    let buffered = full.replacen(r#""stream":true"#, r#""stream":false"#, 1);
    let terminal = String::from_utf8(fixture("sse-completed").frames().pop().unwrap().to_vec()).unwrap();
    let object = terminal[terminal.find(r#""response":"#).unwrap() + 11..terminal.len() - 1].to_owned();

    let proxy = native(
        false,
        Script {
            http: vec![Reply::sse(fixture("sse-completed").bytes())],
            ..Script::default()
        },
        "",
    )
    .await;
    let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], buffered.clone()).await;
    assert_eq!((status, answer.as_str()), (200, object.as_str()));
    let sent = proxy.upstream.http().pop().unwrap();
    assert_eq!(String::from_utf8_lossy(&sent.body), full, "only `stream` changes");

    let json = r#"{"id":"resp_1","object":"response","status":"completed","output":[],"usage":null,"x":1.50}"#;
    let proxy = native(
        true,
        Script {
            http: vec![Reply::json(200, json)],
            ..Script::default()
        },
        "",
    )
    .await;
    let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], buffered.clone()).await;
    assert_eq!((status, answer.as_str()), (200, json));
    let sent = proxy.upstream.http().pop().unwrap();
    assert_eq!(String::from_utf8_lossy(&sent.body), buffered);
}

/// Compaction keeps its body, `stream: false` included, and its answer.
#[tokio::test]
async fn compaction_passes_through_the_route_unchanged() {
    let answer =
        r#"{"id":"cmp_1","object":"response.compaction","output":[{"type":"compaction","encrypted_content":"ENC"}]}"#;
    let proxy = native(
        false,
        Script {
            http: vec![Reply::json(200, answer)],
            ..Script::default()
        },
        "",
    )
    .await;
    let body = routed("http-request-compaction").replacen(r#""stream":true"#, r#""stream":false"#, 1);
    let (status, _, got) = post(&proxy.url, "/v1/responses/compact", &[], body.clone()).await;
    assert_eq!((status, got.as_str()), (200, answer));
    let sent = proxy.upstream.http().pop().unwrap();
    assert_eq!(sent.uri.path(), "/responses/compact");
    assert_eq!(String::from_utf8_lossy(&sent.body), body);
}

/// An upstream error reaches the client with its body; the request-scoped context-window
/// failure arrives as the provider's own event.
#[tokio::test]
async fn upstream_failures_reach_the_client_unchanged() {
    let limit = text_of("http-error-usage-limit-429");
    let proxy = native(
        false,
        Script {
            http: vec![Reply::json(429, limit.clone())],
            ..Script::default()
        },
        "",
    )
    .await;
    let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], routed("http-request-full")).await;
    assert_eq!((status, answer.as_str()), (429, limit.as_str()));
    // The usage limit cools the only credential, so the next case gets its own proxy.
    let proxy = native(
        false,
        Script {
            http: vec![Reply::sse(fixture("sse-failed-context-window").bytes())],
            ..Script::default()
        },
        "",
    )
    .await;
    let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], routed("http-request-full")).await;
    assert_eq!(status, 200);
    assert_eq!(answer, text_of("sse-failed-context-window"));
}

/// Payload rules and the Codex-client ingress rewrite cannot reach a native body.
#[tokio::test]
async fn operator_rules_cannot_rewrite_native_bodies() {
    let proxy = native(
        false,
        Script {
            http: vec![Reply::sse(fixture("sse-completed").bytes())],
            ..Script::default()
        },
        "payload:\n  override:\n    - models: [{name: gpt-5.5}]\n      params: {instructions: injected, store: true}\n  filter:\n    - models: [{name: gpt-5.5}]\n      params: [prompt_cache_options, service_tier]\nupstream:\n  codex:\n    orphan-delegation-compatibility: true\nclient:\n  codex:\n    optimize-multi-agent-v2: true\n",
    )
    .await;
    let body = routed("http-request-full");
    let (status, _, _) = post(
        &proxy.url,
        "/v1/responses",
        &[("user-agent", CODEX_UA), ("x-openai-subagent", "collab_spawn")],
        body.clone(),
    )
    .await;
    assert_eq!(status, 200);
    let sent = proxy.upstream.http().pop().unwrap();
    assert_eq!(String::from_utf8_lossy(&sent.body), body);
}

/// The final failure after the attempts run out: the upstream's own status and body,
/// although the scheduler classified the usage limit as a 429 and cooled the credential.
#[tokio::test]
async fn the_final_failure_keeps_the_upstream_status_and_body() {
    let body =
        r#"{"error":{"type":"usage_limit_reached","message":null,"resets_at":12345678901234567890,"x":{"n":1.50}}}"#;
    let proxy = native(
        false,
        Script {
            http: vec![Reply::json(400, body)],
            ..Script::default()
        },
        "",
    )
    .await;
    let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], routed("http-request-full")).await;
    assert_eq!((status, answer.as_str()), (400, body));
    assert_eq!(proxy.upstream.http().len(), 1, "one attempt, no replay");
    // The credential is cooled as a usage limit: the next request does not reach it.
    let (status, _, _) = post(&proxy.url, "/v1/responses", &[], routed("http-request-full")).await;
    assert_eq!(status, 429);
    assert_eq!(proxy.upstream.http().len(), 1);
}

/// Codex's own client paths (`/backend-api/codex/responses` and its `/compact` form) are
/// the same native routes: the client path maps to the profile's `/responses` or
/// `/responses/compact` (the routing row of docs/codex/CONTRACT.md "Permitted
/// transformations"), and the body and answer pass unchanged.
#[tokio::test]
async fn codex_client_paths_take_the_same_native_routes() {
    let compacted = r#"{"id":"cmp_2","object":"response.compaction","output":[{"type":"compaction","encrypted_content":"ENC"}],"future":null}"#;
    for api_key in [false, true] {
        let proxy = native(
            api_key,
            Script {
                http: vec![
                    Reply::sse(fixture("sse-completed").bytes()),
                    Reply::json(200, compacted),
                ],
                ..Script::default()
            },
            "",
        )
        .await;
        let body = routed("http-request-full");
        let (status, _, answer) = post(
            &proxy.url,
            "/backend-api/codex/responses",
            &[("user-agent", CODEX_UA)],
            body.clone(),
        )
        .await;
        assert_eq!((status, answer.as_str()), (200, text_of("sse-completed").as_str()));
        let sent = proxy.upstream.http().pop().unwrap();
        assert_eq!(sent.uri.path(), "/responses", "api_key={api_key}");
        assert_eq!(String::from_utf8_lossy(&sent.body), body, "api_key={api_key}");

        let body = routed("http-request-compaction").replacen(r#""stream":true"#, r#""stream":false"#, 1);
        let (status, _, answer) = post(&proxy.url, "/backend-api/codex/responses/compact", &[], body.clone()).await;
        assert_eq!((status, answer.as_str()), (200, compacted), "api_key={api_key}");
        let sent = proxy.upstream.http().pop().unwrap();
        assert_eq!(sent.uri.path(), "/responses/compact", "api_key={api_key}");
        assert_eq!(String::from_utf8_lossy(&sent.body), body, "api_key={api_key}");
    }
}

/// A `zstd` request body (the pinned client compresses requests to the Codex backend,
/// core/src/client.rs:1932-1940): the server decodes it to read the routing fields, and
/// the upstream gets the decoded bytes unchanged, without the client's `Content-Encoding`
/// (docs/codex/CONTRACT.md "Permitted transformations", the decoding row). Nothing is
/// re-serialized: the probe's escape and raw number spellings survive.
#[tokio::test]
async fn a_zstd_body_reaches_the_upstream_decoded_and_unchanged() {
    let proxy = native(
        false,
        Script {
            http: vec![Reply::sse(fixture("sse-completed").bytes())],
            ..Script::default()
        },
        "",
    )
    .await;
    let body = routed("probe-request-unknown-fields");
    let compressed = zstd::encode_all(body.as_bytes(), 3).unwrap();
    let response = wreq::Client::new()
        .post(format!("{}/v1/responses", proxy.url))
        .header("authorization", "Bearer client-key")
        .header("content-type", "application/json")
        .header("content-encoding", "zstd")
        .header("user-agent", CODEX_UA)
        .body(compressed)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let answer = response.text().await.unwrap();
    assert_eq!((status, answer.as_str()), (200, text_of("sse-completed").as_str()));
    let sent = proxy.upstream.http().pop().unwrap();
    assert_eq!(String::from_utf8_lossy(&sent.body), body, "the decoded client bytes");
    assert!(
        sent.headers.get("content-encoding").is_none(),
        "decoded bytes never travel with the client's encoding: {:?}",
        sent.headers.get("content-encoding")
    );
}

/// Responses Lite from the official client: the Lite body (tools in a leading
/// `additional_tools` item, no top-level `tools`, `parallel_tool_calls: false`) with its
/// marker header reaches the upstream unchanged, and so does the marker. No top-level
/// tools, instructions or reasoning are manufactured.
#[tokio::test]
async fn responses_lite_keeps_its_body_and_marker() {
    let proxy = native(
        false,
        Script {
            http: vec![Reply::sse(fixture("sse-completed").bytes())],
            ..Script::default()
        },
        "",
    )
    .await;
    let body = routed("http-request-lite");
    let (status, _, answer) = post(
        &proxy.url,
        "/v1/responses",
        &[
            ("user-agent", CODEX_UA),
            ("originator", "codex_cli_rs"),
            ("x-openai-internal-codex-responses-lite", "true"),
        ],
        body.clone(),
    )
    .await;
    assert_eq!((status, answer.as_str()), (200, text_of("sse-completed").as_str()));
    let sent = proxy.upstream.http().pop().unwrap();
    assert_eq!(String::from_utf8_lossy(&sent.body), body);
    assert_eq!(sent.headers["x-openai-internal-codex-responses-lite"], "true");
}

/// Explicit routing on the `chatgpt` profile composes with the buffered adaptation: an
/// OAuth model alias changes only the `model` value, a buffered client's request also gets
/// `stream: true`, and nothing else changes. The client gets the terminal `response`
/// object byte for byte, with the upstream's model name (no `force-mapping` rewrite).
#[tokio::test]
async fn an_oauth_alias_and_the_buffered_adaptation_change_only_their_fields() {
    let proxy = native(
        false,
        Script {
            http: vec![Reply::sse(fixture("sse-completed").bytes())],
            ..Script::default()
        },
        "oauth-model-alias:\n  codex:\n    - name: gpt-5.5\n      alias: team-model\n      fork: false\n",
    )
    .await;
    let client = text_of("probe-request-unknown-fields")
        .replacen(r#""model":"gpt-test""#, r#""model":"team-model""#, 1)
        .replacen(r#""stream":true"#, r#""stream":false"#, 1);
    assert!(client.contains(r#""stream":false"#), "{client}");
    let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], client.clone()).await;
    let terminal = String::from_utf8(fixture("sse-completed").frames().pop().unwrap().to_vec()).unwrap();
    let object = &terminal[terminal.find(r#""response":"#).unwrap() + 11..terminal.len() - 1];
    assert_eq!((status, answer.as_str()), (200, object));
    let sent = proxy.upstream.http().pop().unwrap();
    let want = client
        .replacen(r#""model":"team-model""#, r#""model":"gpt-5.5""#, 1)
        .replacen(r#""stream":false"#, r#""stream":true"#, 1);
    assert_eq!(String::from_utf8_lossy(&sent.body), want, "only `model` and `stream`");
}

/// Inputs the native route cannot carry without a body change are refused through the
/// real routes with an actionable 400, before any upstream request: a thinking suffix on
/// the model, a streamed compaction, and an `image_generation` tool or tool choice while
/// `multimedia.disable-image-generation` strips it (Go stripped it from the body). The
/// same proxy still forwards a body without the tool unchanged.
#[tokio::test]
async fn unsupported_native_inputs_are_refused_before_the_send() {
    let proxy = native(
        false,
        Script {
            http: vec![Reply::sse(fixture("sse-completed").bytes())],
            ..Script::default()
        },
        "multimedia:\n  disable-image-generation: true\n",
    )
    .await;
    let full = routed("http-request-full");
    let with_tool = full.replacen(r#""tools":["#, r#""tools":[{"type":"image_generation"},"#, 1);
    let with_choice = full.replacen(
        r#""tool_choice":"auto""#,
        r#""tool_choice":{"type":"image_generation"}"#,
        1,
    );
    assert_ne!(with_tool, full);
    assert_ne!(with_choice, full);
    let cases = [
        (
            "/v1/responses",
            full.replacen(r#""model":"gpt-5.5""#, r#""model":"gpt-5.5(high)""#, 1),
            "model suffix `(high)` is not supported on native Codex Responses requests; set `reasoning` in the request body",
        ),
        (
            "/v1/responses/compact",
            routed("http-request-compaction"),
            "Streaming not supported for compact responses",
        ),
        ("/v1/responses", with_tool, "multimedia.disable-image-generation"),
        ("/v1/responses", with_choice, "multimedia.disable-image-generation"),
    ];
    for (path, body, message) in cases {
        for headers in [&[("user-agent", CODEX_UA)][..], &[]] {
            let (status, _, answer) = post(&proxy.url, path, headers, body.clone()).await;
            assert_eq!(status, 400, "{path} {message}: {answer}");
            assert!(answer.contains(message), "{path}: {answer}");
            assert!(proxy.upstream.http().is_empty(), "{path} {message}: nothing upstream");
        }
    }
    let (status, _, _) = post(&proxy.url, "/v1/responses", &[("user-agent", CODEX_UA)], full.clone()).await;
    assert_eq!(status, 200);
    let sent = proxy.upstream.http().pop().unwrap();
    assert_eq!(String::from_utf8_lossy(&sent.body), full);
}

/// #2, kept and broadened to the native contract. #2 kept an official Codex client's
/// `parallel_tool_calls` (false, true or omitted) and output-replay `include`; Go forced
/// `parallel_tool_calls: true` and dropped the replay for other Responses clients, forced
/// `false` under Responses Lite and dropped `prompt_cache_options`. Those Go-parity
/// assertions are replaced by the contract's: every client's body reaches the upstream
/// byte for byte (docs/codex/CONTRACT.md "Field preservation"), on the `chatgpt`,
/// `api-key` and Sign in with ChatGPT profiles, whichever headers identify the client.
#[tokio::test]
async fn issue2_parallelism_and_output_replay_pass_unchanged_for_every_client() {
    let clients: [&[(&str, &str)]; 5] = [
        &[("user-agent", CODEX_UA), ("originator", "codex_cli_rs")],
        &[("originator", "codex_cli_rs")],
        &[
            ("user-agent", CODEX_UA),
            ("x-openai-internal-codex-responses-lite", "true"),
        ],
        &[("user-agent", "OpenAI/Python 2.1.0")],
        &[],
    ];
    let includes = [REPLAY, r#"["reasoning.encrypted_content"]"#, "[]"];
    let cases = clients.len() * 3 * includes.len();
    for api_key in [false, true] {
        let replies = (0..cases)
            .map(|_| Reply::sse(fixture("sse-completed").bytes()))
            .collect();
        let proxy = native(
            api_key,
            Script {
                http: replies,
                ..Script::default()
            },
            "",
        )
        .await;
        for headers in clients {
            for parallel in [Some(false), Some(true), None] {
                for include in includes {
                    let client = body(parallel, include);
                    let (status, _, answer) = post(&proxy.url, "/v1/responses", headers, client.clone()).await;
                    assert_eq!(status, 200, "{answer}");
                    let sent = proxy.upstream.http().pop().unwrap();
                    assert_eq!(
                        String::from_utf8_lossy(&sent.body),
                        client,
                        "api_key={api_key} {headers:?} {parallel:?} {include}"
                    );
                }
            }
        }
    }
    let siwc = proxy(true).await;
    for headers in clients {
        for parallel in [Some(false), Some(true), None] {
            let client = body(parallel, REPLAY);
            let (auth, sent) = send(&siwc, headers, client.clone()).await;
            assert_eq!(auth["authorization"], "Bearer siwc-at-FAKE");
            assert_eq!(sent, client, "siwc {headers:?} {parallel:?}");
        }
    }
}

/// #25: first-party Codex derives its retry delay from a `rate_limit_exceeded` failure's
/// message text (`codex-api/src/sse/responses_error.rs:93-128` at 44ccf394573c), so the
/// fixture's message, irregular spacing included, must arrive unchanged. After the
/// response started the client gets the provider's event as sent. Before anything was
/// delivered the scheduler classifies it as a 429 (the credential cools, and with one
/// credential nothing else can serve it), and the streaming and the buffered client alike
/// get the provider's error object and sequence number as sent, under that status.
#[tokio::test]
async fn a_rate_limit_failure_keeps_the_message_codex_reads_its_delay_from() {
    let failed = text_of("sse-failed-rate-limit");
    let data = text(fixture("sse-failed-rate-limit").frames()[0].clone());
    let started = format!(
        "event: response.created\ndata: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp_rl\"}}}}\n\n{failed}"
    );
    let proxy = native(
        true,
        Script {
            http: vec![Reply::sse(started.clone())],
            ..Script::default()
        },
        "",
    )
    .await;
    let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], routed("http-request-full")).await;
    assert_eq!((status, answer.as_str()), (200, started.as_str()), "after the start");

    // The provider's error object, sliced from the fixture's own bytes.
    let error = &data[data.find(r#""error":"#).unwrap() + 8..data.find(r#", "usage""#).unwrap()];
    let delivered = format!(r#"{{"error":{error},"sequence_number":3}}"#);
    let full = routed("http-request-full");
    for body in [full.clone(), full.replacen(r#""stream":true"#, r#""stream":false"#, 1)] {
        let proxy = native(
            false,
            Script {
                http: vec![Reply::sse(failed.clone())],
                ..Script::default()
            },
            "",
        )
        .await;
        let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], body).await;
        assert_eq!((status, answer.as_str()), (429, delivered.as_str()), "before the start");
        assert_eq!(proxy.upstream.http().len(), 1, "one attempt, no replay");
    }
}

/// #25: usage that the upstream reports as zero stays zero, with its own spelling, in a
/// stream and in the buffered answer, and missing usage stays missing. Passive usage
/// accounting is on: it records each answer as a success from a copy and changes no byte.
#[tokio::test]
async fn reported_zero_and_missing_usage_pass_unchanged() {
    let zero = r#"{"type":"response.completed","response":{"id":"resp_z","status":"completed","output":[],"usage":{"input_tokens":0,"input_tokens_details":{"cached_tokens":0},"output_tokens":0,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":0}}}"#;
    let zero_stream = format!("event: response.completed\ndata: {zero}\n\n");
    let zero_object = &zero[zero.find(r#""response":"#).unwrap() + 11..zero.len() - 1];
    let none_stream = text_of("sse-completed-no-usage");
    let none_data = text(fixture("sse-completed-no-usage").frames().pop().unwrap());
    let none_object = &none_data[none_data.find(r#""response":"#).unwrap() + 11..none_data.len() - 1];
    let full = routed("http-request-full");
    let buffered = full.replacen(r#""stream":true"#, r#""stream":false"#, 1);
    for (stream, object) in [(zero_stream.as_str(), zero_object), (none_stream.as_str(), none_object)] {
        let proxy = native(
            false,
            Script {
                http: vec![Reply::sse(stream.to_owned()), Reply::sse(stream.to_owned())],
                ..Script::default()
            },
            "",
        )
        .await;
        let queue = proxy.rt.usage_queue();
        queue.configure(
            true,
            &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
        );
        let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], full.clone()).await;
        assert_eq!((status, answer.as_str()), (200, stream), "streamed");
        let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], buffered.clone()).await;
        assert_eq!((status, answer.as_str()), (200, object), "buffered");
        let mut records: Vec<serde_json::Value> = Vec::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while records.len() < 2 {
                records.extend(queue.pop_oldest(10).iter().map(|r| serde_json::from_slice(r).unwrap()));
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await;
        assert_eq!(records.len(), 2, "{records:?}");
        for record in &records {
            assert_eq!(record["failed"], false, "{record}");
            assert_eq!(record["tokens"]["total_tokens"], 0, "{record}");
        }
    }
}

fn text(bytes: Bytes) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// The buffered answer for an event whose `response` is its last member: that object,
/// sliced from the event's own bytes.
fn last_response_object(event: &str) -> &str {
    &event[event.find(r#""response":"#).unwrap() + 11..event.len() - 1]
}

/// The usage records published for `count` answers.
async fn usage_records(proxy: &Native, count: usize) -> Vec<serde_json::Value> {
    let queue = proxy.rt.usage_queue();
    let mut records: Vec<serde_json::Value> = Vec::new();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while records.len() < count {
            records.extend(queue.pop_oldest(10).iter().map(|r| serde_json::from_slice(r).unwrap()));
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    records.extend(queue.pop_oldest(10).iter().map(|r| serde_json::from_slice(r).unwrap()));
    records
}

/// #25 and #7: every `response.incomplete` the provider sends (the hand-written probe:
/// absent, null, empty, unknown and named reasons with zero output, missing usage, opaque
/// members, raw spellings and a duplicate key, partial output with partial usage)
/// reaches the client as sent at every stage: as the first event, after
/// `response.created`, and after output, with and without bootstrap buffering. A
/// streaming client gets the upstream's bytes; a buffered client gets the event's
/// `response` object byte for byte. The pinned client reads each end itself
/// (`codex-api/src/sse/responses.rs:451-506` at 44ccf394573c), so no 502, completion,
/// usage or output is invented. Each answer is one attempt, recorded once by passive
/// accounting as the provider's terminal event, and the credential does not cool.
#[tokio::test]
async fn every_incomplete_reaches_http_clients_as_sent() {
    let created = concat!(
        "event: response.created\n",
        r#"data: {"type":"response.created","response":{"id":"resp_inc"}}"#,
        "\n\n"
    );
    let output = concat!(
        "event: response.output_text.delta\n",
        r#"data: {"type":"response.output_text.delta","item_id":"msg_inc_1","output_index":0,"content_index":0,"delta":"Hel"}"#,
        "\n\n"
    );
    let full = routed("http-request-full");
    let buffered = full.replacen(r#""stream":true"#, r#""stream":false"#, 1);
    let ends: Vec<String> = fixture("probe-sse-incomplete-ends")
        .frames()
        .into_iter()
        .map(text)
        .collect();
    assert_eq!(ends.len(), 11);
    for end in &ends {
        let terminal = format!("event: response.incomplete\ndata: {end}\n\n");
        for (stage, stream) in [
            ("first event", terminal.clone()),
            ("after response.created", format!("{created}{terminal}")),
            ("after output", format!("{created}{output}{terminal}")),
        ] {
            for (api_key, config) in [
                (false, ""),
                (false, "codex:\n  stream-bootstrap-buffering: true\n"),
                (true, ""),
            ] {
                let case = format!("{stage} api_key={api_key} {config:?}: {end}");
                let proxy = native(
                    api_key,
                    Script {
                        http: vec![Reply::sse(stream.clone()), Reply::sse(stream.clone())],
                        ..Script::default()
                    },
                    config,
                )
                .await;
                proxy.rt.usage_queue().configure(
                    true,
                    &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
                );
                let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], full.clone()).await;
                assert_eq!((status, answer.as_str()), (200, stream.as_str()), "{case}: streamed");
                let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], buffered.clone()).await;
                assert_eq!(
                    (status, answer.as_str()),
                    (200, last_response_object(end)),
                    "{case}: buffered"
                );
                assert_eq!(proxy.upstream.http().len(), 2, "{case}: one attempt each");
                let records = usage_records(&proxy, 2).await;
                assert_eq!(records.len(), 2, "{case}: {records:?}");
                assert!(records.iter().all(|r| r["failed"] == false), "{case}: {records:?}");
                let credential = proxy.rt.store().snapshot()[0].id.clone();
                assert!(proxy.rt.store().cooldowns(&credential).is_empty(), "{case}");
            }
        }
    }
}

/// #25: a remote compaction v2 answer. The pinned client sends a `compaction_trigger`
/// request through its ordinary Responses stream on `/responses` and requires one
/// `compaction` output item before `response.completed`
/// (`core/src/compact_remote_v2.rs:291-492` at 44ccf394573c). The first-party test's
/// answer (`sse-compaction-v2`) and the opaque probe reach a streaming client byte for
/// byte; a buffered client gets the completion's `response` object byte for byte (the
/// destination contract; `chatgpt` acceptance of `stream: false` is unknown). The
/// request reaches the upstream as sent. Both native profiles a loopback can stand for.
#[tokio::test]
async fn compaction_v2_answers_pass_unchanged() {
    let request = routed("http-request-compaction");
    let buffered = request.replacen(r#""stream":true"#, r#""stream":false"#, 1);
    for name in ["sse-compaction-v2", "probe-sse-compaction-v2-opaque"] {
        let stream = text_of(name);
        let completed = text(fixture(name).frames().pop().unwrap());
        for api_key in [false, true] {
            let case = format!("{name} api_key={api_key}");
            let proxy = native(
                api_key,
                Script {
                    http: vec![Reply::sse(stream.clone()), Reply::sse(stream.clone())],
                    ..Script::default()
                },
                "",
            )
            .await;
            let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], request.clone()).await;
            assert_eq!((status, answer.as_str()), (200, stream.as_str()), "{case}: streamed");
            let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], buffered.clone()).await;
            assert_eq!(
                (status, answer.as_str()),
                (200, last_response_object(&completed)),
                "{case}: buffered"
            );
            let sent = proxy.upstream.http();
            assert_eq!(sent.len(), 2, "{case}");
            assert_eq!(sent[0].uri.path(), "/responses", "{case}");
            assert_eq!(
                String::from_utf8_lossy(&sent[0].body),
                request,
                "{case}: the request as sent"
            );
        }
    }
}
