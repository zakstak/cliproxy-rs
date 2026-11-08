//! #26 contract captures: what reaches a local upstream, over HTTP and the Responses
//! WebSocket, for each side of the identity policy (`crate::codex_identity`). Every
//! credential is fake, every upstream a loopback mock, and no test reads an auth dir.

use super::*;
use crate::codex_testkit::{Captured, Mock, Reply};
use axum::extract::ws::{Message as AxMessage, WebSocket as AxSocket, WebSocketUpgrade};
use cpa_core::exec::Caller;
use std::path::Path;
use std::sync::Mutex;

/// The proxy's identity, spelled out rather than taken from the implementation.
const PROXY_UA: &str = concat!("cliproxy-rs/", env!("CARGO_PKG_VERSION"));
/// What Go's cloaking and catalog override claimed. None may appear unless the caller sent it.
const FABRICATED: [&str; 5] = ["codex-tui", "Mac OS", "iTerm", "Codex Desktop", "Go-http-client"];
const CLIENT_KEY: &str = "Bearer client-key-FAKE";
const NATIVE_UA: &str = "codex_cli_rs/0.200.0 (Debian 13.0.0; x86_64) xterm-256color";
const SSE: &str = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n";
const RESPONSES_BODY: &str =
    r#"{"model":"gpt-5.4","input":[{"type":"message","role":"user","content":"hi"}],"stream":true,"store":false}"#;
const CHAT_BODY: &str = r#"{"model":"gpt-5.4","messages":[{"role":"user","content":"hi"}],"stream":true}"#;

fn credential(base_url: &str, api_key: bool, attributes: &[(&str, &str)]) -> Credential {
    let metadata = if api_key {
        serde_json::json!({"type": "codex"})
    } else {
        serde_json::json!({"type": "codex", "access_token": "at-FAKE", "account_id": "acct-SELECTED"})
    };
    let mut credential = Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/codex-identity.json"),
        metadata.as_object().unwrap().clone(),
    )
    .unwrap();
    credential.attributes.insert("base_url".into(), base_url.into());
    if api_key {
        credential
            .attributes
            .insert("api_key".into(), "sk-upstream-FAKE".into());
    }
    for (k, v) in attributes {
        credential.attributes.insert((*k).into(), (*v).into());
    }
    credential
}

/// A WebSocket turn as the server hands it over: the client's `response.create` frame.
fn frame(mut req: ExecRequest) -> ExecRequest {
    let tagged = cpa_common::codex_body::CodexBody::parse(&req.body)
        .unwrap()
        .with_member("type", r#""response.create""#);
    req.body = Bytes::from(tagged);
    req.original_body = req.body.clone();
    req
}

/// A caller request: `native` is Responses on both sides, otherwise Chat Completions.
fn request(native: bool, headers: &[(&str, &str)]) -> ExecRequest {
    let (format, body) = if native {
        (Format::OpenAIResponse, RESPONSES_BODY)
    } else {
        (Format::OpenAI, CHAT_BODY)
    };
    let body = Bytes::from_static(body.as_bytes());
    let mut map = HeaderMap::new();
    // Every caller authenticated to the proxy with its own key and tried to pick an account.
    map.insert("authorization", CLIENT_KEY.parse().unwrap());
    map.insert("chatgpt-account-id", "acct-CALLER".parse().unwrap());
    for (k, v) in headers {
        map.insert(http::HeaderName::try_from(*k).unwrap(), v.parse().unwrap());
    }
    ExecRequest {
        operation: Operation::Generate,
        source_format: format,
        response_format: format,
        requested_model: "gpt-5.4".into(),
        model: "gpt-5.4".into(),
        original_body: body.clone(),
        body,
        stream: true,
        alt: None,
        session: None,
        execution_session: Some("conn-identity".into()),
        derived_session: None,
        resolved_model: None,
        usage: Default::default(),
        request_path: String::new(),
        headers: map,
        caller: Caller {
            principal: "client-key-FAKE".into(),
            source: "authorization",
            provider: String::new(),
        },
    }
}

fn executor() -> CodexExecutor {
    CodexExecutor::with_client(wreq::Client::new(), CodexOAuth::new(wreq::Client::new()))
}

async fn drain(result: Result<ExecResponse, ExecError>) -> Result<(), ExecError> {
    if let ResponseBody::Stream(mut stream) = result?.body {
        while let Some(item) = stream.next().await {
            item?;
        }
    }
    Ok(())
}

fn with_base(credential: &Credential, base: &str) -> Credential {
    let mut credential = credential.clone();
    credential.attributes.insert("base_url".into(), base.into());
    credential
}

/// One HTTP turn; the captured upstream request.
async fn http_turn(credential: &Credential, req: ExecRequest, cfg: &Config) -> Captured {
    let mock = Mock::start().await;
    mock.script(
        "/responses",
        vec![Reply {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: SSE.into(),
        }],
    );
    let credential = with_base(credential, &mock.url);
    drain(executor().execute(&credential, req, cfg).await).await.unwrap();
    let mut captured = mock.take();
    assert_eq!(captured.len(), 1, "one upstream request");
    captured.pop().unwrap()
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// Nothing in `headers` claims an identity the caller did not send, and no caller
/// credential or account choice got through.
fn assert_isolated(headers: &HeaderMap, caller: &HeaderMap, what: &str) {
    for (name, value) in headers {
        let value = value.to_str().unwrap_or_default();
        let sent_by_caller = caller.get(name).is_some_and(|v| v.to_str().ok() == Some(value));
        for claim in FABRICATED {
            assert!(
                sent_by_caller || !value.contains(claim),
                "{what}: {name}: {value:?} claims {claim}"
            );
        }
        assert!(!value.contains("client-key-FAKE"), "{what}: client key in {name}");
        assert!(!value.contains("acct-CALLER"), "{what}: caller account in {name}");
    }
}

/// Both sides of the HTTP policy, on OAuth and API-key credentials, with and without
/// the obsolete cloaking switch set either way in config and as a credential attribute.
#[tokio::test]
async fn http_identity_follows_the_policy_and_never_borrows_caller_credentials() {
    let native_caller = [("user-agent", NATIVE_UA), ("originator", "codex_cli_rs")];
    let other_caller = [("user-agent", "OpenAI/Python 2.1.0")];
    let configs = [
        "",
        "upstream: {codex: {disable-codex-cloaking: false}}\n",
        "upstream: {codex: {disable-codex-cloaking: true}}\n",
    ];
    for cfg_text in configs {
        let cfg = Config::parse(cfg_text).unwrap();
        for attribute in [None, Some("false"), Some("true")] {
            let attrs: Vec<(&str, &str)> = attribute.map(|v| ("codex_disable_cloaking", v)).into_iter().collect();
            for api_key in [false, true] {
                let cred = credential("http://unused", api_key, &attrs);
                let ctx = format!("{cfg_text:?} attr={attribute:?} api_key={api_key}");
                let auth = if api_key {
                    "Bearer sk-upstream-FAKE"
                } else {
                    "Bearer at-FAKE"
                };
                let account = (!api_key).then_some("acct-SELECTED");
                let originator = (!api_key).then_some("cliproxy-rs");

                // A native caller's own identity passes through unchanged.
                let req = request(true, &native_caller);
                let caller = req.headers.clone();
                let up = http_turn(&cred, req, &cfg).await;
                assert_eq!(up.header("user-agent"), NATIVE_UA, "{ctx}");
                assert_eq!(up.header("originator"), "codex_cli_rs", "{ctx}");
                assert_eq!(up.header("authorization"), auth, "{ctx}");
                assert_eq!(header(&up.headers, "chatgpt-account-id"), account, "{ctx}");
                assert_isolated(&up.headers, &caller, &ctx);

                // A native caller without identity headers: the proxy names itself.
                let req = request(true, &[]);
                let caller = req.headers.clone();
                let up = http_turn(&cred, req, &cfg).await;
                assert_eq!(up.header("user-agent"), PROXY_UA, "{ctx}");
                assert_eq!(header(&up.headers, "originator"), originator, "{ctx}");
                assert_eq!(up.header("authorization"), auth, "{ctx}");
                assert_isolated(&up.headers, &caller, &ctx);

                // A translated request is the proxy's: the caller's identity stays home.
                let req = request(false, &other_caller);
                let caller = req.headers.clone();
                let up = http_turn(&cred, req, &cfg).await;
                assert_eq!(up.header("user-agent"), PROXY_UA, "{ctx}");
                assert_eq!(header(&up.headers, "originator"), originator, "{ctx}");
                assert_eq!(up.header("authorization"), auth, "{ctx}");
                assert_isolated(&up.headers, &caller, &ctx);
            }
        }
    }
}

/// The pinned catalog gives `gpt-5.6-luna` a codex-tui macOS identity override; it
/// reaches no request.
#[tokio::test]
async fn the_model_catalog_cannot_set_identity() {
    let info = cpa_core::registry::pinned()
        .lookup("gpt-5.6-luna")
        .expect("pinned model");
    let overrides = &info.raw["config"]["override_header"];
    assert!(
        overrides["user-agent"]
            .as_str()
            .is_some_and(|ua| ua.contains("codex-tui")),
        "the catalog still carries the override this test guards against"
    );
    let cred = credential("http://unused", false, &[]);
    let mut req = request(true, &[]);
    req.model = "gpt-5.6-luna".into();
    req.requested_model = req.model.clone();
    req.body = Bytes::from(RESPONSES_BODY.replace("gpt-5.4", "gpt-5.6-luna"));
    req.original_body = req.body.clone();
    let caller = req.headers.clone();
    let up = http_turn(&cred, req, &Config::default()).await;
    assert_eq!(up.header("user-agent"), PROXY_UA);
    assert_eq!(up.header("originator"), "cliproxy-rs");
    assert_isolated(&up.headers, &caller, "gpt-5.6-luna");
}

/// Operator headers that would bring back a chosen identity, let the caller pick the
/// credential, or copy the proxy client key are refused before anything goes upstream,
/// over HTTP and the WebSocket, whichever source the attribute came from.
#[tokio::test]
async fn operator_headers_cannot_bypass_the_policy() {
    for (name, value) in [
        ("header:User-Agent", "codex-tui/0.154.0 (Mac OS 26.5.2; arm64)"),
        ("header:Originator", "codex-tui"),
        ("header:Authorization", "$Authorization"),
        ("header:chatgpt-account-id", "$Chatgpt-Account-Id"),
        ("header:X-Upstream-Key", "$Authorization"),
        ("header:X-Upstream-Key", "$X-Api-Key"),
    ] {
        for websocket in [false, true] {
            let mock = Mock::start().await;
            let mut attrs = vec![(name, value)];
            if websocket {
                attrs.push(("websockets", "true"));
            }
            let cred = credential(&mock.url, false, &attrs);
            let req = request(true, &[("user-agent", NATIVE_UA), ("x-api-key", "client-key-FAKE")]);
            let executor = executor();
            let result = if websocket {
                let session = ExecSession {
                    id: "conn-identity".into(),
                    continuation: false,
                    lease: None,
                };
                executor
                    .execute_in_session(&cred, frame(req), &Config::default(), &session)
                    .await
            } else {
                executor.execute(&cred, req, &Config::default()).await
            };
            let error = result
                .err()
                .unwrap_or_else(|| panic!("{name}={value} ws={websocket}: refused"));
            assert_eq!(error.scope, FailureScope::Credential, "{name}");
            let message = String::from_utf8_lossy(&error.body).into_owned();
            assert!(message.contains(name.trim_start_matches("header:")), "{message}");
            assert!(
                !message.contains("client-key-FAKE") && !message.contains("Mac OS"),
                "{message}"
            );
            assert!(
                mock.take().is_empty(),
                "{name}={value} ws={websocket}: nothing upstream"
            );
        }
    }
    // A static operator header, or one that forwards a harmless client header, still goes.
    let cred = credential(
        "http://unused",
        false,
        &[("header:X-Static", "fixed"), ("header:X-Trace", "$X-Request-Id")],
    );
    let req = request(true, &[("x-request-id", "trace-1")]);
    let up = http_turn(&cred, req, &Config::default()).await;
    assert_eq!((up.header("x-static"), up.header("x-trace")), ("fixed", "trace-1"));
}

/// Alpha Search: Go fixed `originator: codex_cli_rs` over the caller's.
#[tokio::test]
async fn alpha_search_sends_the_callers_identity_or_the_proxys() {
    let native = [("user-agent", NATIVE_UA), ("originator", "codex_cli_rs")];
    for (client, ua, originator) in [
        (&native[..], NATIVE_UA, "codex_cli_rs"),
        (&[][..], PROXY_UA, "cliproxy-rs"),
    ] {
        let mock = Mock::start().await;
        mock.script(
            "/alpha/search",
            vec![Reply {
                status: 200,
                headers: vec![("content-type".into(), "application/json".into())],
                body: "{}".into(),
            }],
        );
        let executor = executor().with_alpha_base_url(&mock.url);
        let cred = credential(&mock.url, false, &[]);
        let caller = request(true, client).headers;
        executor
            .alpha_search(&cred, b"{}", &caller, "", &Config::default(), &Default::default())
            .await
            .unwrap();
        let up = mock.take().pop().unwrap();
        assert_eq!((up.header("user-agent"), up.header("originator")), (ua, originator));
        assert_eq!(up.header("authorization"), "Bearer at-FAKE");
        assert_eq!(up.header("chatgpt-account-id"), "acct-SELECTED");
        assert_isolated(&up.headers, &caller, "alpha search");
    }
}

/// A local Responses WebSocket upstream that records each handshake's headers and
/// answers every frame with a completed response.
#[derive(Default)]
struct WsUpstream {
    dials: Mutex<Vec<HeaderMap>>,
}

async fn ws_handler(
    axum::extract::State(up): axum::extract::State<std::sync::Arc<WsUpstream>>,
    ws: WebSocketUpgrade,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    let mut converted = HeaderMap::new();
    for (k, v) in &headers {
        converted.append(
            http::HeaderName::from_bytes(k.as_str().as_bytes()).unwrap(),
            http::HeaderValue::from_bytes(v.as_bytes()).unwrap(),
        );
    }
    up.dials.lock().unwrap().push(converted);
    ws.on_upgrade(|mut socket: AxSocket| async move {
        while let Some(Ok(message)) = socket.recv().await {
            if matches!(message, AxMessage::Text(_)) {
                for event in [
                    r#"{"type":"response.created","response":{"id":"r1","output":[]}}"#,
                    r#"{"type":"response.completed","response":{"id":"r1","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#,
                ] {
                    let _ = socket.send(AxMessage::Text(event.into())).await;
                }
            }
        }
    })
}

async fn ws_upstream() -> (std::sync::Arc<WsUpstream>, String) {
    let up = std::sync::Arc::new(WsUpstream::default());
    let app = axum::Router::new().fallback(ws_handler).with_state(up.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (up, url)
}

/// The WebSocket handshake follows the same policy, keeps the protocol beta header, and
/// never carries a caller credential, whatever the obsolete cloaking switch says.
#[tokio::test]
async fn websocket_handshake_follows_the_policy() {
    let native = [("user-agent", NATIVE_UA), ("originator", "codex_cli_rs")];
    for cfg_text in ["", "upstream: {codex: {disable-codex-cloaking: false}}\n"] {
        let cfg = Config::parse(cfg_text).unwrap();
        for api_key in [false, true] {
            for (client, ua, caller_originator) in [
                (&native[..], NATIVE_UA, Some("codex_cli_rs")),
                (&[][..], PROXY_UA, None),
            ] {
                let (up, url) = ws_upstream().await;
                let cred = credential(
                    &url,
                    api_key,
                    &[("websockets", "true"), ("codex_disable_cloaking", "false")],
                );
                let req = request(true, client);
                let caller = req.headers.clone();
                let session = ExecSession {
                    id: "conn-identity".into(),
                    continuation: false,
                    lease: None,
                };
                let executor = executor();
                drain(executor.execute_in_session(&cred, frame(req), &cfg, &session).await)
                    .await
                    .unwrap();
                executor.close_session("conn-identity");
                let dials = up.dials.lock().unwrap().clone();
                assert_eq!(dials.len(), 1);
                let h = &dials[0];
                let ctx = format!("{cfg_text:?} api_key={api_key} ua={ua}");
                assert_eq!(header(h, "user-agent"), Some(ua), "{ctx}");
                let originator = caller_originator.or((!api_key).then_some("cliproxy-rs"));
                assert_eq!(header(h, "originator"), originator, "{ctx}");
                let auth = if api_key {
                    "Bearer sk-upstream-FAKE"
                } else {
                    "Bearer at-FAKE"
                };
                assert_eq!(header(h, "authorization"), Some(auth), "{ctx}");
                assert_eq!(
                    header(h, "chatgpt-account-id"),
                    (!api_key).then_some("acct-SELECTED"),
                    "{ctx}"
                );
                assert_eq!(header(h, "openai-beta"), Some(crate::codex_request::WS_BETA), "{ctx}");
                assert_isolated(h, &caller, &ctx);
            }
        }
    }
}

/// A Sign in with ChatGPT credential: its nested token authenticates, and it carries no
/// account header.
fn siwc_credential(base_url: &str, websockets: bool) -> Credential {
    let mut credential = Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/codex-siwc.json"),
        serde_json::json!({"type": "codex", "access_token": "outer-at-FAKE", "account_id": "acct-SELECTED",
                           "use_siwc": true, "siwc": {"access_token": "siwc-at-FAKE"}})
        .as_object()
        .unwrap()
        .clone(),
    )
    .unwrap();
    credential.attributes.insert("base_url".into(), base_url.into());
    if websockets {
        credential.attributes.insert("websockets".into(), "true".into());
    }
    credential
}

/// Go builds the Sign in with ChatGPT `User-Agent` from the operator's
/// `signin-with-chatgpt-identifier` (default `CLIProxyAPI`), so any identity could be
/// configured. The identifier now only labels the consent screen; requests follow the
/// policy over HTTP and the WebSocket, and keep the SIWC token as their credential.
#[tokio::test]
async fn siwc_requests_follow_the_policy_not_the_configured_identifier() {
    let cfg = Config::parse(
        "oauth: {providers: {codex: {signin-with-chatgpt-identifier: 'codex-tui/0.154.0 (Mac OS 26.5.2; arm64)'}}}\n",
    )
    .unwrap();
    // Native over HTTP, with and without the caller's identity; translated over HTTP.
    for (native, client, ua) in [
        (true, &[("user-agent", NATIVE_UA)][..], NATIVE_UA),
        (true, &[][..], PROXY_UA),
        (false, &[("user-agent", "OpenAI/Python 2.1.0")][..], PROXY_UA),
    ] {
        let req = request(native, client);
        let caller = req.headers.clone();
        let up = http_turn(&siwc_credential("http://unused", false), req, &cfg).await;
        let ctx = format!("http native={native}");
        assert_eq!(up.header("user-agent"), ua, "{ctx}");
        assert_eq!(up.header("authorization"), "Bearer siwc-at-FAKE", "{ctx}");
        assert_eq!(header(&up.headers, "chatgpt-account-id"), None, "{ctx}");
        assert_eq!(header(&up.headers, "originator"), None, "{ctx}");
        assert!(!up.header("user-agent").contains("CLIProxyAPI"), "{ctx}");
        assert_isolated(&up.headers, &caller, &ctx);
    }
    // Native over the WebSocket.
    for (client, ua) in [(&[("user-agent", NATIVE_UA)][..], NATIVE_UA), (&[][..], PROXY_UA)] {
        let (up, url) = ws_upstream().await;
        let req = request(true, client);
        let caller = req.headers.clone();
        let session = ExecSession {
            id: "conn-identity".into(),
            continuation: false,
            lease: None,
        };
        let executor = executor();
        drain(
            executor
                .execute_in_session(&siwc_credential(&url, true), frame(req), &cfg, &session)
                .await,
        )
        .await
        .unwrap();
        executor.close_session("conn-identity");
        let dials = up.dials.lock().unwrap().clone();
        assert_eq!(dials.len(), 1);
        let h = &dials[0];
        assert_eq!(header(h, "user-agent"), Some(ua), "ws");
        assert_eq!(header(h, "authorization"), Some("Bearer siwc-at-FAKE"), "ws");
        assert_eq!(header(h, "chatgpt-account-id"), None, "ws");
        assert_eq!(header(h, "openai-beta"), Some(crate::codex_request::WS_BETA), "ws");
        assert_isolated(h, &caller, "ws");
    }
    // The identifier still names the agent on the consent screen.
    let url = crate::codex_siwc::authorize_url(
        "s",
        "n",
        "00000000-0000-4000-8000-000000000000",
        "http://127.0.0.1:1455/auth/callback",
        "c",
        &crate::codex_siwc::identifier(&cfg),
    )
    .unwrap();
    let hint = url::Url::parse(&url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "agent_name_hint")
        .map(|(_, v)| v.into_owned());
    assert_eq!(hint.as_deref(), Some("codex-tui/0.154.0 (Mac OS 26.5.2; arm64)"));
}
