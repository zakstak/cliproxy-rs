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

/// Every Codex profile takes the native route (#28), Sign in with ChatGPT included: no
/// first-party source covers its Responses behaviour, so the proxy adds nothing. Go's
/// SIWC body rules (`store`, `stream`, dropped `temperature`, `max_output_tokens`,
/// `prompt_cache_retention` and blank `instructions`, collapsed `configuration_update`
/// items) no longer run; a buffered request keeps the client's `stream: false`. A Chat
/// Completions request is not native on any profile.
#[tokio::test]
async fn sign_in_with_chatgpt_takes_the_native_route() {
    let req = request(fixture("http-request-full").bytes(), "gpt-test", true, None);
    for kind in [Kind::OAuth, Kind::Key, Kind::Siwc] {
        let credential = credential(kind, "http://127.0.0.1:1");
        assert!(CodexExecutor::native_http(&credential, &req));
    }
    let mut chat = req.clone();
    chat.source_format = Format::OpenAI;
    assert!(!CodexExecutor::native_http(
        &credential(Kind::Siwc, "http://127.0.0.1:1"),
        &chat
    ));
    let body = r#"{"model":"gpt-test","stream":false,"store":true,"temperature":0.5,"max_output_tokens":9,"prompt_cache_retention":"24h","instructions":" ","input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"type":"message","role":"user","content":"hi"}]}"#;
    let mock = Mock::start().await;
    mock.script(
        "/responses",
        vec![json(
            200,
            r#"{"id":"resp_1","object":"response","status":"completed","output":[]}"#,
        )],
    );
    let out = executor()
        .execute(
            &credential(Kind::Siwc, &mock.url),
            request(body, "gpt-test", false, None),
            &Config::default(),
        )
        .await
        .unwrap();
    let sent = mock.take();
    assert_eq!(sent.len(), 1);
    assert_eq!(text(&sent[0].body), body, "the client's bytes, unchanged");
    let ResponseBody::Buffered(answer) = out.body else {
        panic!("a buffered answer")
    };
    assert_eq!(
        text(&answer),
        r#"{"id":"resp_1","object":"response","status":"completed","output":[]}"#
    );
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

fn observer() -> Observer {
    Observer {
        usage: Default::default(),
        wire: Default::default(),
        capture_lines: true,
        model_level_cooling: false,
        codex_client: false,
        items: 0,
        saw_delta: false,
    }
}

/// `chunks` through the shared framer and the native stream: what the client receives
/// and the error that ended it.
async fn through(chunks: &[&[u8]], hold: Option<Option<Duration>>) -> (Vec<u8>, Option<ExecError>) {
    let upstream = futures_util::stream::iter(
        chunks
            .iter()
            .map(|c| Ok::<_, ExecError>(Bytes::copy_from_slice(c)))
            .collect::<Vec<_>>(),
    );
    let mut stream = passthrough(crate::upstream::framed(upstream), observer(), hold, false);
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(bytes) => out.extend_from_slice(&bytes),
            Err(error) => return (out, Some(error)),
        }
    }
    (out, None)
}

const CREATED: &str = r#"{"type":"response.created","response":{"id":"r"}}"#;
const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"r","output":[],"n":12345678901234567890}}"#;

fn framed_with(eol: &str) -> String {
    format!(
        "event: response.created{eol}data: {CREATED}{eol}{eol}event: response.completed{eol}data: {COMPLETED}{eol}{eol}"
    )
}

/// CR-only, CRLF and mixed line endings, in one piece or split anywhere (between `\r`
/// and `\n` included), with and without held handshake events: the client gets the
/// exact bytes and the terminal event is recognized.
#[tokio::test]
async fn every_sse_line_ending_and_split_passes_unchanged() {
    let mixed =
        format!("event: response.created\r\ndata: {CREATED}\r\r\nevent: response.completed\rdata: {COMPLETED}\n\r\n");
    for body in [framed_with("\r"), framed_with("\r\n"), framed_with("\n"), mixed] {
        let bytes = body.as_bytes();
        for hold in [None, Some(None), Some(Some(Duration::from_secs(30)))] {
            for split in [bytes.len(), 1, 7, bytes.len() / 2] {
                let chunks: Vec<&[u8]> = bytes.chunks(split.max(1)).collect();
                let (out, error) = through(&chunks, hold).await;
                assert!(error.is_none(), "{body:?} split={split}: {error:?}");
                assert_eq!(text(&out), body, "split={split}");
            }
        }
        // A buffered answer reads the same terminal event.
        let object = observer().aggregate(bytes).unwrap();
        assert_eq!(
            text(&object),
            r#"{"id":"r","output":[],"n":12345678901234567890}"#,
            "{body:?}"
        );
    }
}

/// A large multiline event joins in one pass: 60 000 `data:` lines, 3 MB of payload.
#[test]
fn a_large_multiline_event_parses_linearly() {
    let lines = 60_000;
    let mut frame = String::from("event: response.output_text.delta\n");
    for i in 0..lines {
        frame.push_str(&format!("data: {i:0>48}\n"));
    }
    frame.push('\n');
    let (_, data) = parse_frame(frame.as_bytes());
    let data = data.unwrap();
    assert_eq!(data.len(), lines * 48 + (lines - 1));
    assert!(data.starts_with(&format!("{:0>48}\n", 0)));
    assert!(data.ends_with(&format!("{:0>48}", lines - 1)));
}

/// A failure the client does not see as an event carries the provider's error object
/// byte for byte: no message filled in, unknown fields and number spellings kept.
#[tokio::test]
async fn failures_deliver_the_raw_error_object() {
    let error =
        r#"{"type":"usage_limit_reached","message":null,"resets_at":12345678901234567890,"x_future":{"n":1.50}}"#;
    let omitted = r#"{"code":"usage_limit_reached","x_future":[]}"#;
    for raw in [error, omitted] {
        let failed =
            format!(r#"{{"type":"response.failed","sequence_number":7.0,"response":{{"id":"r","error":{raw}}}}}"#);
        let expected = format!(r#"{{"error":{raw},"sequence_number":7.0}}"#);
        // Before anything was delivered, on a held stream: the attempt fails with it.
        let sse = format!("data: {CREATED}\n\ndata: {failed}\n\n");
        let (out, error) = through(&[sse.as_bytes()], Some(None)).await;
        let error = error.expect("fails");
        assert!(out.is_empty());
        assert!(error.direct);
        assert_eq!(text(&error.body), expected);
        // A buffered answer: the same.
        let error = observer().aggregate(sse.as_bytes()).unwrap_err();
        assert_eq!(text(&error.body), expected);
    }
    // An `error` event's own object, and an event without any: the whole event.
    let event = r#"{"type":"error","error":{"code":"server_is_overloaded","message":""}}"#;
    let error = observer()
        .aggregate(format!("data: {event}\n\n").as_bytes())
        .unwrap_err();
    assert_eq!(
        text(&error.body),
        r#"{"error":{"code":"server_is_overloaded","message":""}}"#
    );
    let bare = r#"{"type":"response.failed","response":{"id":"r","status":"failed"}}"#;
    let error = observer()
        .aggregate(format!("data: {bare}\n\n").as_bytes())
        .unwrap_err();
    assert_eq!(text(&error.body), bare);
}

/// The client gets the upstream's own HTTP status; the scheduler keeps the classified one.
#[tokio::test]
async fn upstream_http_status_is_delivered_apart_from_classification() {
    let mock = Mock::start().await;
    for (status, body) in [
        (
            400,
            r#"{"error":{"type":"usage_limit_reached","message":null,"resets_at":1738888888}}"#,
        ),
        (
            503,
            r#"{"error":{"code":"model_at_capacity","message":"Selected model is at capacity.","x":1.50}}"#,
        ),
    ] {
        mock.script("/responses", vec![json(status, body)]);
        let error = executor()
            .execute(
                &credential(Kind::OAuth, &mock.url),
                request(fixture("http-request-full").bytes(), "gpt-test", true, None),
                &Config::default(),
            )
            .await
            .err()
            .expect("an error");
        assert_eq!(error.delivered_status, Some(status));
        assert_eq!(error.status, 429, "classified as a limit for the scheduler");
        assert_eq!(text(&error.body), body);
        assert!(error.direct);
    }
}

/// The two narrow exceptions stay: an empty `response.incomplete` without a client-side
/// reason before any delivery is a 502; with `interrupted` it passes unchanged.
#[tokio::test]
async fn empty_incomplete_keeps_its_narrow_exception() {
    let empty = r#"{"type":"response.incomplete","response":{"id":"r","output":[],"usage":{"output_tokens":0}}}"#;
    let (out, error) = through(&[format!("data: {empty}\n\n").as_bytes()], None).await;
    assert!(out.is_empty());
    assert_eq!(error.unwrap().status, 502);
    let interrupted = r#"{"type":"response.incomplete","response":{"id":"r","incomplete_details":{"reason":"interrupted"},"output":[],"usage":{"output_tokens":0}}}"#;
    let sse = format!("data: {interrupted}\n\n");
    let (out, error) = through(&[sse.as_bytes()], None).await;
    assert!(error.is_none(), "{error:?}");
    assert_eq!(text(&out), sse);
}

/// An API key on api.openai.com is the `api-key` profile. The request is captured as it
/// would be sent, then a dead loopback proxy stops it: nothing leaves the machine.
#[tokio::test]
async fn the_api_openai_com_profile_maps_without_traffic() {
    let mut credential = credential(Kind::Key, "https://api.openai.com/v1");
    credential
        .attributes
        .insert("proxy_url".into(), "http://127.0.0.1:9".into());
    assert_eq!(Profile::of(&View::new(&credential)), Some(Profile::ApiKey));
    let tap = std::sync::Arc::new(crate::codex_testkit::Wiretap::default());
    let body = fixture("http-request-full").bytes();
    let mut req = request(body.clone(), "gpt-test", true, None);
    req.usage = cpa_core::exec::UsageSink::default().with_capture(cpa_core::exec::CaptureSink::new(tap.clone()));
    let error = executor()
        .execute(&credential, req, &Config::default())
        .await
        .err()
        .expect("no route out");
    assert_eq!(error.scope, FailureScope::Transport, "{error:?}");
    let sent = tap.taps().into_iter().find_map(|t| match t {
        crate::codex_testkit::Tap::Request { url, body, headers, .. } => Some((url, body, headers)),
        _ => None,
    });
    let (url, sent, headers) = sent.expect("the request was captured");
    assert_eq!(url, "https://api.openai.com/v1/responses");
    assert_eq!(sent, text(&body));
    assert!(
        headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("authorization") && v == "Bearer key-FAKE")
    );
    for (base, profile) in [
        ("https://API.OPENAI.COM/v1", Profile::ApiKey),
        ("https://relay.example/v1", Profile::Compatible),
    ] {
        assert_eq!(Profile::of(&View::new(&credential_with(base))), Some(profile), "{base}");
    }
}

fn credential_with(base: &str) -> Credential {
    credential(Kind::Key, base)
}

/// A final event the upstream left open gets the line endings that close it, so an SSE
/// client does not drop it; its own bytes are unchanged.
#[tokio::test]
async fn an_open_final_event_is_closed() {
    for (sent, want) in [
        (format!("data: {COMPLETED}"), format!("data: {COMPLETED}\n\n")),
        (format!("data: {COMPLETED}\n"), format!("data: {COMPLETED}\n\n")),
        (format!("data: {COMPLETED}\r"), format!("data: {COMPLETED}\r\r")),
        (
            format!("data: {COMPLETED}\r\n\r\n"),
            format!("data: {COMPLETED}\r\n\r\n"),
        ),
    ] {
        let (out, error) = through(&[sent.as_bytes()], None).await;
        assert!(error.is_none(), "{sent:?}: {error:?}");
        assert_eq!(text(&out), want, "{sent:?}");
    }
}

/// #28: `multimedia.disable-image-generation` keeps an `image_generation` tool from the
/// provider. Go stripped it in its payload step; no Codex route edits a body, so the
/// request is refused before the upstream, on the native HTTP route, a WebSocket turn over
/// HTTP and the translated route. `chat` mode spares the Images API paths, `passthrough`
/// and the default send the tool, and a body without the tool goes through.
#[tokio::test]
async fn a_disabled_image_tool_is_refused_not_stripped() {
    let with_tool = r#"{"model":"gpt-test","stream":true,"input":[],"tools":[{"type":"image_generation"}]}"#;
    let with_choice = r#"{"model":"gpt-test","stream":true,"input":[],"tool_choice":{"type":"image_generation"}}"#;
    let plain = r#"{"model":"gpt-test","stream":true,"input":[]}"#;
    for (config, body, refused) in [
        ("multimedia: {disable-image-generation: true}\n", with_tool, true),
        ("multimedia: {disable-image-generation: true}\n", with_choice, true),
        ("multimedia: {disable-image-generation: chat}\n", with_tool, true),
        (
            "multimedia: {disable-image-generation: passthrough}\n",
            with_tool,
            false,
        ),
        ("{}", with_tool, false),
        ("multimedia: {disable-image-generation: true}\n", plain, false),
    ] {
        let cfg = Config::parse(config).unwrap();
        for kind in [Kind::OAuth, Kind::Key, Kind::Siwc] {
            let mock = Mock::start().await;
            mock.script("/responses", vec![sse("sse-completed")]);
            let mut req = request(body, "gpt-test", true, None);
            req.request_path = "/v1/responses".into();
            let result = executor().execute(&credential(kind, &mock.url), req, &cfg).await;
            let sent = mock.take();
            let name = format!("{config:?} {body}");
            if refused {
                let error = result.err().unwrap_or_else(|| panic!("{name}: refused"));
                assert_eq!((error.status, error.scope), (400, FailureScope::Request), "{name}");
                assert!(
                    text(&error.body).contains("multimedia.disable-image-generation"),
                    "{name}"
                );
                assert!(sent.is_empty(), "{name}: nothing upstream");
            } else {
                let ResponseBody::Stream(mut stream) = result.unwrap_or_else(|e| panic!("{name}: {e}")).body else {
                    panic!("{name}: a stream")
                };
                while stream.next().await.is_some() {}
                let all = [sent, mock.take()].concat();
                assert_eq!(text(&all[0].body), body, "{name}: the client's bytes");
            }
        }
    }
    // A WebSocket turn over HTTP refuses the frame the same way.
    let cfg = Config::parse("multimedia: {disable-image-generation: true}\n").unwrap();
    let mock = Mock::start().await;
    let frame = r#"{"type":"response.create","model":"gpt-test","input":[],"tools":[{"type":"image_generation"}]}"#;
    let mut req = request(frame, "gpt-test", true, None);
    req.request_path = "/v1/responses".into();
    let session = ExecSession {
        id: "conn-image".into(),
        continuation: false,
        lease: None,
    };
    let error = executor()
        .execute_in_session(&credential(Kind::Key, &mock.url), req, &cfg, &session)
        .await
        .err()
        .expect("refused");
    assert_eq!(error.status, 400);
    assert!(mock.take().is_empty(), "nothing upstream");
}

/// #28: a Home credential whose `credential_options` mark the model `is-compat` asked
/// for Go's compatibility rewrites. No Codex route honours the flag, and a Home credential
/// arrives at run time, so the request fails for the credential before anything is sent,
/// on HTTP and on a WebSocket turn. The same credential without the flag is served.
#[tokio::test]
async fn a_home_is_compat_model_is_refused_for_the_credential() {
    for compat in [true, false] {
        let mock = Mock::start().await;
        mock.script("/responses", vec![sse("sse-completed"), sse("sse-completed")]);
        let mut home = credential(Kind::Key, &mock.url);
        home.attributes
            .insert(cpa_core::config::credentials::HOME_PROVIDER.into(), "codex".into());
        home.metadata.insert(
            "credential_options".into(),
            serde_json::json!({"models": [{"name": "gpt-test", "is-compat": compat}]}),
        );
        let body = r#"{"model":"gpt-test","stream":true,"input":[]}"#;
        let http = executor()
            .execute(&home, request(body, "gpt-test", true, None), &Config::default())
            .await;
        let session = ExecSession {
            id: format!("conn-compat-{compat}"),
            continuation: false,
            lease: None,
        };
        let frame = r#"{"type":"response.create","model":"gpt-test","input":[]}"#;
        let ws = executor()
            .execute_in_session(
                &home,
                request(frame, "gpt-test", true, None),
                &Config::default(),
                &session,
            )
            .await;
        for (name, result) in [("http", http), ("websocket", ws)] {
            match result {
                Err(error) if compat => {
                    assert_eq!((error.status, error.scope), (500, FailureScope::Credential), "{name}");
                    assert!(text(&error.body).contains("is-compat"), "{name}");
                }
                Ok(response) if !compat => {
                    if let ResponseBody::Stream(mut stream) = response.body {
                        while stream.next().await.is_some() {}
                    }
                }
                other => panic!("{name} compat={compat}: {:?}", other.err()),
            }
        }
        assert_eq!(mock.take().len(), if compat { 0 } else { 2 }, "compat={compat}");
    }
}
