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

/// Every native event passes through the observers (usage, output-item bookkeeping,
/// failure classification), which read through `cpa_common::rawjson` since the third-party
/// gjson crate was removed. Observation reads a copy: duplicate keys, number spellings the
/// reader formats differently (`1.0`, `2.50`, `1e400`, a 30-digit integer), escapes and
/// unknown members reach the client as sent, streamed and buffered alike. The terminal
/// event has no duplicate key: the native route finds its type with the strict
/// `CodexBody` parser, which refuses one.
#[tokio::test]
async fn observed_events_keep_duplicates_number_spellings_and_escapes() {
    let created = r#"{"type":"response.created","type":"ignored","sequence_number":1.0,"response":{"id":"resp_\u0031","created_at":1e400,"future":{"n":123456789012345678901234567890,"s":"\ud83d\ude00\u00e9"}}}"#;
    let done = r#"{"type":"response.output_item.done","output_index":0.0,"item":{"type":"message","id":"msg_1","id":"msg_dup","content":[{"type":"output_text","text":"h\u0069"}]}}"#;
    let response = r#"{"id":"resp_1","output":[{"type":"message","id":"msg_1","content":[{"type":"output_text","text":"hi"}]}],"usage":{"input_tokens":5,"output_tokens":2.50,"total_tokens":7.0e0},"x":[1E+2,-0.0]}"#;
    let completed = format!(r#"{{"type":"response.completed","sequence_number":3,"response":{response}}}"#);
    let events = format!(
        "event: response.created\ndata: {created}\n\nevent: response.output_item.done\ndata: {done}\n\nevent: response.completed\ndata: {completed}\n\n"
    );
    let mock = Mock::start().await;
    let body = fixture("http-request-full").bytes();
    for kind in [Kind::OAuth, Kind::Key] {
        mock.script("/responses", vec![sse_text(&events)]);
        let streamed = executor()
            .execute(
                &credential(kind, &mock.url),
                request(body.clone(), "gpt-test", true, None),
                &Config::default(),
            )
            .await
            .unwrap();
        let (out, error) = drain(streamed).await;
        assert!(error.is_none(), "{error:?}");
        assert_eq!(text(&out), events, "streamed events");
        mock.take();
    }
    let full = text(&body).replacen(r#""stream":true"#, r#""stream":false"#, 1);
    mock.script("/responses", vec![sse_text(&events)]);
    let buffered_answer = executor()
        .execute(
            &credential(Kind::OAuth, &mock.url),
            request(full, "gpt-test", false, None),
            &Config::default(),
        )
        .await
        .unwrap();
    assert_eq!(text(&buffered(buffered_answer)), response, "buffered terminal object");
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
    }
}

/// `chunks` through the shared framer and the native stream: what the client receives
/// and the error that ended it.
async fn through(chunks: &[&[u8]], hold: Option<Option<Duration>>) -> (Vec<u8>, Option<ExecError>) {
    let (items, error) = through_with(observer(), chunks, hold, false).await;
    (items.concat(), error)
}

/// `chunks` through the native stream with `observer`; with `websocket`, as a turn
/// served over HTTP delivers it (one payload per item).
async fn through_with(
    observer: Observer,
    chunks: &[&[u8]],
    hold: Option<Option<Duration>>,
    websocket: bool,
) -> (Vec<Vec<u8>>, Option<ExecError>) {
    let upstream = futures_util::stream::iter(
        chunks
            .iter()
            .map(|c| Ok::<_, ExecError>(Bytes::copy_from_slice(c)))
            .collect::<Vec<_>>(),
    );
    let mut stream = passthrough(crate::upstream::framed(upstream), observer, hold, websocket);
    if websocket {
        stream = stream.filter_map(|item| async move { unframed(item) }).boxed();
    }
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(bytes) => out.push(bytes.to_vec()),
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
        let object = observer().aggregate(bytes, true).unwrap();
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
        let error = observer().aggregate(sse.as_bytes(), true).unwrap_err();
        assert_eq!(text(&error.body), expected);
    }
    // An `error` event's own object, and an event without any: the whole event.
    let event = r#"{"type":"error","error":{"code":"server_is_overloaded","message":""}}"#;
    let error = observer()
        .aggregate(format!("data: {event}\n\n").as_bytes(), true)
        .unwrap_err();
    assert_eq!(
        text(&error.body),
        r#"{"error":{"code":"server_is_overloaded","message":""}}"#
    );
    let bare = r#"{"type":"response.failed","response":{"id":"r","status":"failed"}}"#;
    let error = observer()
        .aggregate(format!("data: {bare}\n\n").as_bytes(), true)
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

/// The buffered answer for an event whose `response` is its last member: that object,
/// sliced from the event's own bytes.
fn last_response_object(event: &str) -> &str {
    &event[event.find(r#""response":"#).unwrap() + 11..event.len() - 1]
}

/// #25 and #7: every `response.incomplete` is the provider's terminal event, whatever its
/// reason (absent, null, empty, unknown or named), output or usage, and however much of
/// the response was delivered before it. The pinned client reads it as a terminal,
/// non-retryable `ResponseIncomplete`, or `interrupted` as the end of the turn
/// (`codex-api/src/sse/responses.rs:451-506` at 44ccf394573c). It reaches a streaming
/// client byte for byte, with no error after it, whether bootstrap buffering holds the
/// leading events or not; a buffered client gets its `response` object byte for byte.
#[tokio::test]
async fn every_incomplete_passes_unchanged_at_every_stage() {
    let created = format!("event: response.created\ndata: {CREATED}\n\n");
    let output = concat!(
        "event: response.output_text.delta\n",
        r#"data: {"type":"response.output_text.delta","item_id":"msg_inc_1","output_index":0,"content_index":0,"delta":"Hel"}"#,
        "\n\n",
    );
    let ends = fixture("probe-sse-incomplete-ends").frames();
    assert_eq!(ends.len(), 11);
    for end in ends {
        let end = text(&end);
        let terminal = format!("event: response.incomplete\ndata: {end}\n\n");
        for (stage, sse) in [
            ("first event", terminal.clone()),
            ("after response.created", format!("{created}{terminal}")),
            ("after output", format!("{created}{output}{terminal}")),
        ] {
            for hold in [None, Some(None), Some(Some(Duration::from_secs(30)))] {
                let (out, error) = through(&[sse.as_bytes()], hold).await;
                assert!(error.is_none(), "{stage} hold={hold:?} {end}: {error:?}");
                assert_eq!(text(&out), sse, "{stage} hold={hold:?}");
            }
            let object = observer().aggregate(sse.as_bytes(), true).unwrap();
            assert_eq!(text(&object), last_response_object(&end), "{stage}: buffered");
        }
    }
}

/// Upstream events are read as the provider sent them, not through the client request
/// boundary: a terminal event with a duplicate key or a top-level member whose type a
/// request could not have still ends the response, and a failure event with a duplicate
/// key still delivers its error object. An event that is not valid JSON has no type (the
/// pinned client skips it, `sse/responses.rs:633-643`), so it never ends the response;
/// it passes as sent like any other event.
#[tokio::test]
async fn upstream_events_skip_the_request_boundary() {
    let completed =
        r#"{"type":"response.completed","model":1,"stream":"x","response":{"id":"r","output":[],"dup":1,"dup":2}}"#;
    let broken = r#"{"type":"response.completed","response":{"id":"r""#;
    for sse in [
        format!("data: {completed}\n\n"),
        format!("data: {CREATED}\n\ndata: {broken}\n\ndata: {completed}\n\n"),
    ] {
        for hold in [None, Some(None)] {
            let (out, error) = through(&[sse.as_bytes()], hold).await;
            assert!(error.is_none(), "hold={hold:?}: {error:?}");
            assert_eq!(text(&out), sse);
        }
        let object = observer().aggregate(sse.as_bytes(), true).unwrap();
        assert_eq!(text(&object), last_response_object(completed));
    }
    let failed = r#"{"type":"response.failed","sequence_number":7,"response":{"id":"r","dup":1,"dup":2,"error":{"code":"server_error","message":"m","x":1.50}}}"#;
    let error = observer()
        .aggregate(format!("data: {failed}\n\n").as_bytes(), true)
        .unwrap_err();
    assert_eq!(
        text(&error.body),
        r#"{"error":{"code":"server_error","message":"m","x":1.50},"sequence_number":7}"#
    );
}

/// A stream that ends without a terminal event is still a truncation: an
/// `incomplete` event is never invented for it, and the attempt ends with the
/// request-scoped 408 after the events that arrived (`docs/codex/CONTRACT.md` "Native
/// HTTP route").
#[tokio::test]
async fn a_stream_without_its_terminal_event_stays_truncated() {
    let sse = format!("data: {CREATED}\n\n");
    for hold in [None, Some(None)] {
        let (out, error) = through(&[sse.as_bytes()], hold).await;
        let error = error.expect("a truncated stream fails");
        assert_eq!(
            (error.status, error.scope),
            (408, FailureScope::Request),
            "hold={hold:?}"
        );
        if hold.is_none() {
            assert_eq!(text(&out), sse, "what arrived, unchanged");
        }
    }
    let error = observer().aggregate(sse.as_bytes(), true).unwrap_err();
    assert_eq!((error.status, error.scope), (408, FailureScope::Request));
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

#[tokio::test]
async fn missing_mime_classifies_framing_and_preserves_original_chunks() {
    for (body, want) in [
        ("data: {\"type\":\"response.created\"}\n\n", true),
        ("\r\nevent: response.created\ndata: {}\n\n", true),
        ("\u{feff}data: {}\n\n", true),
        (": comment\n\ndata: {}\n\n", true),
        ("{\"object\":\"response\"}", false),
        ("", false),
        ("dat", false),
    ] {
        // One-byte chunks exercise every boundary, including BOM and field names.
        let chunks: Vec<_> = body.as_bytes().chunks(1).map(Bytes::copy_from_slice).collect();
        let upstream = crate::proxy::Upstream {
            status: 200,
            headers: HeaderMap::new(),
            body: futures_util::stream::iter(chunks.clone().into_iter().map(Ok)).boxed(),
        };
        let (sse, mut upstream) = classify_response(upstream, true, &Wire::default()).await.unwrap();
        assert_eq!(sse, want, "{body:?}");
        let mut replay = Vec::new();
        while let Some(chunk) = upstream.body.next().await {
            replay.push(chunk.unwrap());
        }
        assert_eq!(replay, chunks, "original chunk boundaries and bytes survive");
    }
}

#[tokio::test]
async fn missing_mime_does_not_buffer_a_whole_stream_or_override_explicit_json() {
    let first = Bytes::from_static(b"data: {}\n\n");
    let upstream = crate::proxy::Upstream {
        status: 200,
        headers: HeaderMap::new(),
        body: futures_util::stream::once(futures_util::future::ready(Ok(first.clone())))
            .chain(futures_util::stream::pending())
            .boxed(),
    };
    let (sse, mut replay) = tokio::time::timeout(
        Duration::from_secs(1),
        classify_response(upstream, true, &Wire::default()),
    )
    .await
    .expect("framing detection must not wait for EOF")
    .unwrap();
    assert!(sse);
    assert_eq!(replay.body.next().await.unwrap().unwrap(), first);
    for (mime, expected, want) in [
        (Some("application/json"), true, false),
        (None, false, false),
        (Some("Text/Event-Stream; charset=utf-8"), true, true),
    ] {
        let mut headers = HeaderMap::new();
        if let Some(mime) = mime {
            headers.insert(http::header::CONTENT_TYPE, mime.parse().unwrap());
        }
        let upstream = crate::proxy::Upstream {
            status: 200,
            headers,
            body: futures_util::stream::pending().boxed(),
        };
        let (sse, _) = tokio::time::timeout(
            Duration::from_secs(1),
            classify_response(upstream, expected, &Wire::default()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(sse, want);
    }
}

#[tokio::test]
async fn missing_mime_preserves_body_errors_and_bounds_inspection() {
    let upstream = crate::proxy::Upstream {
        status: 200,
        headers: HeaderMap::new(),
        body: futures_util::stream::iter(vec![
            Ok(Bytes::from_static(b"da")),
            Err(ExecError::local(502, FailureScope::Transport, "fixture read error")),
        ])
        .boxed(),
    };
    let Err(error) = classify_response(upstream, true, &Wire::default()).await else {
        panic!("read error must survive");
    };
    assert_eq!(error.scope, FailureScope::Transport);
    let upstream = crate::proxy::Upstream {
        status: 200,
        headers: HeaderMap::new(),
        body: futures_util::stream::once(futures_util::future::ready(Ok(Bytes::from(vec![b'\n'; 65536]))))
            .chain(futures_util::stream::pending())
            .boxed(),
    };
    let (sse, _) = tokio::time::timeout(
        Duration::from_secs(1),
        classify_response(upstream, true, &Wire::default()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!sse);
}

#[tokio::test]
async fn missing_mime_keeps_native_http_streaming_and_chatgpt_buffered_aggregation() {
    let mock = Mock::start().await;
    for stream in [true, false] {
        let mut reply = sse("sse-completed");
        reply.headers.clear();
        mock.script("/responses", vec![reply]);
        let body = text(&fixture("http-request-full").bytes());
        let body = if stream {
            body
        } else {
            body.replace("\"stream\":true", "\"stream\":false")
        };
        let response = executor()
            .execute(
                &credential(Kind::OAuth, &mock.url),
                request(body, "gpt-test", stream, None),
                &Config::default(),
            )
            .await
            .unwrap();
        if stream {
            let (bytes, error) = drain(response).await;
            assert!(error.is_none());
            assert_eq!(bytes, fixture("sse-completed").bytes());
        } else {
            let terminal = text(&fixture("sse-completed").frames().pop().unwrap());
            let object = &terminal[terminal.find(r#""response":"#).unwrap() + 11..terminal.len() - 1];
            assert_eq!(text(&buffered(response)), object);
        }
    }
}

#[tokio::test]
async fn initial_bom_is_framing_for_http_delivery_and_aggregation() {
    let mock = Mock::start().await;
    let terminal = fixture("sse-completed").frames().pop().unwrap();
    let terminal = text(&terminal);
    let object = &terminal[terminal.find(r#""response":"#).unwrap() + 11..terminal.len() - 1];
    for mime in [None, Some("text/event-stream")] {
        for event_line in ["", "event: response.completed\n"] {
            let wire = format!("\u{feff}{event_line}data: {terminal}\n\n");
            for stream in [true, false] {
                mock.script(
                    "/responses",
                    vec![Reply {
                        status: 200,
                        headers: mime
                            .map(|mime| vec![("content-type".into(), mime.into())])
                            .unwrap_or_default(),
                        body: wire.clone(),
                    }],
                );
                let response = executor()
                    .execute(
                        &credential(Kind::OAuth, &mock.url),
                        request(fixture("http-request-full").bytes(), "gpt-test", stream, None),
                        &Config::default(),
                    )
                    .await
                    .unwrap();
                if stream {
                    let (bytes, error) = drain(response).await;
                    assert!(error.is_none(), "MIME={mime:?} event_line={event_line:?}: {error:?}");
                    assert_eq!(bytes, wire.as_bytes(), "the BOM and wire bytes pass through unchanged");
                } else {
                    assert_eq!(text(&buffered(response)), object);
                }
            }
        }
    }
}

/// The failures a native stream settled in the attempt's record when its consumer
/// dropped it (#54), as (status, text).
#[derive(Default)]
struct Settled(std::sync::Mutex<Vec<(u16, String)>>);

impl cpa_core::exec::UsageObserver for Settled {
    fn response_body(&self, _: Format, _: &[u8]) {}
    fn response_line(&self, _: Format, _: &[u8]) {}
    fn request(&self, _: Format, _: &[u8]) {}
    fn publish_failure(&self, status: u16, body: &str) {
        self.0.lock().unwrap().push((status, body.to_owned()));
    }
    fn publish_failure_with_usage(&self, status: u16, body: &str) {
        self.0.lock().unwrap().push((status, format!("with usage: {body}")));
    }
    fn publish(&self) {
        self.0.lock().unwrap().push((0, "publish".into()));
    }
}

/// A native stream over `events` whose upstream then stays open, reporting to the
/// returned record.
fn open_stream(events: &[&str]) -> (ExecStream, std::sync::Arc<Settled>) {
    let body: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
    let upstream = futures_util::stream::iter([Ok::<_, ExecError>(Bytes::from(body))])
        .chain(futures_util::stream::pending())
        .boxed();
    let settled = std::sync::Arc::new(Settled::default());
    let observer = Observer {
        usage: cpa_core::exec::UsageSink::new(settled.clone()),
        ..observer()
    };
    (
        passthrough(crate::upstream::framed(upstream), observer, None, false),
        settled,
    )
}

fn settled(record: &Settled) -> Vec<(u16, String)> {
    record.0.lock().unwrap().clone()
}

const DELTA: &str =
    r#"{"type":"response.output_text.delta","item_id":"m","output_index":0,"content_index":0,"delta":"hi"}"#;
const FAILED: &str = r#"{"type":"response.failed","response":{"id":"r","status":"failed","error":{"code":"server_error","message":"boom"}}}"#;

/// A consumer that leaves before the upstream ended the response cancels it, once,
/// whether it left while the stream waited for the upstream or between two polls (a
/// client that went away while the server wrote an earlier event).
#[tokio::test]
async fn a_dropped_unfinished_stream_records_one_cancellation() {
    use futures_util::FutureExt;
    for awaiting in [true, false] {
        let (mut stream, record) = open_stream(&[CREATED, DELTA]);
        assert!(stream.next().await.unwrap().is_ok());
        assert!(stream.next().await.unwrap().is_ok());
        if awaiting {
            assert!(stream.next().now_or_never().is_none(), "the upstream is still open");
        }
        assert!(settled(&record).is_empty());
        drop(stream);
        assert_eq!(
            settled(&record),
            [(499, "context canceled".to_owned())],
            "awaiting={awaiting}"
        );
    }
}

/// The upstream completed: a consumer that takes the terminal event and leaves before the
/// stream's end leaves the record to the tracker, which has the observed usage.
#[tokio::test]
async fn a_completed_stream_dropped_before_its_end_records_no_cancellation() {
    let (mut stream, record) = open_stream(&[CREATED, COMPLETED]);
    assert!(stream.next().await.unwrap().is_ok());
    let terminal = stream.next().await.unwrap().unwrap();
    assert!(text(&terminal).contains("response.completed"));
    drop(stream);
    assert!(settled(&record).is_empty(), "{:?}", settled(&record));
}

/// An upstream failure queued behind its delivered event is recorded as that failure if
/// the consumer leaves before taking it, and left to the consumer once taken.
#[tokio::test]
async fn an_upstream_failure_is_never_recorded_as_a_cancellation() {
    let events = [CREATED, DELTA, FAILED];
    let (mut stream, record) = open_stream(&events);
    let mut error = None;
    while let Some(item) = stream.next().await {
        if let Err(e) = item {
            error = Some(e);
            break;
        }
    }
    let error = error.expect("the stream ends with the upstream failure");
    drop(stream);
    assert!(settled(&record).is_empty(), "the consumer settles what it took");

    let (mut stream, record) = open_stream(&events);
    loop {
        let event = stream.next().await.unwrap().unwrap();
        if text(&event).contains("response.failed") {
            break;
        }
    }
    drop(stream);
    let expected = (error.status, String::from_utf8_lossy(&error.body).into_owned());
    assert_ne!(expected.0, 499);
    assert_eq!(settled(&record), [expected]);
}

/// Upstream events the pinned client does not decode (`codex_event::decode`): repeated
/// known members in either order, a known member of the wrong type, and invalid JSON
/// that reads as a failure, a context-window `error`, an account failure or a
/// completion carrying usage.
const UNDECODABLE: [&str; 10] = [
    r#"{"type":"response.completed","type":"response.future","response":{"id":"r1","output":[]}}"#,
    r#"{"type":"response.future","type":"response.completed","response":{"id":"r1","output":[]}}"#,
    r#"{"type":"response.completed","response":{"id":"old","output":[]},"response":{"id":"new","output":[]}}"#,
    r#"{"type":"response.failed","type":"x","response":{"error":{"code":"server_error","message":"m"}}}"#,
    r#"{"type":"error","error":{"code":"context_length_exceeded"},"error":{"code":"context_length_exceeded"}}"#,
    r#"{"type":"response.completed","item_id":7,"response":{"id":"r1","output":[]}}"#,
    r#"{"type":"response.failed","response":{"error":{"code":"server_error","message":"broken"}}"#,
    r#"{"type":"error","error":{"code":"context_length_exceeded","message":"broken"}"#,
    r#"{"type":"response.failed","response":{"error":{"code":"rate_limit_exceeded","message":"acct"}}"#,
    r#"{"type":"response.completed","response":{"id":"r1","output":[],"usage":{"input_tokens":90,"output_tokens":9,"total_tokens":99}}"#,
];

/// #25 (F1, F3): an undecodable event passes as sent and the stream goes on to the real
/// terminal event, on a held and an unheld stream, for a Codex client (which would get
/// #5's conversion) and any other. A buffered answer is the real terminal's `response`.
/// Usage reads only the decoded events. On a turn served over HTTP, each reaches the
/// client as a frame, except invalid JSON that reads as an account failure, which is
/// withheld and still not classified.
#[tokio::test]
async fn undecodable_events_never_end_fail_or_convert_the_response() {
    let end = r#"{"type":"response.completed","response":{"id":"r2","output":[]}}"#;
    let account = UNDECODABLE[8];
    for event in UNDECODABLE {
        let sse = format!("data: {event}\n\nevent: response.completed\ndata: {end}\n\n");
        for codex_client in [false, true] {
            for hold in [None, Some(None)] {
                let recorder = std::sync::Arc::new(crate::usage_testkit::Recorder::default());
                let recorded = Observer {
                    codex_client,
                    usage: cpa_core::exec::UsageSink::new(recorder.clone()),
                    ..observer()
                };
                let (out, error) = through_with(recorded, &[sse.as_bytes()], hold, false).await;
                assert!(error.is_none(), "{event}: {error:?}");
                assert_eq!(text(&out.concat()), sse, "{event}");
                let lines: Vec<String> = recorder
                    .0
                    .lock()
                    .unwrap()
                    .iter()
                    .filter_map(|r| match r {
                        crate::usage_testkit::Report::Line(_, line) => Some(String::from_utf8_lossy(line).into_owned()),
                        _ => None,
                    })
                    .collect();
                assert_eq!(lines, [end], "{event}: usage reads only the decoded event");
                let client = Observer {
                    codex_client,
                    ..observer()
                };
                let (frames, error) = through_with(client, &[sse.as_bytes()], hold, true).await;
                assert!(error.is_none(), "{event}: {error:?}");
                let frames: Vec<String> = frames.iter().map(|f| text(f)).collect();
                let want: &[&str] = if event == account { &[end] } else { &[event, end] };
                assert_eq!(frames, want, "{event}: served over HTTP");
            }
        }
        let answer = observer().aggregate(sse.as_bytes(), true).unwrap();
        assert_eq!(text(&answer), r#"{"id":"r2","output":[]}"#, "{event}");
    }
}

/// Decodable events with repeats the pinned client ignores still end the response:
/// unknown members, and keys repeated inside `response`, whose object a buffered client
/// gets as sent. A failure whose `response` repeats `error` is delivered whole rather
/// than one of them picked.
#[tokio::test]
async fn harmless_repeats_keep_the_decoded_outcome() {
    let end = r#"{"type":"response.completed","x":1,"x":[2],"response":{"id":"a","id":"b","output":[],"output":null}}"#;
    let sse = format!("data: {end}\n\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"late\"}}\n\n");
    let (out, error) = through(&[sse.as_bytes()], None).await;
    assert!(error.is_none(), "{error:?}");
    assert_eq!(text(&out), format!("data: {end}\n\n"));
    let answer = observer().aggregate(sse.as_bytes(), true).unwrap();
    assert_eq!(text(&answer), r#"{"id":"a","id":"b","output":[],"output":null}"#);
    let failed = r#"{"type":"response.failed","sequence_number":3,"response":{"id":"r","error":{"code":"a"},"error":{"code":"b"}}}"#;
    let error = observer()
        .aggregate(format!("data: {failed}\n\n").as_bytes(), true)
        .unwrap_err();
    assert_eq!(text(&error.body), failed);
    let single =
        r#"{"type":"response.failed","x":1,"x":2,"response":{"id":"r","id":"s","error":{"code":"a","code":"b"}}}"#;
    let error = observer()
        .aggregate(format!("data: {single}\n\n").as_bytes(), true)
        .unwrap_err();
    assert_eq!(text(&error.body), r#"{"error":{"code":"a","code":"b"}}"#);
}

/// #25 (read-on), how the body ended: a buffered answer ends on a processing error only
/// at a `clean` end (`sse/responses.rs:616-621`). After a read failure the same events
/// are a stream without a terminal event, which the pinned reader returns as the read
/// error at once (lines 602-614): the request-scoped 408. A terminal event or a failure
/// before the read failure still decides.
#[test]
fn a_processing_error_ends_a_buffered_answer_only_at_a_clean_end() {
    let recorded = r#"{"type":"response.incomplete","response":{"id":"r","status":"incomplete","incomplete_details":{"reason":"content_filter"},"output":[],"usage":{"input_tokens":9,"output_tokens":9,"total_tokens":18}}}"#;
    let completed = r#"{"type":"response.completed","response":{"id":"r2","output":[]}}"#;
    let failed = r#"{"type":"response.failed","response":{"id":"r","error":{"code":"server_error","message":"m"}}}"#;
    let one = format!("data: {recorded}\n\n");
    let object = observer().aggregate(one.as_bytes(), true).unwrap();
    assert_eq!(text(&object), last_response_object(recorded));
    for sse in [one.clone(), format!("data: {CREATED}\n\n{one}"), format!("{one}{one}")] {
        let error = observer().aggregate(sse.as_bytes(), false).unwrap_err();
        assert_eq!((error.status, error.scope), (408, FailureScope::Request), "{sse}");
    }
    for clean in [true, false] {
        let later = format!("{one}data: {completed}\n\n");
        let object = observer().aggregate(later.as_bytes(), clean).unwrap();
        assert_eq!(text(&object), last_response_object(completed), "clean={clean}");
        let error = observer()
            .aggregate(format!("{one}data: {failed}\n\n").as_bytes(), clean)
            .unwrap_err();
        assert!(error.direct, "clean={clean}: {error:?}");
    }
}
