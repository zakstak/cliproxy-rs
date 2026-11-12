//! CORS preflights over a real loopback socket through the binary's middleware stack
//! (crates/cliproxy/src/main.rs, without Home and observability). Fetch classifies
//! `Authorization` as a CORS non-wildcard request-header name: a preflight answer
//! must list it, and `Access-Control-Allow-Headers: *` does not
//! (<https://fetch.spec.whatwg.org/#cors-preflight-fetch>, CORS-preflight fetch;
//! MDN "Access-Control-Allow-Headers"). Go v8.0.67 058b803b made the same change.
//! The origin policy (`*`) and the absence of credentials stay as they were.
use std::net::SocketAddr;
use std::sync::Arc;

use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use cpa_server::management::{self, Management};

const ORIGIN: &str = "http://127.0.0.1:8765";
const CLIENT_KEY: &str = "fake-cors-client-key";
const FIXED: &str = "Authorization, Content-Type, X-Api-Key, X-Goog-Api-Key, Anthropic-Version, Anthropic-Beta";

struct Server {
    base: String,
    task: tokio::task::JoinHandle<()>,
    dir: std::path::PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn serve() -> Server {
    // A private config and auth-dir, never the default `~/.cli-proxy-api`.
    let dir = std::env::temp_dir().join(format!("cpa-cors-{}", uuid::Uuid::new_v4()));
    let auth = dir.join("auth");
    std::fs::create_dir_all(&auth).unwrap();
    let path = dir.join("config.yaml");
    std::fs::write(
        &path,
        format!(
            "auth-dir: {}\naccess:\n  api-keys: [{CLIENT_KEY}]\nmanagement:\n  disable-control-panel: true\n",
            auth.display()
        ),
    )
    .unwrap();
    let rt = Arc::new(cpa_server::testing::runtime(
        Config::load(&path).unwrap(),
        vec![],
        Executors {
            // Nothing listens here; no request in this file reaches an upstream.
            claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            meta: Default::default(),
            xai: Default::default(),
        },
    ));
    let state = Management::with_options(rt.clone(), path, management::Options::default());
    let app = cpa_server::app(rt.clone(), management::router(state.clone()));
    let app = cpa_server::safe_mode::router(&rt, app);
    let app = app.layer(axum::middleware::from_fn(management::cors));
    let app = cpa_server::request_logging::router(&state, app);
    let app = cpa_server::body_limit::layer(&rt, app);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
            .await
            .unwrap();
    });
    Server { base, task, dir }
}

fn header<'a>(response: &'a wreq::Response, name: &str) -> Option<&'a str> {
    response.headers().get(name).map(|v| v.to_str().unwrap())
}

/// The header-name step of Fetch's CORS-preflight fetch, for a request without
/// credentials (`*` origin forbids them): a CORS non-wildcard request-header name
/// (`Authorization`) must be listed by name; any other unsafe name is admitted by
/// name or by `*`.
fn fetch_admits(allow_headers: &str, requested: &[&str]) -> bool {
    let names: Vec<&str> = allow_headers.split(',').map(str::trim).collect();
    let listed = |want: &str| names.iter().any(|n| n.eq_ignore_ascii_case(want));
    requested
        .iter()
        .all(|r| listed(r) || (!r.eq_ignore_ascii_case("authorization") && listed("*")))
}

/// Shared preflight assertions; returns `Access-Control-Allow-Headers`.
async fn preflight(server: &Server, path: &str, method: &str, requested: &str) -> String {
    let response = wreq::Client::new()
        .request(wreq::Method::OPTIONS, format!("{}{path}", server.base))
        .header("Origin", ORIGIN)
        .header("Access-Control-Request-Method", method)
        .header("Access-Control-Request-Headers", requested)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        204,
        "{path}: preflight answers before routing and auth"
    );
    assert_eq!(header(&response, "access-control-allow-origin"), Some("*"), "{path}");
    assert_eq!(header(&response, "access-control-allow-credentials"), None, "{path}");
    assert_eq!(
        header(&response, "access-control-allow-methods"),
        Some("GET, POST, PUT, PATCH, DELETE, OPTIONS"),
        "{path}"
    );
    assert!(header(&response, "access-control-expose-headers").is_some(), "{path}");
    let allow = header(&response, "access-control-allow-headers").unwrap().to_owned();
    let names: Vec<&str> = requested.split(',').map(str::trim).collect();
    assert_ne!(allow, "*", "{path}: a wildcard never covers Authorization");
    assert!(
        fetch_admits(&allow, &names),
        "{path}: {allow:?} must admit {requested:?}"
    );
    assert!(response.bytes().await.unwrap().is_empty(), "{path}");
    allow
}

#[tokio::test]
async fn preflight_lists_authorization_and_requested_headers() {
    let server = serve().await;
    // The witness the issue names: Authorization with Content-Type.
    assert_eq!(
        preflight(&server, "/v1/chat/completions", "POST", "authorization,content-type").await,
        FIXED
    );
    assert_eq!(preflight(&server, "/v1/models", "GET", "authorization").await, FIXED);
    assert_eq!(
        preflight(
            &server,
            "/v1/messages",
            "POST",
            "x-api-key,anthropic-version,anthropic-beta"
        )
        .await,
        FIXED
    );
    // SDK headers outside the fixed list are echoed, so dropping `*` loses nothing.
    assert_eq!(
        preflight(
            &server,
            "/v1/responses",
            "POST",
            "authorization, x-stainless-os, openai-beta"
        )
        .await,
        format!("{FIXED}, X-Stainless-Os, Openai-Beta")
    );
    // Management and unknown paths share the global middleware, unauthenticated.
    assert_eq!(
        preflight(
            &server,
            "/v0/management/config",
            "GET",
            "authorization,x-management-key"
        )
        .await,
        format!("{FIXED}, X-Management-Key")
    );
    assert_eq!(
        preflight(&server, "/no/such/route", "DELETE", "authorization").await,
        FIXED
    );
}

#[tokio::test]
async fn ordinary_cross_origin_requests_keep_their_policy() {
    let server = serve().await;
    let client = wreq::Client::new();
    let get = |key: Option<&str>| {
        let mut request = client
            .get(format!("{}/v1/models", server.base))
            .header("Origin", ORIGIN);
        if let Some(key) = key {
            request = request.bearer_auth(key);
        }
        request.send()
    };
    for (key, status) in [(Some(CLIENT_KEY), 200), (None, 401), (Some("wrong-key"), 401)] {
        let response = get(key).await.unwrap();
        assert_eq!(response.status(), status, "{key:?}: authentication still decides");
        assert_eq!(header(&response, "access-control-allow-origin"), Some("*"));
        assert_eq!(header(&response, "access-control-allow-credentials"), None);
        assert_eq!(header(&response, "access-control-allow-headers"), Some(FIXED));
        let exposed = header(&response, "access-control-expose-headers").unwrap();
        assert!(exposed.split(',').any(|h| h.trim() == "ETag"), "{exposed}");
    }
    // A non-preflight request never echoes Access-Control-Request-Headers.
    let response = client
        .get(format!("{}/v1/models", server.base))
        .header("Origin", ORIGIN)
        .header("Access-Control-Request-Headers", "x-not-echoed")
        .bearer_auth(CLIENT_KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(header(&response, "access-control-allow-headers"), Some(FIXED));
}

#[test]
fn fetch_model_matches_the_spec_cases() {
    // The pre-fix answer: `*` admits custom headers but not Authorization.
    assert!(fetch_admits("*", &["content-type", "x-custom"]));
    assert!(!fetch_admits("*", &["authorization"]));
    assert!(fetch_admits("Authorization, *", &["authorization", "x-custom"]));
    assert!(!fetch_admits("Content-Type", &["authorization"]));
}
