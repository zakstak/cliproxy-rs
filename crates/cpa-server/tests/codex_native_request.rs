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

/// The upstream completion each undecodable event is followed by.
const REAL_END: &str = r#"{"type":"response.completed","response":{"id":"resp_real","status":"completed","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;

/// Upstream events the pinned client does not decode into `ResponsesStreamEvent`
/// (`codex-api/src/sse/responses.rs:161-179` and 633-643 at 44ccf394573c), so it skips
/// them: a repeated known member in either order, a known member of the wrong type, and
/// invalid JSON that would otherwise read as a failure, a context-window `error`, an
/// account failure or a completion.
const UNDECODABLE: [(&str, &str); 10] = [
    (
        "repeated type, terminal first",
        r#"{"type":"response.completed","type":"response.future","response":{"id":"resp_dup","output":[]}}"#,
    ),
    (
        "repeated type, terminal last",
        r#"{"type":"response.future","type":"response.completed","response":{"id":"resp_dup","output":[]}}"#,
    ),
    (
        "repeated response",
        r#"{"type":"response.completed","response":{"id":"resp_old","output":[]},"response":{"id":"resp_new","output":[]}}"#,
    ),
    (
        "repeated type on a failure",
        r#"{"type":"response.failed","type":"response.future","response":{"error":{"code":"server_error","message":"m"}}}"#,
    ),
    (
        "repeated error",
        r#"{"type":"error","error":{"code":"context_length_exceeded","message":"a"},"error":{"code":"context_length_exceeded","message":"b"}}"#,
    ),
    (
        "known member of the wrong type",
        r#"{"type":"response.completed","delta":5,"response":{"id":"resp_typed","output":[]}}"#,
    ),
    (
        "malformed failure",
        r#"{"type":"response.failed","response":{"error":{"code":"server_error","message":"broken"}}"#,
    ),
    (
        "malformed context window",
        r#"{"type":"error","error":{"code":"context_length_exceeded","message":"broken"}"#,
    ),
    (
        "malformed account failure",
        r#"{"type":"response.failed","response":{"error":{"code":"rate_limit_exceeded","message":"limit for acct-FAKE"}}"#,
    ),
    (
        "malformed completion",
        r#"{"type":"response.completed","response":{"id":"resp_bad","output":[]}"#,
    ),
];

/// #25 (F1, F3): an upstream event the pinned client cannot decode never ends, fails or
/// converts a native HTTP response, and it feeds no usage. It reaches a streaming client
/// as sent, and the later real completion follows it. A buffered client gets that
/// completion's `response` object. This holds as the first event and after
/// `response.created`, with and without bootstrap buffering, for a Codex client (which
/// would get #5's conversion). Each request is one attempt, recorded once with the
/// completion's usage, and the credential does not cool.
#[tokio::test]
async fn undecodable_upstream_events_never_end_or_fail_http_responses() {
    let created = concat!(
        "event: response.created\n",
        r#"data: {"type":"response.created","response":{"id":"resp_real"}}"#,
        "\n\n"
    );
    let real = format!("event: response.completed\ndata: {REAL_END}\n\n");
    let full = routed("http-request-full");
    let buffered = full.replacen(r#""stream":true"#, r#""stream":false"#, 1);
    for (name, event) in UNDECODABLE {
        for (stage, leading) in [("first event", ""), ("after response.created", created)] {
            for config in ["", "codex:\n  stream-bootstrap-buffering: true\n"] {
                let case = format!("{name}, {stage}, {config:?}");
                let stream = format!("{leading}data: {event}\n\n{real}");
                let proxy = native(
                    false,
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
                let headers = [("user-agent", CODEX_UA), ("originator", "codex_cli_rs")];
                let (status, _, answer) = post(&proxy.url, "/v1/responses", &headers, full.clone()).await;
                assert_eq!((status, answer.as_str()), (200, stream.as_str()), "{case}: streamed");
                let (status, _, answer) = post(&proxy.url, "/v1/responses", &headers, buffered.clone()).await;
                assert_eq!(
                    (status, answer.as_str()),
                    (200, last_response_object(REAL_END)),
                    "{case}: buffered"
                );
                assert_eq!(proxy.upstream.http().len(), 2, "{case}: one attempt each");
                let records = usage_records(&proxy, 2).await;
                assert_eq!(records.len(), 2, "{case}: {records:?}");
                for record in &records {
                    assert_eq!(record["failed"], false, "{case}: {records:?}");
                    assert_eq!(record["tokens"]["total_tokens"], 5, "{case}: {records:?}");
                }
                let credential = proxy.rt.store().snapshot()[0].id.clone();
                assert!(proxy.rt.store().cooldowns(&credential).is_empty(), "{case}");
            }
        }
    }
}

/// The controls for the boundary above, through the same route. Decodable events with
/// repeated members the pinned client ignores (unknown members, and keys repeated inside
/// `response`, where its `Value` keeps the last) still end the response: the stream ends
/// with that terminal event, and nothing the upstream sends after it is read. A buffered
/// client gets the `response` object as sent, repeats included. A valid status-less
/// context-window `error` still reaches a Codex client as `response.failed` (#5), and a
/// valid account failure still cools the credential (#6).
#[tokio::test]
async fn decodable_events_keep_their_outcomes_on_http_responses() {
    let end = r#"{"type":"response.completed","future":1,"future":{"n":1.50},"response":{"id":"resp_rep","id":"resp_rep_last","output":[],"usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5},"usage":null}}"#;
    let after =
        r#"{"type":"response.output_text.delta","item_id":"m","output_index":0,"content_index":0,"delta":"late"}"#;
    let terminal = format!("event: response.completed\ndata: {end}\n\n");
    let stream = format!("{terminal}data: {after}\n\n");
    let full = routed("http-request-full");
    let buffered = full.replacen(r#""stream":true"#, r#""stream":false"#, 1);
    let proxy = native(
        false,
        Script {
            http: vec![Reply::sse(stream.clone()), Reply::sse(stream)],
            ..Script::default()
        },
        "",
    )
    .await;
    let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], full.clone()).await;
    assert_eq!((status, answer.as_str()), (200, terminal.as_str()));
    let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], buffered).await;
    assert_eq!((status, answer.as_str()), (200, last_response_object(end)));

    // #5: the valid status-less context-window `error`, renamed for a Codex client.
    let context = r#"{"type":"error","error":{"code":"context_length_exceeded","message":"too long"},"response_id":"resp_ctx","sequence_number":2}"#;
    let proxy = native(
        false,
        Script {
            http: vec![Reply::sse(format!("data: {context}\n\ndata: {REAL_END}\n\n"))],
            ..Script::default()
        },
        "",
    )
    .await;
    let headers = [("user-agent", CODEX_UA), ("originator", "codex_cli_rs")];
    let (status, _, answer) = post(&proxy.url, "/v1/responses", &headers, full.clone()).await;
    assert_eq!(status, 200);
    assert_eq!(
        answer,
        r#"data: {"type":"response.failed","sequence_number":2,"response":{"id":"resp_ctx","object":"response","status":"failed","error":{"code":"context_length_exceeded","message":"too long"}}}"#
            .to_owned()
            + "\n\n"
    );

    // #6: a valid account failure before anything was delivered is classified, cools the
    // only credential and reaches the client as the provider's error object.
    let limit = r#"{"type":"response.failed","response":{"error":{"code":"rate_limit_exceeded","message":"limit"}}}"#;
    let proxy = native(
        false,
        Script {
            http: vec![Reply::sse(format!("data: {limit}\n\ndata: {REAL_END}\n\n"))],
            ..Script::default()
        },
        "",
    )
    .await;
    let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], full).await;
    assert_eq!(
        (status, answer.as_str()),
        (429, r#"{"error":{"code":"rate_limit_exceeded","message":"limit"}}"#)
    );
    let credential = proxy.rt.store().snapshot()[0].id.clone();
    assert!(
        !proxy.rt.store().cooldowns(&credential).is_empty(),
        "the account failure cools"
    );
}

/// Events the pinned client decodes or refuses differently from a plain JSON reading, each
/// followed by [`REAL_END`]: E1, objects whose first key is serde_json's private number or
/// raw-value token with anything but its one valid value, which the first-party build's
/// `Value` (`arbitrary_precision`, `raw_value`) refuses, so the event does not decode;
/// E4, decoded events the pinned reader does not end the response on: a
/// `response.completed` without a `response` (`sse/responses.rs:507-519` at
/// 44ccf394573c), and `response.done`, which it does not handle.
const NOT_TERMINAL: [(&str, &str); 7] = [
    (
        "invalid private number",
        r#"{"type":"response.completed","response":{"id":"bad","future":{"$serde_json::private::Number":"bad"}}}"#,
    ),
    (
        "private number with a second member",
        r#"{"type":"response.completed","response":{"id":"bad","output":[],"n":{"$serde_json::private::Number":"1","x":3}}}"#,
    ),
    (
        "private number that is no string",
        r#"{"type":"response.completed","error":{"$serde_json::private::Number":12},"response":{"id":"bad","output":[]}}"#,
    ),
    (
        "private raw value that is no JSON",
        r#"{"type":"response.completed","response":{"id":"bad","output":[{"$serde_json::private::RawValue":"{"}]}}"#,
    ),
    (
        "completed with a null response",
        r#"{"type":"response.completed","response":null}"#,
    ),
    (
        "completed without a response",
        r#"{"type":"response.completed","sequence_number":4}"#,
    ),
    (
        "response.done",
        r#"{"type":"response.done","response":{"id":"resp_done","status":"completed","output":[],"usage":{"input_tokens":9,"output_tokens":9,"total_tokens":18}}}"#,
    ),
];

/// #25 (E1, E4): none of [`NOT_TERMINAL`] ends a native HTTP response. A streaming client
/// gets it as sent and then the real completion; a buffered (`chatgpt`) client gets the
/// real completion's `response` object. As the first event and after `response.created`,
/// with and without bootstrap buffering. Each request is one attempt, recorded once with
/// the real completion's usage, and nothing cools. The controls: the same private tokens
/// with their valid values, a marker that is not an object's first key, and one in an
/// unknown member, decode, so that completion ends the response and the real one after it
/// is not read.
#[tokio::test]
async fn pinned_non_terminal_events_never_end_http_responses() {
    let created = concat!(
        "event: response.created\n",
        r#"data: {"type":"response.created","response":{"id":"resp_real"}}"#,
        "\n\n"
    );
    let real = format!("event: response.completed\ndata: {REAL_END}\n\n");
    let full = routed("http-request-full");
    let buffered = full.replacen(r#""stream":true"#, r#""stream":false"#, 1);
    for (name, event) in NOT_TERMINAL {
        for (stage, leading) in [("first event", ""), ("after response.created", created)] {
            for config in ["", "codex:\n  stream-bootstrap-buffering: true\n"] {
                let case = format!("{name}, {stage}, {config:?}");
                let stream = format!("{leading}data: {event}\n\n{real}");
                let proxy = native(
                    false,
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
                    (200, last_response_object(REAL_END)),
                    "{case}: buffered"
                );
                assert_eq!(proxy.upstream.http().len(), 2, "{case}: one attempt each");
                let records = usage_records(&proxy, 2).await;
                assert_eq!(records.len(), 2, "{case}: {records:?}");
                for record in &records {
                    assert_eq!(record["failed"], false, "{case}: {records:?}");
                    assert_eq!(record["tokens"]["total_tokens"], 5, "{case}: {records:?}");
                }
                let credential = proxy.rt.store().snapshot()[0].id.clone();
                assert!(proxy.rt.store().cooldowns(&credential).is_empty(), "{case}");
            }
        }
    }
    for end in [
        r#"{"type":"response.completed","response":{"id":"resp_ok","output":[],"n":{"$serde_json::private::Number":"-1.5e+3"}}}"#,
        r#"{"type":"response.completed","response":{"id":"resp_ok","output":[{"$serde_json::private::RawValue":"{\"a\":[1e400]}"}]}}"#,
        r#"{"type":"response.completed","response":{"id":"resp_ok","output":[],"n":{"x":1,"$serde_json::private::Number":"bad"}}}"#,
        r#"{"type":"response.completed","future":{"$serde_json::private::Number":"bad"},"response":{"id":"resp_ok","output":[]}}"#,
    ] {
        let terminal = format!("data: {end}\n\n");
        let proxy = native(
            false,
            Script {
                http: vec![
                    Reply::sse(format!("{terminal}{real}")),
                    Reply::sse(format!("{terminal}{real}")),
                ],
                ..Script::default()
            },
            "",
        )
        .await;
        let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], full.clone()).await;
        assert_eq!((status, answer.as_str()), (200, terminal.as_str()), "{end}");
        let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], buffered.clone()).await;
        assert_eq!((status, answer.as_str()), (200, last_response_object(end)), "{end}");
    }
}

/// #25 (E2) on the native HTTP route: a failure is classified as the pinned `Value` holds
/// it, with the last of a repeated key. A request fault first and a rate limit last is a
/// rate limit: classified 429, the credential cools. The reverse order is a request fault:
/// classified 400, delivered in the stream as sent, nothing cools. A context-window code first and `invalid_api_key` last is an
/// authentication failure, not #5's context window, so a Codex client gets no
/// `response.failed` conversion; the reverse order is the context window and gets it.
/// An account failure before anything was delivered reaches the client as the provider's
/// error object (the HTTP contract).
#[tokio::test]
async fn http_failures_are_classified_with_the_last_repeated_key() {
    let full = routed("http-request-full");
    let headers = [("user-agent", CODEX_UA), ("originator", "codex_cli_rs")];
    for (event, status, cools) in [
        (
            r#"{"type":"response.failed","response":{"id":"r1","error":{"type":"invalid_request_error","type":"rate_limit_error","code":"other","message":"limited"}}}"#,
            429,
            true,
        ),
        (
            r#"{"type":"response.failed","response":{"id":"r1","error":{"type":"rate_limit_error","type":"invalid_request_error","code":"other","message":"bad input"}}}"#,
            400,
            false,
        ),
        (
            r#"{"type":"error","error":{"code":"context_length_exceeded","code":"invalid_api_key","message":"limited"},"response_id":"r1"}"#,
            401,
            true,
        ),
    ] {
        let proxy = native(
            false,
            Script {
                http: vec![Reply::sse(format!("data: {event}\n\ndata: {REAL_END}\n\n"))],
                ..Script::default()
            },
            "",
        )
        .await;
        let (got, _, answer) = post(&proxy.url, "/v1/responses", &headers, full.clone()).await;
        if status == 400 {
            // A request fault does not fail over: the stream delivers the event as sent.
            assert_eq!((got, answer), (200, format!("data: {event}\n\n")), "{event}");
        } else {
            assert_eq!(got, status, "{event}: {answer}");
            let error = cpa_common::rawjson::get(event, "response.error");
            let error = if error.exists() {
                error
            } else {
                cpa_common::rawjson::get(event, "error")
            };
            assert_eq!(answer, format!(r#"{{"error":{}}}"#, error.json()), "{event}");
        }
        let credential = proxy.rt.store().snapshot()[0].id.clone();
        assert_eq!(!proxy.rt.store().cooldowns(&credential).is_empty(), cools, "{event}");
    }
    // The context window last: #5's conversion for a Codex client, the bytes as sent.
    let context = r#"{"type":"error","error":{"code":"invalid_api_key","code":"context_length_exceeded","message":"too long"},"response_id":"resp_ctx"}"#;
    let proxy = native(
        false,
        Script {
            http: vec![Reply::sse(format!("data: {context}\n\ndata: {REAL_END}\n\n"))],
            ..Script::default()
        },
        "",
    )
    .await;
    let (status, _, answer) = post(&proxy.url, "/v1/responses", &headers, full).await;
    assert_eq!(status, 200);
    assert_eq!(
        answer,
        r#"data: {"type":"response.failed","response":{"id":"resp_ctx","object":"response","status":"failed","error":{"code":"invalid_api_key","code":"context_length_exceeded","message":"too long"}}}"#
            .to_owned()
            + "\n\n"
    );
    let credential = proxy.rt.store().snapshot()[0].id.clone();
    assert!(proxy.rt.store().cooldowns(&credential).is_empty());
}

/// Decoded terminal-shaped events the pinned SSE reader records as an error and reads past
/// (`codex-api/src/sse/responses.rs:667-690` at 44ccf394573c): a `response` that does not
/// read as `ResponseCompleted` (lines 109-159 and 494-520: no string `id`, a malformed
/// `usage`, `usage_metadata` or `end_turn`), on a `response.completed` or an interrupted
/// `response.incomplete`, and a content-filter `response.incomplete` (lines 461-463). Each
/// reports usage of 9 + 9 = 18 where its `usage` reads.
const READ_ON: [(&str, &str); 10] = [
    (
        "completed without an id",
        r#"{"type":"response.completed","response":{"status":"completed","output":[],"usage":{"input_tokens":9,"output_tokens":9,"total_tokens":18}}}"#,
    ),
    (
        "completed with a numeric id",
        r#"{"type":"response.completed","response":{"id":7,"status":"completed","output":[],"usage":{"input_tokens":9,"output_tokens":9,"total_tokens":18}}}"#,
    ),
    (
        "completed with a null id",
        r#"{"type":"response.completed","response":{"id":null,"status":"completed","output":[],"usage":{"input_tokens":9,"output_tokens":9,"total_tokens":18}}}"#,
    ),
    (
        "completed with a string token count",
        r#"{"type":"response.completed","response":{"id":"resp_early","status":"completed","output":[],"usage":{"input_tokens":"9","output_tokens":9,"total_tokens":18}}}"#,
    ),
    (
        "completed without total_tokens",
        r#"{"type":"response.completed","response":{"id":"resp_early","status":"completed","output":[],"usage":{"input_tokens":9,"output_tokens":9}}}"#,
    ),
    (
        "completed with a null cached_tokens",
        r#"{"type":"response.completed","response":{"id":"resp_early","status":"completed","output":[],"usage":{"input_tokens":9,"input_tokens_details":{"cached_tokens":null},"output_tokens":9,"total_tokens":18}}}"#,
    ),
    (
        "completed with a numeric usage_metadata amount",
        r#"{"type":"response.completed","response":{"id":"resp_early","status":"completed","output":[],"usage_metadata":{"amount":5},"usage":{"input_tokens":9,"output_tokens":9,"total_tokens":18}}}"#,
    ),
    (
        "completed with a string end_turn",
        r#"{"type":"response.completed","response":{"id":"resp_early","status":"completed","end_turn":"yes","output":[],"usage":{"input_tokens":9,"output_tokens":9,"total_tokens":18}}}"#,
    ),
    (
        "content filter",
        r#"{"type":"response.incomplete","response":{"id":"resp_early","status":"incomplete","incomplete_details":{"reason":"content_filter"},"output":[],"usage":{"input_tokens":9,"output_tokens":9,"total_tokens":18}}}"#,
    ),
    (
        "interrupted without an id",
        r#"{"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"interrupted"},"output":[],"usage":{"input_tokens":9,"output_tokens":9,"total_tokens":18}}}"#,
    ),
];

/// #25 (E4, read-on): none of [`READ_ON`] ends a native HTTP response, because the pinned
/// SSE reader reads on after each and ends on the real completion that follows. As the
/// first event, after `response.created` and after output, with and without bootstrap
/// buffering: a streaming client receives every event as sent, the candidate and the real
/// completion included; a buffered client receives the real completion's `response`
/// object byte for byte; each answer is one attempt with one record that is not failed
/// and carries the real completion's usage (5), not the candidate's (18); nothing cools.
#[tokio::test]
async fn read_on_events_leave_http_responses_to_the_real_completion() {
    let created = concat!(
        "event: response.created\n",
        r#"data: {"type":"response.created","response":{"id":"resp_real"}}"#,
        "\n\n"
    );
    let output = concat!(
        r#"data: {"type":"response.created","response":{"id":"resp_real"}}"#,
        "\n\n",
        r#"data: {"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"partial"}"#,
        "\n\n"
    );
    let real = format!("event: response.completed\ndata: {REAL_END}\n\n");
    let full = routed("http-request-full");
    let buffered = full.replacen(r#""stream":true"#, r#""stream":false"#, 1);
    for (name, event) in READ_ON {
        for (stage, leading) in [
            ("first event", ""),
            ("after response.created", created),
            ("after output", output),
        ] {
            for config in ["", "codex:\n  stream-bootstrap-buffering: true\n"] {
                let case = format!("{name}, {stage}, {config:?}");
                let stream = format!("{leading}data: {event}\n\n{real}");
                let proxy = native(
                    false,
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
                    (200, last_response_object(REAL_END)),
                    "{case}: buffered"
                );
                assert_eq!(proxy.upstream.http().len(), 2, "{case}: one attempt each");
                let records = usage_records(&proxy, 2).await;
                assert_eq!(records.len(), 2, "{case}: {records:?}");
                for record in &records {
                    assert_eq!(record["failed"], false, "{case}: {records:?}");
                    assert_eq!(record["tokens"]["total_tokens"], 5, "{case}: {records:?}");
                }
                let credential = proxy.rt.store().snapshot()[0].id.clone();
                assert!(proxy.rt.store().cooldowns(&credential).is_empty(), "{case}");
            }
        }
    }
}

/// #25 (read-on) at the end of the upstream stream: when no usable terminal event follows
/// a [`READ_ON`] event, the response ends where the upstream ended it, on the last such
/// event, which is the provider's own end. The pinned SSE reader then reports the error it
/// recorded for that event (`sse/responses.rs:616-621`), which the client reads from the
/// event itself, so nothing is added: a streaming client gets the events as sent and a
/// clean end, a buffered client gets the last such event's `response` object, and the one
/// record carries that event's reported usage. Two in a row keep the last (the reader
/// keeps the last recorded error). Controls: a stream that ends after
/// `response.created` alone still fails, with the request-scoped 408, and each event the
/// pinned reader ends on (a readable completion with missing, null or empty usage
/// details, a readable interruption, and any other incomplete reason, even with an
/// unreadable `usage`) still ends the response there, so a later completion is not read.
#[tokio::test]
async fn read_on_events_end_http_responses_only_at_the_upstream_end() {
    let created = concat!(
        "event: response.created\n",
        r#"data: {"type":"response.created","response":{"id":"resp_real"}}"#,
        "\n\n"
    );
    let full = routed("http-request-full");
    let buffered = full.replacen(r#""stream":true"#, r#""stream":false"#, 1);
    let later = r#"{"type":"response.incomplete","response":{"id":"resp_later","status":"incomplete","incomplete_details":{"reason":"content_filter"},"output":[],"usage":{"input_tokens":20,"output_tokens":10,"total_tokens":30}}}"#;
    let cases = [
        (READ_ON[0].1, None, 18),
        (READ_ON[8].1, None, 18),
        (READ_ON[9].1, None, 18),
        (READ_ON[0].1, Some(later), 30),
    ];
    for (event, second, total) in cases {
        let last = second.unwrap_or(event);
        let stream = match second {
            None => format!("{created}data: {event}\n\n"),
            Some(second) => format!("{created}data: {event}\n\ndata: {second}\n\n"),
        };
        let proxy = native(
            false,
            Script {
                http: vec![Reply::sse(stream.clone()), Reply::sse(stream.clone())],
                ..Script::default()
            },
            "",
        )
        .await;
        proxy.rt.usage_queue().configure(
            true,
            &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
        );
        let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], full.clone()).await;
        assert_eq!((status, answer.as_str()), (200, stream.as_str()), "{last}: streamed");
        let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], buffered.clone()).await;
        assert_eq!(
            (status, answer.as_str()),
            (200, last_response_object(last)),
            "{last}: buffered"
        );
        let records = usage_records(&proxy, 2).await;
        assert_eq!(records.len(), 2, "{last}: {records:?}");
        for record in &records {
            assert_eq!(record["failed"], false, "{last}: {records:?}");
            assert_eq!(record["tokens"]["total_tokens"], total, "{last}: {records:?}");
        }
    }
    // Nothing the client could read as an end: the truncated response still fails.
    let proxy = native(
        false,
        Script {
            http: vec![Reply::sse(created.to_owned())],
            ..Script::default()
        },
        "",
    )
    .await;
    let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], buffered.clone()).await;
    assert_eq!(status, 408, "{answer}");
    assert!(answer.contains("stream closed before response.completed"), "{answer}");
    // The pinned reader ends on these: the completion after them is not read.
    let real = format!("data: {REAL_END}\n\n");
    for end in [
        r#"{"type":"response.completed","response":{"id":"resp_ok","status":"completed","output":[]}}"#,
        r#"{"type":"response.completed","response":{"id":"resp_ok","status":"completed","output":[],"usage":null}}"#,
        r#"{"type":"response.completed","response":{"id":"resp_ok","status":"completed","output":[],"usage":{"input_tokens":1,"input_tokens_details":null,"output_tokens":1,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":2,"codex_rollout_budget_units":1e400},"end_turn":null}}"#,
        r#"{"type":"response.incomplete","response":{"id":"resp_ok","status":"incomplete","incomplete_details":{"reason":"interrupted"},"output":[]}}"#,
        r#"{"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[],"usage":{"input_tokens":"bad"}}}"#,
    ] {
        let stream = format!("{created}data: {end}\n\n{real}");
        let proxy = native(
            false,
            Script {
                http: vec![Reply::sse(stream.clone()), Reply::sse(stream)],
                ..Script::default()
            },
            "",
        )
        .await;
        let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], full.clone()).await;
        assert_eq!(
            (status, answer),
            (200, format!("{created}data: {end}\n\n")),
            "{end}: streamed"
        );
        let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], buffered.clone()).await;
        assert_eq!(
            (status, answer.as_str()),
            (200, last_response_object(end)),
            "{end}: buffered"
        );
    }
}

/// How [`chunked_upstream`] ends its one answer after the first chunk.
#[derive(Clone, Copy, Debug)]
enum Ending {
    /// The terminating chunk: a clean end of the stream.
    Clean,
    /// One more chunk with `REAL_END`, then the terminating chunk.
    Later,
    /// The connection closes without the terminating chunk: a read failure.
    Broken,
}

/// An upstream that answers its one request with a chunked event stream whose first chunk
/// is `first`, then ends as `ending` says. The capture upstream always ends cleanly; this
/// one can break the body after a whole event.
async fn chunked_upstream(first: String, ending: Ending) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (mut conn, _) = listener.accept().await.unwrap();
        // The request: its head, then a `content-length` body.
        let mut request = Vec::new();
        loop {
            let mut buf = [0; 8192];
            let n = conn.read(&mut buf).await.unwrap();
            if n == 0 {
                return;
            }
            request.extend_from_slice(&buf[..n]);
            if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&request[..end]).into_owned();
                let length: usize = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .unwrap_or(0);
                if request.len() >= end + 4 + length {
                    break;
                }
            }
        }
        let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n";
        conn.write_all(format!("{head}{:x}\r\n{first}\r\n", first.len()).as_bytes())
            .await
            .unwrap();
        conn.flush().await.unwrap();
        match ending {
            Ending::Clean => conn.write_all(b"0\r\n\r\n").await.unwrap(),
            Ending::Later => {
                let tail = format!("data: {REAL_END}\n\n");
                conn.write_all(format!("{:x}\r\n{tail}\r\n0\r\n\r\n", tail.len()).as_bytes())
                    .await
                    .unwrap();
            }
            Ending::Broken => {
                // Lets the first chunk reach the reader before the close cuts the body.
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
        conn.flush().await.unwrap();
    });
    url
}

/// #25 (read-on), how the upstream ended: a buffered answer from an SSE upstream ends on a
/// processing error only at a clean end of the stream, where the pinned SSE reader reports
/// the error it recorded (`sse/responses.rs:616-621`). A read failure before any terminal
/// event fails the answer, because that reader returns a read error at once (lines
/// 602-614), whatever it recorded: the content-filter `response.incomplete` with usage
/// 9/9/18 in one whole chunk, then a body cut off without its terminating chunk, is no
/// successful answer and no successful record. The failure keeps the buffered route's
/// request-scoped 408 for a stream without a terminal event, and its record carries no
/// tokens (the server's failure records never do). Controls: the same event with a clean
/// end is the answer, with its usage; a later completion decides; a cut-off body after
/// `response.created` alone fails the same way; a cut-off after a real completion keeps
/// that completion. The streaming route already fails on the read failure.
#[tokio::test]
async fn buffered_answers_end_on_a_processing_error_only_at_a_clean_end() {
    const RECORDED: &str = r#"{"type":"response.incomplete","response":{"id":"resp_real","status":"incomplete","incomplete_details":{"reason":"content_filter"},"output":[],"usage":{"input_tokens":9,"output_tokens":9,"total_tokens":18}}}"#;
    const CREATED: &str = r#"{"type":"response.created","response":{"id":"resp_real"}}"#;
    let full = routed("http-request-full");
    let buffered = full.replacen(r#""stream":true"#, r#""stream":false"#, 1);
    /// One body: its first chunk and ending, the client's mode, and what it gets.
    struct Case {
        name: &'static str,
        first: &'static str,
        ending: Ending,
        buffered: bool,
        status: u16,
        answer: Option<&'static str>,
        failed: bool,
        total: i64,
    }
    let case = |name, first, ending, buffered, status, answer, failed, total| Case {
        name,
        first,
        ending,
        buffered,
        status,
        answer,
        failed,
        total,
    };
    let recorded_object = last_response_object(RECORDED);
    let real_object = last_response_object(REAL_END);
    let cases = [
        case("recorded, broken", RECORDED, Ending::Broken, true, 408, None, true, 0),
        case(
            "recorded, clean",
            RECORDED,
            Ending::Clean,
            true,
            200,
            Some(recorded_object),
            false,
            18,
        ),
        case(
            "recorded, later completion",
            RECORDED,
            Ending::Later,
            true,
            200,
            Some(real_object),
            false,
            5,
        ),
        case("created, broken", CREATED, Ending::Broken, true, 408, None, true, 0),
        case(
            "completion, broken",
            REAL_END,
            Ending::Broken,
            true,
            200,
            Some(real_object),
            false,
            5,
        ),
        case(
            "streaming, recorded, broken",
            RECORDED,
            Ending::Broken,
            false,
            200,
            None,
            true,
            0,
        ),
    ];
    for Case {
        name: case,
        first,
        ending,
        buffered: is_buffered,
        status,
        answer,
        failed,
        total,
    } in cases
    {
        let upstream = chunked_upstream(format!("data: {first}\n\n"), ending).await;
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
            vec![credential(false, &upstream)],
            executors,
        ));
        rt.usage_queue().configure(
            true,
            &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
        );
        let url = serve(cpa_server::router(rt.clone())).await;
        let body = if is_buffered { buffered.clone() } else { full.clone() };
        let response = wreq::Client::new()
            .post(format!("{url}/v1/responses"))
            .header("authorization", "Bearer client-key")
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        let got_status = response.status().as_u16();
        // A streaming body cut off by the upstream ends with an error, after the event.
        let got = response.text().await.unwrap_or_default();
        assert_eq!(got_status, status, "{case}: {got}");
        match answer {
            Some(answer) => assert_eq!(got, answer, "{case}"),
            None if is_buffered => assert!(got.contains("stream closed before response.completed"), "{case}: {got}"),
            None => {}
        }
        assert!(!got.contains("at-FAKE"), "{case}: {got}");
        let queue = rt.usage_queue();
        let mut records: Vec<serde_json::Value> = Vec::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while records.is_empty() {
                records.extend(queue.pop_oldest(10).iter().map(|r| serde_json::from_slice(r).unwrap()));
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        records.extend(queue.pop_oldest(10).iter().map(|r| serde_json::from_slice(r).unwrap()));
        assert_eq!(records.len(), 1, "{case}: {records:?}");
        assert_eq!(records[0]["failed"], failed, "{case}: {records:?}");
        assert_eq!(records[0]["tokens"]["total_tokens"], total, "{case}: {records:?}");
        let credential = rt.store().snapshot()[0].id.clone();
        assert!(rt.store().cooldowns(&credential).is_empty(), "{case}");
    }
}

/// #25 and #6 on the native HTTP route, an event written as a JSON array: the pinned SSE
/// reader decodes `["response.failed",null,null,{…}]` by position
/// (`codex-api/src/sse/responses.rs:161-179` at 44ccf394573c), and failure classification
/// reads the members it decodes to (`codex_event::classification_view`). Each positional
/// event takes its object form's answer, status, cooldown and record, for a streaming and
/// a buffered client: an account failure is that status with the provider's error object
/// (the route's established answer) and cools the credential; another failure keeps its
/// classification; a Codex client gets the #5 `response.failed` built from a status-less
/// context-window `error`'s members; a positional completion ends the response. The real
/// completion after a failure is never the answer. One attempt each.
#[tokio::test]
async fn positional_events_classify_as_their_object_form_on_http() {
    let full = routed("http-request-full");
    let buffered = full.replacen(r#""stream":true"#, r#""stream":false"#, 1);
    let headers = [("user-agent", CODEX_UA), ("originator", "codex_cli_rs")];
    let limited = r#"{"type":"rate_limit_error","code":"rate_limit_exceeded","message":"limited","token":"at-FAKE","private_account":"acct-PRIVATE"}"#;
    let context = r#"{"code":"context_length_exceeded","message":"too long","param":"input"}"#;
    let positional = |members: &[(usize, &str)]| {
        let mut all = vec!["null"; 14];
        for (i, raw) in members {
            all[*i] = raw;
        }
        format!("[{}]", all.join(","))
    };
    let limited_response = format!(r#"{{"id":"r1","error":{limited}}}"#);
    // (positional, object form, the streaming client's status)
    let cases = [
        (
            positional(&[(0, r#""response.failed""#), (3, &limited_response)]),
            format!(r#"{{"type":"response.failed","response":{limited_response}}}"#),
            429,
        ),
        (
            positional(&[(0, r#""error""#), (5, limited)]),
            format!(r#"{{"type":"error","error":{limited}}}"#),
            429,
        ),
        (
            positional(&[
                (0, r#""response.failed""#),
                (3, r#"{"id":"r1","error":{"type":"invalid_request_error","code":"other","message":"bad input"}}"#),
            ]),
            r#"{"type":"response.failed","response":{"id":"r1","error":{"type":"invalid_request_error","code":"other","message":"bad input"}}}"#.to_owned(),
            200,
        ),
        (
            positional(&[(0, r#""error""#), (4, r#""resp_ctx""#), (5, context)]),
            format!(r#"{{"type":"error","response_id":"resp_ctx","error":{context}}}"#),
            200,
        ),
        (
            positional(&[(0, r#""response.completed""#), (3, r#"{"id":"r1","end_turn":true}"#)]),
            r#"{"type":"response.completed","response":{"id":"r1","end_turn":true}}"#.to_owned(),
            200,
        ),
    ];
    let answer = |event: &str, body: &str| {
        let event = event.to_owned();
        let body = body.to_owned();
        async move {
            let proxy = native(
                false,
                Script {
                    http: vec![Reply::sse(format!("data: {event}\n\ndata: {REAL_END}\n\n"))],
                    ..Script::default()
                },
                "",
            )
            .await;
            proxy.rt.usage_queue().configure(
                true,
                &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
            );
            let (status, _, answer) = post(&proxy.url, "/v1/responses", &headers, body).await;
            let records = usage_records(&proxy, 1).await;
            let credential = proxy.rt.store().snapshot()[0].id.clone();
            let cooled = !proxy.rt.store().cooldowns(&credential).is_empty();
            assert_eq!(proxy.upstream.http().len(), 1, "{event}");
            assert_eq!(records.len(), 1, "{event}: {records:?}");
            let record = (records[0]["failed"].clone(), records[0]["fail"]["status_code"].clone());
            (status, answer.replace(&event, "EVENT"), cooled, record)
        }
    };
    for (array, object, streaming_status) in cases {
        assert!(cpa_exec::codex_event::decode(&array).is_some(), "{array}");
        for (stream, body) in [(true, &full), (false, &buffered)] {
            let case = format!("stream {stream}: {array}");
            let got = answer(&array, body).await;
            let want = answer(&object, body).await;
            assert_eq!(got, want, "{case}");
            assert!(!got.1.contains("resp_real"), "{case}: {got:?}");
            if stream {
                assert_eq!(got.0, streaming_status, "{case}: {got:?}");
            }
            assert_eq!(got.2, streaming_status == 429, "{case}: {got:?}");
        }
    }
}

// #25 acceptance criterion 7 on the native HTTP route, the outbound credential guard
// (`cpa_exec::codex_secret`): the selected credential's token is the fixture's `at-FAKE`,
// sent upstream as `Bearer at-FAKE`; the client's key is `client-key`. Every case also
// runs with `at-OTHER`, text no request carried, which keeps every byte. Generated
// frames, no recorded provider traffic.

const SELECTED: &str = "at-FAKE";
const LOOKALIKE: &str = "at-OTHER";

fn with_token(frame: &str, token: &str) -> String {
    let escaped: String = token.chars().map(|c| format!("\\u{:04x}", u32::from(c))).collect();
    frame.replace("{T}", token).replace("{E}", &escaped)
}

fn carries_token(text: &str, token: &str) -> bool {
    let escaped: String = token.chars().map(|c| format!("\\u{:04x}", u32::from(c))).collect();
    text.contains(token) || text.contains(&escaped)
}

/// Usage records on, for the assertions on them.
fn counted(proxy: &Native) {
    proxy.rt.usage_queue().configure(
        true,
        &Config::parse("observability: {usage: {usage-statistics-enabled: true}}\n").unwrap(),
    );
}

/// The attempt carried the selected credential, never the client's key.
fn selected_upstream(proxy: &Native) {
    for exchange in proxy.upstream.http() {
        assert_eq!(exchange.headers["authorization"], format!("Bearer {SELECTED}"));
        assert!(!format!("{:?}", exchange.headers).contains("client-key"));
    }
}

/// Streaming: an event the pinned reader skips or reads as content that carries the
/// token is dropped and the real completion follows, one successful record with its
/// usage; a failure after content that carries it ends the stream without it (the record
/// and its status as the control's); one before content is a status-only error with the
/// control's status.
#[tokio::test]
async fn the_selected_credential_never_reaches_a_streaming_http_client() {
    const CREATED: &str = r#"{"type":"response.created","response":{"id":"resp_real","output":[]}}"#;
    const DELTA: &str = r#"{"type":"response.output_text.delta","response_id":"resp_real","delta":"hello"}"#;
    let sse = |events: &[&str]| events.iter().map(|e| format!("data: {e}\n\n")).collect::<String>();
    for (name, frame) in [
        (
            "malformed JSON",
            r#"{"type":"response.failed","response":{"error":{"message":"{T}"}}"#,
        ),
        ("unknown event", r#"{"type":"response.future","note":["{T}",1.50]}"#),
        (
            "escaped delta",
            r#"{"type":"response.output_text.delta","response_id":"resp_real","delta":"{E}"}"#,
        ),
        (
            "repeated member",
            r#"{"type":"error","error":{"message":"{T}"},"error":{"code":"x"}}"#,
        ),
    ] {
        for token in [SELECTED, LOOKALIKE] {
            let event = with_token(frame, token);
            let upstream = sse(&[CREATED, &event, REAL_END]);
            let proxy = native(
                false,
                Script {
                    http: vec![Reply::sse(upstream.clone())],
                    ..Script::default()
                },
                "",
            )
            .await;
            counted(&proxy);
            let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], routed("http-request-full")).await;
            assert_eq!(status, 200, "{name} {token}");
            if token == SELECTED {
                assert_eq!(answer, sse(&[CREATED, REAL_END]), "{name}: dropped, completion follows");
            } else {
                assert_eq!(answer, upstream, "{name}: the control as sent");
            }
            let records = usage_records(&proxy, 1).await;
            assert_eq!(records.len(), 1, "{name} {token}");
            assert_eq!(records[0]["failed"], false, "{name} {token}");
            assert_eq!(records[0]["tokens"]["total_tokens"], 5, "{name} {token}");
            selected_upstream(&proxy);
        }
    }
    let failure = r#"{"type":"response.failed","response":{"id":"resp_real","error":{"code":"x","message":"retry {T}"},"error":{"code":"x","message":"safe"}}}"#;
    for leading in [&[CREATED, DELTA][..], &[][..]] {
        let mut seen = Vec::new();
        for token in [SELECTED, LOOKALIKE] {
            let event = with_token(failure, token);
            let mut events = leading.to_vec();
            events.push(&event);
            let upstream = sse(&events);
            let proxy = native(
                false,
                Script {
                    http: vec![Reply::sse(upstream.clone())],
                    ..Script::default()
                },
                "",
            )
            .await;
            counted(&proxy);
            let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], routed("http-request-full")).await;
            assert!(!carries_token(&answer, SELECTED), "{answer}");
            if leading.is_empty() {
                if token == LOOKALIKE {
                    assert!(answer.contains(LOOKALIKE), "the control's own error: {answer}");
                }
            } else if token == SELECTED {
                assert_eq!(answer, sse(leading), "the stream ends without the failure");
            } else {
                assert_eq!(answer, upstream, "the control as sent");
            }
            let records = usage_records(&proxy, 1).await;
            seen.push((
                status,
                records[0]["failed"].clone(),
                records[0]["fail"]["status_code"].clone(),
            ));
            selected_upstream(&proxy);
        }
        assert_eq!(seen[0], seen[1], "privacy changes delivery only: {seen:?}");
        assert_eq!(seen[0].1, true);
    }
}

/// An upstream HTTP error, a buffered answer and a compaction answer that carry the
/// token: a status-only error instead of the upstream's body (the error keeps its
/// classified status; a buffered or compaction answer, which has no stream to end, is a
/// request-scoped 502 that neither fails over nor cools). A usage limit that carries the
/// token still cools the credential, and the cooldown's later "last error" carries only
/// its status. The controls keep their bytes.
#[tokio::test]
async fn the_selected_credential_never_reaches_an_http_answer() {
    let full = routed("http-request-full");
    let buffered = full.replacen(r#""stream":true"#, r#""stream":false"#, 1);
    let compaction = routed("http-request-compaction").replacen(r#""stream":true"#, r#""stream":false"#, 1);
    let error = r#"{"error":{"type":"invalid_request_error","code":"x","message":"bad {T}"}}"#;
    let completed = r#"{"type":"response.completed","response":{"id":"resp_real","status":"completed","output":[],"note":"{E}","usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    let compacted = r#"{"id":"cmp_1","object":"response.compaction","output":[{"type":"compaction","encrypted_content":"ENC {T}"}]}"#;
    for token in [SELECTED, LOOKALIKE] {
        let selected = token == SELECTED;
        // An upstream 400.
        let body = with_token(error, token);
        let proxy = native(
            false,
            Script {
                http: vec![Reply::json(400, body.clone())],
                ..Script::default()
            },
            "",
        )
        .await;
        let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], full.clone()).await;
        assert_eq!(status, 400, "{token}");
        if selected {
            assert!(!carries_token(&answer, SELECTED), "{answer}");
            assert!(!answer.contains("bad"), "status-only: {answer}");
        } else {
            assert_eq!(answer, body);
        }
        selected_upstream(&proxy);
        // A buffered answer.
        let event = with_token(completed, token);
        let proxy = native(
            false,
            Script {
                http: vec![Reply::sse(format!("data: {event}\n\n"))],
                ..Script::default()
            },
            "",
        )
        .await;
        let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], buffered.clone()).await;
        if selected {
            assert_eq!(status, 502, "{answer}");
            assert!(!carries_token(&answer, SELECTED), "{answer}");
        } else {
            assert_eq!((status, answer.as_str()), (200, last_response_object(&event)));
        }
        assert_eq!(
            proxy.upstream.http().len(),
            1,
            "no failover after the upstream served it"
        );
        let cooled: usize = proxy
            .rt
            .store()
            .snapshot()
            .iter()
            .map(|c| proxy.rt.store().cooldowns(&c.id).len())
            .sum();
        assert_eq!(cooled, 0, "{token}");
        selected_upstream(&proxy);
        // A compaction answer.
        let answer_body = with_token(compacted, token);
        let proxy = native(
            false,
            Script {
                http: vec![Reply::json(200, answer_body.clone())],
                ..Script::default()
            },
            "",
        )
        .await;
        let (status, _, answer) = post(&proxy.url, "/v1/responses/compact", &[], compaction.clone()).await;
        if selected {
            assert_eq!(status, 502, "{answer}");
            assert!(!carries_token(&answer, SELECTED), "{answer}");
        } else {
            assert_eq!((status, answer.as_str()), (200, answer_body.as_str()));
        }
        selected_upstream(&proxy);
        // A usage limit cools the credential; the cooldown's answer names its last error.
        let limit = with_token(
            r#"{"error":{"type":"usage_limit_reached","message":"limit for {T}","resets_in_seconds":600}}"#,
            token,
        );
        let proxy = native(
            false,
            Script {
                http: vec![Reply::json(429, limit.clone())],
                ..Script::default()
            },
            "",
        )
        .await;
        let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], full.clone()).await;
        assert_eq!(status, 429, "{token}: {answer}");
        if selected {
            assert!(!carries_token(&answer, SELECTED), "{answer}");
        } else {
            assert_eq!(answer, limit);
        }
        let (status, _, later) = post(&proxy.url, "/v1/responses", &[], full.clone()).await;
        assert_eq!(status, 429, "{later}");
        assert_eq!(proxy.upstream.http().len(), 1, "cooled");
        if selected {
            assert!(!carries_token(&later, SELECTED), "{later}");
        } else {
            assert!(later.contains(LOOKALIKE), "the control's last error is shown: {later}");
        }
        selected_upstream(&proxy);
    }
}

/// A token a second Codex account carries.
const SECOND: &str = "at-SECOND";

/// [`native`] (the ChatGPT profile) with one credential per token of `tokens`, the first
/// preferred, `config` appended to its configuration, and with `logged`, through the
/// shared request-log middleware (`request-log: true`), logging to the returned directory
/// in its private `auth-dir`.
async fn native_accounts(
    script: Script,
    tokens: &[&str],
    config: &str,
    logged: bool,
) -> (
    Native,
    Option<(std::path::PathBuf, Arc<cpa_server::management::Management>)>,
) {
    let upstream = CaptureUpstream::start(script).await;
    let auth_dir = AuthDir::new();
    let cfg = Config::parse(&format!(
        "auth-dir: {}\naccess:\n  api-keys: [client-key]\nrequest-log: {logged}\n{config}",
        auth_dir.0.display()
    ))
    .unwrap();
    let credentials = tokens
        .iter()
        .enumerate()
        .map(|(i, token)| {
            let mut c = credential(false, &upstream.base_url);
            if i > 0 {
                c.id = format!("codex-{i}.json");
                c.metadata.insert("access_token".into(), serde_json::json!(token));
            }
            c.attributes.insert("priority".into(), (9 - i).to_string());
            c
        })
        .collect();
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        meta: Default::default(),
        xai: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(cfg, credentials, executors));
    let logs = logged.then(|| {
        let dir = auth_dir.0.join("logs");
        std::fs::create_dir_all(&dir).unwrap();
        let management = cpa_server::management::Management::with_options(
            rt.clone(),
            auth_dir.0.join("config.yaml"),
            cpa_server::management::Options {
                log_dir: Some(dir.clone()),
                management_password: Some(String::new()),
                auth_dir: Some(auth_dir.0.clone()),
                ..Default::default()
            },
        );
        (dir, management)
    });
    let app = match &logs {
        Some((_, management)) => cpa_server::request_logging::router(management, cpa_server::router(rt.clone())),
        None => cpa_server::router(rt.clone()),
    };
    let proxy = Native {
        url: serve(app).await,
        upstream,
        rt,
        _auth_dir: auth_dir,
    };
    counted(&proxy);
    (proxy, logs)
}

/// The one request log in `dir`, once complete: no spool file is left and it reads the
/// same three times in a row.
async fn finished_log(dir: &Path) -> String {
    let mut last: Option<String> = None;
    let mut same = 0;
    for _ in 0..250 {
        let entries: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
        let spooling = entries.iter().any(|p| p.extension().is_some_and(|x| x == "tmp"));
        let logs: Vec<_> = entries
            .iter()
            .filter(|p| p.extension().is_some_and(|x| x == "log"))
            .collect();
        if let ([log], false) = (&logs[..], spooling) {
            let content = std::fs::read_to_string(log).unwrap();
            if last.as_ref() == Some(&content) {
                same += 1;
                if same == 3 {
                    return content;
                }
            } else {
                (last, same) = (Some(content), 0);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("no complete request log in {dir:?}: {last:?}")
}

fn cooldowns(proxy: &Native) -> usize {
    let store = proxy.rt.store();
    store.snapshot().iter().map(|c| store.cooldowns(&c.id).len()).sum()
}

/// #25 on a streaming HTTP client: an event the credential guard leaves out was served,
/// so the stream commits on it. A stream whose every event is left out, its completion
/// included, keeps the outcome processing gave it: one execution on the selected account,
/// one successful record with the reported usage, no cooldown, and no second execution
/// on the other account available. The client reads a stream that ended without the
/// completion. This holds with and without `stream-bootstrap-buffering`. The `at-OTHER`
/// controls differ only in what the client reads.
#[tokio::test]
async fn a_withheld_stream_keeps_the_served_outcome_without_a_replay() {
    const CREATED: &str = r#"{"type":"response.created","response":{"id":"resp_real","note":"{T}"}}"#;
    const SAFE_CREATED: &str = r#"{"type":"response.created","response":{"id":"resp_real"}}"#;
    const END: &str = r#"{"type":"response.completed","response":{"id":"resp_real","model":"gpt-5.5 {T}","service_tier":"echo {T}","status":"completed","output":[],"note":"{T}","usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    const SECOND_END: &str = r#"{"type":"response.completed","response":{"id":"resp_second","status":"completed","output":[],"usage":{"input_tokens":9,"output_tokens":9,"total_tokens":18}}}"#;
    let sse = |events: &[String]| events.iter().map(|e| format!("data: {e}\n\n")).collect::<String>();
    let cases = [
        ("sole completion", &[END][..]),
        ("withheld prefix and completion", &[CREATED, END][..]),
        ("safe prefix, withheld completion", &[SAFE_CREATED, END][..]),
    ];
    let buffering = ["", "codex:\n  stream-bootstrap-buffering: true\n"];
    for (config, (name, events)) in buffering.iter().flat_map(|c| cases.map(|case| (c, case))) {
        let name = format!("{name}{}", if config.is_empty() { "" } else { ", buffering" });
        let mut seen = Vec::new();
        for token in [SELECTED, LOOKALIKE] {
            let events: Vec<String> = events.iter().map(|e| with_token(e, token)).collect();
            let (proxy, _) = native_accounts(
                Script {
                    http: vec![Reply::sse(sse(&events)), Reply::sse(sse(&[SECOND_END.to_owned()]))],
                    ..Script::default()
                },
                &[SELECTED, SECOND],
                config,
                false,
            )
            .await;
            let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], routed("http-request-full")).await;
            let records = usage_records(&proxy, 1).await;
            let exchanges = proxy.upstream.http();
            assert_eq!(exchanges.len(), 1, "{name} {token}: one execution, no replay");
            assert_eq!(exchanges[0].headers["authorization"], format!("Bearer {SELECTED}"));
            assert_eq!(status, 200, "{name} {token}");
            let delivered: Vec<String> = events.iter().filter(|e| !e.contains(SELECTED)).cloned().collect();
            assert_eq!(answer, sse(&delivered), "{name} {token}");
            assert_eq!(records.len(), 1, "{name} {token}: {records:?}");
            assert_eq!(records[0]["failed"], false, "{name} {token}: {records:?}");
            let usage = ["input_tokens", "output_tokens", "total_tokens"].map(|k| records[0]["tokens"][k].as_i64());
            assert_eq!(usage, [Some(4), Some(1), Some(5)], "{name} {token}");
            assert_eq!(cooldowns(&proxy), 0, "{name} {token}");
            assert!(
                !carries_token(&records[0].to_string(), SELECTED),
                "{name}: {}",
                records[0]
            );
            let scrubbed = |field: &str| records[0][field].as_str().map(|m| m.replace(token, "[REDACTED]"));
            seen.push((
                records[0]["auth_index"].clone(),
                records[0]["tokens"].clone(),
                scrubbed("response_model"),
                scrubbed("response_service_tier"),
            ));
        }
        assert_eq!(seen[0], seen[1], "{name}: privacy changes delivery only");
        // The record keeps the model and the tier the withheld completion reported, scrubbed.
        assert_eq!(seen[0].2.as_deref(), Some("gpt-5.5 [REDACTED]"), "{name}");
        assert_eq!(seen[0].3.as_deref(), Some("echo [REDACTED]"), "{name}");
    }
}

/// #25 and AGENTS.md on the native HTTP route: an upstream answer that quotes the selected
/// credential (sent upstream as `Bearer at-FAKE`; the client's key is `client-key`) keeps
/// it out of the request log and the queued failure record: an error body, a streamed
/// event, and a buffered answer the route withholds. The entries stay, with `[REDACTED]`
/// where the value was, and the record keeps the control's status and tokens; the
/// withheld buffered answer is the route's request-scoped 502, whose fixed text carries
/// nothing. The `at-OTHER` controls keep every byte.
#[tokio::test]
async fn http_logs_and_usage_records_never_keep_the_selected_credential() {
    const CREATED: &str = r#"{"type":"response.created","response":{"id":"resp_real","output":[]}}"#;
    let full = routed("http-request-full");
    let buffered = full.replacen(r#""stream":true"#, r#""stream":false"#, 1);
    let error = r#"{"error":{"type":"invalid_request_error","code":"x","message":"bad {T}"}}"#;
    let notice = r#"{"type":"response.future","note":"{T}"}"#;
    let answer = r#"{"type":"response.completed","response":{"id":"resp_real","status":"completed","output":[],"note":"{T}","usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}}}"#;
    for (name, body, reply) in [
        ("error body", &full, Reply::json(400, error)),
        (
            "streamed event",
            &full,
            Reply::sse(format!("data: {CREATED}\n\ndata: {notice}\n\ndata: {REAL_END}\n\n")),
        ),
        (
            "withheld buffered answer",
            &buffered,
            Reply::sse(format!("data: {answer}\n\n")),
        ),
    ] {
        let mut seen = Vec::new();
        for token in [SELECTED, LOOKALIKE] {
            let mut reply = reply.clone();
            reply.body = Bytes::from(with_token(std::str::from_utf8(&reply.body).unwrap(), token));
            // An error's upstream headers are kept in its record too.
            if reply.status != 200 {
                reply.headers.push(("x-trace".into(), format!("trace {token}")));
            }
            let (proxy, logs) = native_accounts(
                Script {
                    http: vec![reply],
                    ..Script::default()
                },
                &[SELECTED],
                "",
                true,
            )
            .await;
            let (status, _, client) = post(&proxy.url, "/v1/responses", &[], body.clone()).await;
            let records = usage_records(&proxy, 1).await;
            let log = finished_log(&logs.as_ref().unwrap().0).await;
            selected_upstream(&proxy);
            assert!(!carries_token(&client, SELECTED), "{name}: {client}");
            let fail = records[0]["fail"]["body"].as_str().unwrap_or_default().to_owned();
            let headers = records[0]["response_headers"].to_string();
            if token == SELECTED {
                assert!(!carries_token(&log, SELECTED), "{name}: {log}");
                assert!(!log.contains("client-key"), "{name}: {log}");
                assert!(log.contains("[REDACTED]"), "{name}: {log}");
                assert!(!carries_token(&fail, SELECTED), "{name}: {fail}");
                assert!(!carries_token(&headers, SELECTED), "{name}: {headers}");
                if name == "error body" {
                    assert!(headers.contains("trace [REDACTED]"), "{name}: {headers}");
                }
            } else {
                if name == "error body" {
                    assert!(headers.contains("trace at-OTHER"), "{name}: {headers}");
                }
                assert!(log.contains(LOOKALIKE), "{name}: the control keeps its bytes");
                assert!(!log.contains("[REDACTED"), "{name}: nothing else is scrubbed");
            }
            seen.push((
                status,
                records[0]["failed"].clone(),
                records[0]["fail"]["status_code"].clone(),
                records[0]["tokens"].clone(),
                fail.replace(token, "[REDACTED]"),
            ));
        }
        if name == "withheld buffered answer" {
            assert_eq!(seen[0].0, 502, "{name}: {seen:?}");
            assert_eq!(seen[1].0, 200, "{name}: {seen:?}");
        } else {
            assert_eq!(seen[0], seen[1], "{name}: the record is the control's, scrubbed");
        }
    }
}

/// #25 (`Delivery::Confidential`): a failure that carried the selected credential hands the
/// client no upstream headers, even safe ones and even when `passthrough-headers` is on. A
/// streaming request whose upstream answers HTTP 200 with a safe `x-upstream-trace` header
/// and then an initial usage-limit `response.failed` quoting the token fails as a stream
/// bootstrap error. Its status-only 429 carries neither the token nor that header. The
/// `at-OTHER` control is the same classified failure, rendered from the upstream's body as
/// before.
#[tokio::test]
async fn a_confidential_bootstrap_failure_hands_the_client_no_upstream_headers() {
    let failed = r#"{"type":"response.failed","response":{"id":"real","error":{"type":"usage_limit_reached","message":"limited {T}","resets_in_seconds":600}}}"#;
    for (case, body) in [
        ("selected", format!("data: {}\n\n", with_token(failed, SELECTED))),
        ("unsent", format!("data: {}\n\n", with_token(failed, LOOKALIKE))),
    ] {
        let mut reply = Reply::sse(body);
        reply
            .headers
            .push(("x-upstream-trace".into(), "safe-never-sent-marker".into()));
        let (proxy, _) = native_accounts(
            Script {
                http: vec![reply],
                ..Script::default()
            },
            &[SELECTED],
            "requests:\n  passthrough-headers: true\n",
            false,
        )
        .await;
        let (status, headers, body) = post(&proxy.url, "/v1/responses", &[], routed("http-request-full")).await;
        let records = usage_records(&proxy, 1).await;
        selected_upstream(&proxy);
        assert!(!carries_token(&body, SELECTED), "{case}: {body}");
        assert!(!carries_token(&format!("{headers:?}"), SELECTED), "{case}: {headers:?}");
        assert_eq!(records[0]["failed"], true, "{case}");
        assert_eq!(status, 429, "{case}: {body}");
        if case == "selected" {
            assert!(
                headers.get("x-upstream-trace").is_none(),
                "a Confidential failure kept a passthrough header: {headers:?}"
            );
        } else {
            assert!(
                body.contains(LOOKALIKE),
                "the classified control keeps its body: {body}"
            );
        }
    }
}

/// FP1 (#115) qualification at the research pin: a native body whose
/// `function_call_output` carries multimodal content items, file-backed audio included
/// (`http-request-tool-output-media`, openai/codex main `5c3ff1dabf64`), reaches the
/// upstream byte for byte. That holds for HTTP streaming, and for a buffered request on
/// the `chatgpt` profile (where only `stream` changes) and on an API key (no change).
/// This is source-refresh evidence. The contract pin is unchanged, and endpoint acceptance
/// of the new shape stays unknown.
#[tokio::test]
async fn tool_output_media_reaches_the_upstream_byte_for_byte() {
    let full = routed("http-request-tool-output-media");
    let buffered = full.replacen(r#""stream":true"#, r#""stream":false"#, 1);
    assert_ne!(buffered, full);
    let json = r#"{"id":"resp_1","object":"response","status":"completed","output":[],"usage":null}"#;
    for api_key in [false, true] {
        let second = if api_key {
            Reply::json(200, json)
        } else {
            Reply::sse(fixture("sse-completed").bytes())
        };
        let proxy = native(
            api_key,
            Script {
                http: vec![Reply::sse(fixture("sse-completed").bytes()), second],
                ..Script::default()
            },
            "",
        )
        .await;
        let (status, _, _) = post(&proxy.url, "/v1/responses", &[("user-agent", CODEX_UA)], full.clone()).await;
        assert_eq!(status, 200, "api_key={api_key}: streaming");
        let (status, _, _) = post(&proxy.url, "/v1/responses", &[], buffered.clone()).await;
        assert_eq!(status, 200, "api_key={api_key}: buffered");
        let sent: Vec<String> = proxy
            .upstream
            .http()
            .into_iter()
            .map(|e| String::from_utf8(e.body.to_vec()).unwrap())
            .collect();
        let want_buffered = if api_key { &buffered } else { &full };
        assert_eq!(sent, [full.clone(), want_buffered.clone()], "api_key={api_key}");
    }
}

/// N1 (#111) W3, native HTTP streaming: under `stream-bootstrap-buffering`, an upstream
/// `response.metadata` before an explicit `server_is_overloaded` rejection is held, so the
/// request fails over to the next account, and the failed attempt's events, its metadata
/// included, never reach the client; the serving attempt's events, its own metadata
/// included, reach it byte for byte and in order. The moderation payload follows Go's
/// upstream capture (Go issue #6856), not verified provider behaviour.
#[tokio::test]
async fn bootstrap_buffering_holds_response_metadata_on_http() {
    const CREATED: &str = r#"{"type":"response.created","response":{"id":"r1"}}"#;
    const MODERATION: &str = r#"{"type":"response.metadata","response_id":"r1","sequence_number":2,"metadata":{"openai_chatgpt_moderation_metadata":{"prompt":{"omnimod":{"protection_type":"omnimod"}}}}}"#;
    const OVERLOADED: &str =
        r#"{"type":"response.failed","response":{"id":"r1","error":{"code":"server_is_overloaded","message":null}}}"#;
    const SERVED: &str = r#"{"type":"response.metadata","response_id":"r2","metadata":{"openai_chatgpt_moderation_metadata":{"prompt":{}}},"future":1.50}"#;
    const DONE: &str = r#"{"type":"response.completed","response":{"id":"r2","output":[],"usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}}}"#;
    let sse = |frames: &[&str]| frames.iter().map(|f| format!("data: {f}\n\n")).collect::<String>();
    let proxy = native(
        false,
        Script {
            http: vec![
                Reply::sse(sse(&[CREATED, MODERATION, OVERLOADED])),
                Reply::sse(sse(&[CREATED, SERVED, DONE])),
            ],
            ..Script::default()
        },
        "codex:\n  stream-bootstrap-buffering: true\n",
    )
    .await;
    let mut second = credential(false, &proxy.upstream.base_url);
    second.id = "codex-second.json".into();
    second.metadata.insert("access_token".into(), "at-SECOND".into());
    proxy.rt.store().upsert(second);
    let (status, _, answer) = post(&proxy.url, "/v1/responses", &[], routed("http-request-full")).await;
    assert_eq!(status, 200);
    assert_eq!(proxy.upstream.http().len(), 2, "the overload failed over: {answer}");
    assert_eq!(
        answer,
        sse(&[CREATED, SERVED, DONE]),
        "the serving attempt's events, exactly"
    );
    assert!(!answer.contains("omnimod"), "no failed-attempt metadata");
}

/// FP2 (#116): the base64 `x-error-json` header first-party Codex reads a 403's code from
/// (`api_bridge.rs:342-354` at 5c3ff1dabf64), naming the `http-error-workspace-provider`
/// fixture's JSON.
fn x_error_json(json: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(json.trim())
}

/// FP2 (#116) on the native HTTP route, with a second account available. First-party
/// Codex at research main `5c3ff1dabf644169f4bd74b215719f381e4fdb3f` never retries these
/// (`protocol/src/error.rs` `retry_policy`, `RetryPolicy::Never`):
///
/// - HTTP: `inference_hook_denied` on a 403, `content_policy_violation` on a 400, and a
///   403 whose `/detail/code`, `/error/code` or `x-error-json` code is a workspace or
///   access-program denial (`api_bridge.rs:138-143`, `293-311`; the workspace bodies
///   are the `http-error-workspace-*` fixtures);
/// - in-stream: every event of the `sse-terminal-rejections` fixture, read with no outer
///   status (`sse/responses.rs:379-383`), as the first event and after output.
///
/// Each ends the request on the selected account: one upstream attempt carrying that
/// account's token (no failover, no refresh-and-retry), no cooldown on any account, and
/// exactly one failed usage record. The client gets the provider's status and bytes,
/// unknown fields and IDs included, as the native HTTP route delivers every upstream
/// error (docs/codex/CONTRACT.md "Native HTTP route"). The 400 `context_length_exceeded`
/// is the control Go already stopped on.
#[tokio::test]
async fn first_party_terminal_rejections_stop_on_the_selected_account() {
    const CREATED: &str = r#"{"type":"response.created","response":{"id":"resp_fp2","output":[]}}"#;
    const DELTA: &str = r#"{"type":"response.output_text.delta","response_id":"resp_fp2","delta":"partial"}"#;
    let sse = |events: &[&str]| events.iter().map(|e| format!("data: {e}\n\n")).collect::<String>();
    let provider = text_of("http-error-workspace-provider");
    let mut cases: Vec<(String, Reply, u16, String)> = Vec::new();
    for (status, body) in [
        (403, r#"{"error":{"code":"inference_hook_denied","message":"denied by policy","param":"input","future_field":{"n":1.50}}}"#.to_owned()),
        (400, r#"{"error":{"code":"content_policy_violation","message":"denied by policy","param":"input"}}"#.to_owned()),
        (403, r#"{"error":{"code":"access_program_not_enabled","message":"denied by policy"}}"#.to_owned()),
        (403, text_of("http-error-workspace-detail")),
        (403, provider.clone()),
        (403, r#"{"detail":{"code":"codex_workspace_access_denied"}}"#.to_owned()),
        (400, r#"{"error":{"code":"context_length_exceeded","message":"too long","param":"input"}}"#.to_owned()),
    ] {
        cases.push((format!("HTTP {status} {body}"), Reply::json(status, body.clone()), status, body));
    }
    cases.push((
        "HTTP 403 x-error-json".to_owned(),
        Reply {
            status: 403,
            headers: vec![("x-error-json".into(), x_error_json(&provider))],
            body: Bytes::new(),
        },
        403,
        String::new(),
    ));
    for frame in fixture("sse-terminal-rejections").frames() {
        let frame = String::from_utf8(frame.to_vec()).unwrap();
        for (stage, leading) in [("first", &[][..]), ("after output", &[CREATED, DELTA][..])] {
            let mut events = leading.to_vec();
            events.push(&frame);
            let upstream = sse(&events);
            cases.push((format!("{stage}: {frame}"), Reply::sse(upstream.clone()), 200, upstream));
        }
    }
    assert_eq!(cases.len(), 8 + 24);
    for (name, reply, status, want) in cases {
        let (proxy, _) = native_accounts(
            Script {
                http: vec![reply, Reply::sse(fixture("sse-completed").bytes())],
                ..Script::default()
            },
            &[SELECTED, SECOND],
            "",
            false,
        )
        .await;
        let (got, _, answer) = post(&proxy.url, "/v1/responses", &[], routed("http-request-full")).await;
        assert_eq!((got, answer.as_str()), (status, want.as_str()), "{name}");
        let exchanges = proxy.upstream.http();
        assert_eq!(
            exchanges.len(),
            1,
            "{name}: one attempt, no failover, no refresh-and-retry"
        );
        assert_eq!(
            exchanges[0].headers["authorization"],
            format!("Bearer {SELECTED}"),
            "{name}"
        );
        assert_eq!(cooldowns(&proxy), 0, "{name}: no account is cooled");
        let records = usage_records(&proxy, 1).await;
        assert_eq!(records.len(), 1, "{name}: one settlement {records:?}");
        assert_eq!(records[0]["failed"], true, "{name}");
    }
}

/// FP2 (#116) controls: failures first-party Codex retries, or that are the account's
/// own, still move to the second account before anything was delivered, once. A
/// `context_length_exceeded` with a status other than 400 is no context-window failure
/// (`error.rs:110-112`, `responses_websocket.rs:1416-1439` at 5c3ff1dabf64), with or
/// without Anthropic's `invalid_request_error` type; a 503 workspace body is an ordinary
/// 503 (`only_an_explicit_workspace_403_stops_retries`); a bare 403, a 403 naming another
/// code beside a workspace `x-error-json`, and a first-party runtime authentication denial
/// (`protocol/src/error.rs:703-732`) stay credential failures. The first account is
/// cooled, and the second serves the answer as sent.
#[tokio::test]
async fn retryable_and_account_failures_move_to_another_account() {
    let provider = text_of("http-error-workspace-provider");
    let header = vec![("x-error-json".to_owned(), x_error_json(&provider))];
    for (name, status, headers, body) in [
        (
            "context 503",
            503,
            vec![],
            r#"{"error":{"code":"context_length_exceeded","message":"Your input exceeds the context window of this model."}}"#,
        ),
        (
            "typed context 503",
            503,
            vec![],
            r#"{"error":{"type":"invalid_request_error","code":"context_length_exceeded","message":null}}"#,
        ),
        (
            "context 403",
            403,
            vec![],
            r#"{"error":{"code":"context_length_exceeded"}}"#,
        ),
        (
            "workspace 503",
            503,
            vec![],
            r#"{"detail":{"code":"codex_workspace_access_denied"}}"#,
        ),
        ("bare 403", 403, vec![], ""),
        (
            "other 403 code",
            403,
            header.clone(),
            r#"{"detail":{"code":"route_rate_limited"}}"#,
        ),
        (
            "runtime denial 403",
            403,
            vec![],
            r#"{"error":{"code":"runtime_instance_revoked"}}"#,
        ),
    ] {
        let reply = Reply {
            status,
            headers: [
                vec![("content-type".to_owned(), "application/json".to_owned())],
                headers,
            ]
            .concat(),
            body: Bytes::from(body),
        };
        let (proxy, _) = native_accounts(
            Script {
                http: vec![reply, Reply::sse(fixture("sse-completed").bytes())],
                ..Script::default()
            },
            &[SELECTED, SECOND],
            "",
            false,
        )
        .await;
        let (got, _, answer) = post(&proxy.url, "/v1/responses", &[], routed("http-request-full")).await;
        assert_eq!(
            (got, answer.as_str()),
            (200, text_of("sse-completed").as_str()),
            "{name}"
        );
        let tokens: Vec<_> = proxy
            .upstream
            .http()
            .iter()
            .map(|e| e.headers["authorization"].to_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            tokens,
            [format!("Bearer {SELECTED}"), format!("Bearer {SECOND}")],
            "{name}"
        );
        assert!(cooldowns(&proxy) > 0, "{name}: the failing account is cooled");
    }
}

/// FP2 (#116): the transient retry stays bounded. With `request-retry: 0` and both
/// accounts answering a context-labelled 503, each account is tried exactly once, both are
/// cooled, and the client gets the last provider status and body as sent.
#[tokio::test]
async fn a_context_labelled_503_is_retried_once_per_account() {
    let body = r#"{"error":{"code":"context_length_exceeded","message":"Your input exceeds the context window of this model.","param":"input"}}"#;
    let (proxy, _) = native_accounts(
        Script {
            http: vec![Reply::json(503, body), Reply::json(503, body)],
            ..Script::default()
        },
        &[SELECTED, SECOND],
        "routing:\n  retry:\n    request-retry: 0\n",
        false,
    )
    .await;
    let (got, _, answer) = post(&proxy.url, "/v1/responses", &[], routed("http-request-full")).await;
    assert_eq!((got, answer.as_str()), (503, body));
    assert_eq!(proxy.upstream.http().len(), 2, "one attempt per account");
    let store = proxy.rt.store();
    for credential in store.snapshot() {
        assert!(
            !store.cooldowns(&credential.id).is_empty(),
            "{} is cooled",
            credential.id
        );
    }
}

/// FP2 (#116) review findings 1, 3 and 5 on the native HTTP route, with a second account
/// available: a terminal rejection is read from its code and status before Go's capacity
/// reading of its message, the 403 workspace match runs whatever else the body names, and
/// a repeated key reads as its last occurrence (`serde_json::Value`, as first-party
/// `api_bridge.rs:138-140` reads it). Each stops on the selected account: one attempt, no
/// cooldown, one failed record, and the provider's status and bytes.
#[tokio::test]
async fn terminal_rejections_decide_before_messages_and_repeated_keys() {
    for body in [
        r#"{"error":{"code":"inference_hook_denied","message":"model is at capacity"}}"#,
        r#"{"error":{"code":"other","code":"inference_hook_denied","message":"denied"}}"#,
        r#"{"detail":{"code":"codex_workspace_access_denied"},"error":{"code":"context_length_exceeded"}}"#,
    ] {
        let (proxy, _) = native_accounts(
            Script {
                http: vec![Reply::json(403, body), Reply::sse(fixture("sse-completed").bytes())],
                ..Script::default()
            },
            &[SELECTED, SECOND],
            "",
            false,
        )
        .await;
        let (got, _, answer) = post(&proxy.url, "/v1/responses", &[], routed("http-request-full")).await;
        assert_eq!((got, answer.as_str()), (403, body), "{body}");
        let exchanges = proxy.upstream.http();
        assert_eq!(exchanges.len(), 1, "{body}: one attempt, no failover");
        assert_eq!(
            exchanges[0].headers["authorization"],
            format!("Bearer {SELECTED}"),
            "{body}"
        );
        assert_eq!(cooldowns(&proxy), 0, "{body}: no account is cooled");
        let records = usage_records(&proxy, 1).await;
        assert_eq!(records.len(), 1, "{body}: one settlement {records:?}");
        assert_eq!(records[0]["failed"], true, "{body}");
    }
}

/// FP2 (#116) review finding 2: a first-party code on a status first-party Codex retries
/// (`api_bridge.rs:223-312`) is no request fault, even beside Go's `invalid_request_error`
/// type, which Go reads as one on any status. The 503 moves once to the second account,
/// which serves the answer; the first account is cooled.
#[tokio::test]
async fn a_typed_first_party_code_on_a_503_moves_to_another_account() {
    let body = r#"{"error":{"code":"content_policy_violation","type":"invalid_request_error","message":"m"}}"#;
    let (proxy, _) = native_accounts(
        Script {
            http: vec![Reply::json(503, body), Reply::sse(fixture("sse-completed").bytes())],
            ..Script::default()
        },
        &[SELECTED, SECOND],
        "",
        false,
    )
    .await;
    let (got, _, answer) = post(&proxy.url, "/v1/responses", &[], routed("http-request-full")).await;
    assert_eq!((got, answer.as_str()), (200, text_of("sse-completed").as_str()));
    let tokens: Vec<_> = proxy
        .upstream
        .http()
        .iter()
        .map(|e| e.headers["authorization"].to_str().unwrap().to_owned())
        .collect();
    assert_eq!(tokens, [format!("Bearer {SELECTED}"), format!("Bearer {SECOND}")]);
    assert!(cooldowns(&proxy) > 0, "the failing account is cooled");
}
