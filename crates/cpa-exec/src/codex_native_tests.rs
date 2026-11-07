//! The native HTTP route against the #21 contract fixtures: what reaches the upstream
//! and what reaches the client, byte for byte, with each permitted transformation
//! asserted explicitly.

use std::path::Path;

use cpa_core::exec::Caller;

use super::*;
use crate::codex_contract::fixture;
use crate::codex_testkit::{Mock, Reply, json};

fn executor() -> CodexExecutor {
    CodexExecutor::with_client(
        wreq::Client::new(),
        crate::codex_oauth::CodexOAuth::new(wreq::Client::new()),
    )
}

#[derive(Clone, Copy)]
enum Kind {
    OAuth,
    Key,
    Siwc,
}

fn credential(kind: Kind, base_url: &str) -> Credential {
    let mut metadata = serde_json::json!({"type":"codex","access_token":"at-FAKE","account_id":"acct-FAKE"});
    if matches!(kind, Kind::Siwc) {
        metadata["use_siwc"] = true.into();
        metadata["siwc"] = serde_json::json!({"access_token": "siwc-at-FAKE"});
    }
    let mut c = Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/codex-native.json"),
        metadata.as_object().unwrap().clone(),
    )
    .unwrap();
    c.attributes.insert("base_url".into(), base_url.into());
    if matches!(kind, Kind::Key) {
        c.attributes.insert("api_key".into(), "key-FAKE".into());
    }
    c
}

fn request(body: impl Into<Bytes>, model: &str, stream: bool, alt: Option<&str>) -> ExecRequest {
    let body = body.into();
    ExecRequest {
        operation: Operation::Generate,
        source_format: Format::OpenAIResponse,
        response_format: Format::OpenAIResponse,
        stream_payloads: false,
        requested_model: model.into(),
        model: model.into(),
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
            principal: "client-key-FAKE".into(),
            source: "authorization",
            provider: String::new(),
        },
    }
}

fn sse(name: &str) -> Reply {
    Reply {
        status: 200,
        headers: vec![("content-type".into(), "text/event-stream".into())],
        body: String::from_utf8(fixture(name).bytes().to_vec()).unwrap(),
    }
}

fn sse_text(body: &str) -> Reply {
    Reply {
        status: 200,
        headers: vec![("content-type".into(), "text/event-stream".into())],
        body: body.into(),
    }
}

/// Everything the client stream carried, and the error that ended it, if any.
async fn drain(response: ExecResponse) -> (Vec<u8>, Option<ExecError>) {
    let ResponseBody::Stream(mut stream) = response.body else {
        panic!("expected a stream");
    };
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(bytes) => out.extend_from_slice(&bytes),
            Err(error) => return (out, Some(error)),
        }
    }
    (out, None)
}

fn buffered(response: ExecResponse) -> Bytes {
    match response.body {
        ResponseBody::Buffered(body) => body,
        ResponseBody::Stream(_) => panic!("expected a buffered body"),
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

const REQUESTS: [&str; 5] = [
    "http-request-full",
    "http-request-lite",
    "http-request-compaction",
    "http-request-minimal",
    "probe-request-unknown-fields",
];

/// A native streaming request reaches the upstream byte for byte, and the upstream's
/// events reach the client byte for byte, on every native profile.
#[tokio::test]
async fn streaming_requests_and_events_pass_unchanged() {
    let mock = Mock::start().await;
    let cfg = Config::default();
    for kind in [Kind::OAuth, Kind::Key] {
        let credential = credential(kind, &mock.url);
        for name in REQUESTS {
            mock.script("/responses", vec![sse("sse-completed")]);
            let body = fixture(name).bytes();
            let response = executor()
                .execute(&credential, request(body.clone(), "gpt-test", true, None), &cfg)
                .await
                .unwrap_or_else(|e| panic!("{name}: {e:?}"));
            let (out, error) = drain(response).await;
            assert!(error.is_none(), "{name}: {error:?}");
            assert_eq!(
                text(&out),
                text(&fixture("sse-completed").bytes()),
                "{name}: client events"
            );
            let [sent] = mock.take().try_into().unwrap();
            assert_eq!(sent.path, "/responses");
            assert_eq!(text(&sent.body), text(&body), "{name}: upstream body");
            assert_eq!(
                sent.header("authorization"),
                "Bearer at-FAKE".replace(
                    "at-FAKE",
                    match kind {
                        Kind::Key => "key-FAKE",
                        _ => "at-FAKE",
                    }
                )
            );
        }
    }
}

/// Routing to an alias's upstream model changes the `model` value and nothing else.
#[tokio::test]
async fn an_alias_changes_only_the_model_value() {
    let mock = Mock::start().await;
    mock.script("/responses", vec![sse("sse-completed")]);
    let body = fixture("http-request-full").bytes();
    let response = executor()
        .execute(
            &credential(Kind::OAuth, &mock.url),
            request(body.clone(), "gpt-upstream", true, None),
            &Config::default(),
        )
        .await
        .unwrap();
    drain(response).await;
    let [sent] = mock.take().try_into().unwrap();
    let expected = text(&body).replacen(r#""model":"gpt-test""#, r#""model":"gpt-upstream""#, 1);
    assert_eq!(text(&sent.body), expected);
}

/// A buffered client on the `chatgpt` profile: the upstream gets `stream: true` (the only
/// change), and the client gets the terminal event's `response` object byte for byte.
#[tokio::test]
async fn chatgpt_buffered_clients_get_the_terminal_response_object() {
    let mock = Mock::start().await;
    let completed = fixture("sse-completed");
    let terminal = completed.frames().pop().unwrap();
    let terminal = text(&terminal);
    let object = &terminal[terminal.find(r#""response":"#).unwrap() + 11..terminal.len() - 1];
    let full = text(&fixture("http-request-full").bytes());
    for (body, upstream) in [
        (full.replacen(r#""stream":true"#, r#""stream":false"#, 1), full.clone()),
        (
            full.replacen(r#""stream":true,"#, "", 1),
            full.replacen(r#""model":"gpt-test","stream":true,"#, r#""model":"gpt-test","#, 1)
                .replacen('{', r#"{"stream":true,"#, 1),
        ),
    ] {
        mock.script("/responses", vec![sse("sse-completed")]);
        let response = executor()
            .execute(
                &credential(Kind::OAuth, &mock.url),
                request(body.clone(), "gpt-test", false, None),
                &Config::default(),
            )
            .await
            .unwrap();
        assert_eq!(text(&buffered(response)), object);
        let [sent] = mock.take().try_into().unwrap();
        assert_eq!(text(&sent.body), upstream);
    }
}

/// On an API key, a buffered request and its JSON answer pass unchanged both ways.
#[tokio::test]
async fn api_key_buffered_requests_pass_unchanged_both_ways() {
    let mock = Mock::start().await;
    let answer = r#"{"id":"resp_1","object":"response","status":"completed","output":[],"usage":null,"future":1.50}"#;
    mock.script("/responses", vec![json(200, answer)]);
    let body = text(&fixture("http-request-full").bytes()).replacen(r#""stream":true"#, r#""stream":false"#, 1);
    let response = executor()
        .execute(
            &credential(Kind::Key, &mock.url),
            request(body.clone(), "gpt-test", false, None),
            &Config::default(),
        )
        .await
        .unwrap();
    assert_eq!(text(&buffered(response)), answer);
    let [sent] = mock.take().try_into().unwrap();
    assert_eq!(text(&sent.body), body);
}

/// Compaction keeps its body (including `stream: false`) and its answer.
#[tokio::test]
async fn compaction_passes_unchanged_both_ways() {
    let mock = Mock::start().await;
    let answer = r#"{"id":"cmp_1","object":"response.compaction","output":[{"type":"compaction","encrypted_content":"ENC"}],"usage":{"input_tokens":5}}"#;
    mock.script("/responses/compact", vec![json(200, answer)]);
    let body = text(&fixture("http-request-compaction").bytes()).replacen(r#""stream":true"#, r#""stream":false"#, 1);
    let response = executor()
        .execute(
            &credential(Kind::OAuth, &mock.url),
            request(body.clone(), "gpt-test", false, Some("responses/compact")),
            &Config::default(),
        )
        .await
        .unwrap();
    assert_eq!(text(&buffered(response)), answer);
    let [sent] = mock.take().try_into().unwrap();
    assert_eq!(sent.path, "/responses/compact");
    assert_eq!(text(&sent.body), body);
}

/// Events the proxy does not model, and a stream without usage, reach the client as sent.
#[tokio::test]
async fn unknown_events_and_missing_usage_pass_unchanged() {
    let mock = Mock::start().await;
    let stream = concat!(
        "event: codex.rate_limits\ndata: {\"type\":\"codex.rate_limits\",\"rate_limits\":{\"primary\":{\"used_percent\":1.50}}}\n\n",
        ": comment\n\n",
        "data: {\"type\":\"responsesapi.websocket_timing\",\"t\":1}\n\n",
        "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_2\"}}\n\n",
        "data: {\"type\":\"response.future_event\",\"x\":12345678901234567890}\n\n",
        "data: {\"type\":\"response.output_item.interrupted\",\"item_id\":\"rs_1\",\"output_index\":0}\n\n",
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_2\"}}\n\n",
    );
    for body in [stream.to_owned(), text(&fixture("sse-completed-no-usage").bytes())] {
        mock.script("/responses", vec![sse_text(&body)]);
        let response = executor()
            .execute(
                &credential(Kind::OAuth, &mock.url),
                request(fixture("http-request-full").bytes(), "gpt-test", true, None),
                &Config::default(),
            )
            .await
            .unwrap();
        let (out, error) = drain(response).await;
        assert!(error.is_none(), "{error:?}");
        assert_eq!(text(&out), body);
    }
}

/// A request fault (here the context window) reaches the client as the provider's
/// event; the error after it only ends the attempt.
#[tokio::test]
async fn request_faults_reach_the_client_as_the_provider_event() {
    let mock = Mock::start().await;
    for name in ["sse-failed-context-window", "sse-incomplete-partial"] {
        mock.script("/responses", vec![sse(name)]);
        let response = executor()
            .execute(
                &credential(Kind::OAuth, &mock.url),
                request(fixture("http-request-full").bytes(), "gpt-test", true, None),
                &Config::default(),
            )
            .await
            .unwrap();
        let (out, _) = drain(response).await;
        assert_eq!(text(&out), text(&fixture(name).bytes()), "{name}");
    }
}

/// A credential failure before any content fails the attempt, so another credential
/// may serve the request; the error carries the provider's error object for the client.
#[tokio::test]
async fn credential_failures_before_content_fail_over() {
    let mock = Mock::start().await;
    let failed = r#"{"type":"response.failed","response":{"id":"resp_3","status":"failed","error":{"type":"usage_limit_reached","code":"usage_limit_reached","message":"The usage limit has been reached","resets_at":1738888888}}}"#;
    mock.script(
        "/responses",
        vec![sse_text(&format!(
            "event: response.created\ndata: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp_3\"}}}}\n\nevent: response.failed\ndata: {failed}\n\n"
        ))],
    );
    let mut settings = String::from("upstream:\n  codex:\n    stream-bootstrap-buffering: true\n");
    settings.push_str("    stream-bootstrap-timeout: 30s\n");
    let cfg = Config::parse(&settings).unwrap();
    let response = executor()
        .execute(
            &credential(Kind::OAuth, &mock.url),
            request(fixture("http-request-full").bytes(), "gpt-test", true, None),
            &cfg,
        )
        .await
        .unwrap();
    let (out, error) = drain(response).await;
    assert!(out.is_empty(), "nothing reaches the client: {}", text(&out));
    let error = error.expect("the attempt fails");
    assert_ne!(error.scope, FailureScope::Request, "{error:?}");
    assert!(error.direct);
    assert!(
        text(&error.body).contains(r#""resets_at":1738888888"#),
        "{}",
        text(&error.body)
    );
}

/// An upstream HTTP error keeps its classification for the scheduler and its body for
/// the client.
#[tokio::test]
async fn upstream_http_errors_reach_the_client_unchanged() {
    let mock = Mock::start().await;
    let body = text(&fixture("http-error-usage-limit-429").bytes());
    mock.script("/responses", vec![json(429, &body)]);
    let error = executor()
        .execute(
            &credential(Kind::OAuth, &mock.url),
            request(fixture("http-request-full").bytes(), "gpt-test", true, None),
            &Config::default(),
        )
        .await
        .err()
        .expect("an error");
    assert_eq!(error.status, 429);
    assert!(error.direct);
    assert_eq!(text(&error.body), body);
    assert_ne!(error.scope, FailureScope::Request);
}

#[test]
fn sign_in_with_chatgpt_keeps_the_legacy_path() {
    let req = request(fixture("http-request-full").bytes(), "gpt-test", true, None);
    for (kind, native) in [(Kind::OAuth, true), (Kind::Key, true), (Kind::Siwc, false)] {
        let credential = credential(kind, "http://127.0.0.1:1");
        assert_eq!(CodexExecutor::native_http(&credential, &req), native);
    }
    let mut chat = req.clone();
    chat.source_format = Format::OpenAI;
    assert!(!CodexExecutor::native_http(
        &credential(Kind::OAuth, "http://127.0.0.1:1"),
        &chat
    ));
}

#[tokio::test]
async fn a_model_suffix_is_refused_before_the_upstream() {
    let mock = Mock::start().await;
    let error = executor()
        .execute(
            &credential(Kind::OAuth, &mock.url),
            request(fixture("http-request-full").bytes(), "gpt-test(high)", true, None),
            &Config::default(),
        )
        .await
        .err()
        .expect("refused");
    assert_eq!((error.status, error.scope), (400, FailureScope::Request));
    assert!(text(&error.body).contains("`reasoning`"));
    assert!(mock.take().is_empty());
}

/// Payload rules never reach a native body.
#[tokio::test]
async fn payload_rules_do_not_apply() {
    let mock = Mock::start().await;
    mock.script("/responses", vec![sse("sse-completed")]);
    let cfg = Config::parse(
        "payload:\n  override:\n    - models: [{name: gpt-test}]\n      params: {instructions: injected, store: true}\n",
    )
    .unwrap();
    let body = fixture("http-request-full").bytes();
    let response = executor()
        .execute(
            &credential(Kind::OAuth, &mock.url),
            request(body.clone(), "gpt-test", true, None),
            &cfg,
        )
        .await
        .unwrap();
    drain(response).await;
    let [sent] = mock.take().try_into().unwrap();
    assert_eq!(text(&sent.body), text(&body));
}

/// An event whose data spans several `data:` lines reaches the client as sent; the
/// legacy scanner joined such lines.
#[tokio::test]
async fn split_data_lines_pass_unchanged() {
    let mock = Mock::start().await;
    let stream = "event: response.created\ndata: {\"type\":\"response.created\",\n\
                  data: \"response\":{\"id\":\"r\"}}\n\n\
                  event: response.completed\r\ndata: {\"type\":\"response.completed\",\r\n\
                  data: \"response\":{\"id\":\"r\",\"output\":[]}}\r\nid: after-terminal\r\n\r\n";
    mock.script("/responses", vec![sse_text(stream)]);
    let response = executor()
        .execute(
            &credential(Kind::OAuth, &mock.url),
            request(fixture("http-request-full").bytes(), "gpt-test", true, None),
            &Config::default(),
        )
        .await
        .unwrap();
    let (out, error) = drain(response).await;
    assert!(error.is_none(), "{error:?}");
    assert_eq!(text(&out), stream);
}
