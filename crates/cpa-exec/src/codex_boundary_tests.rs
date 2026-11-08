//! The Codex request boundary at the executor's entry points, with the #21 contract
//! fixtures. These tests check what the boundary refuses and that a refused body never
//! reaches the upstream. They do not assert the forwarded bytes of an accepted body:
//! the native request rewrites stay until #23 and #24 migrate their routes.

use std::path::Path;

use cpa_common::codex_body::{CodexBody, Kind};
use cpa_core::exec::{Caller, ExecSession};

use super::*;
use crate::codex_contract::{Direction, Format as FixtureFormat, fixture, manifest};
use crate::codex_testkit::{Mock, Reply};

fn executor() -> CodexExecutor {
    CodexExecutor::with_client(wreq::Client::new(), CodexOAuth::new(wreq::Client::new()))
}

fn credential(base_url: &str) -> Credential {
    let mut c = Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/codex-boundary.json"),
        serde_json::json!({"type":"codex","access_token":"at-FAKE","account_id":"acct-FAKE"})
            .as_object()
            .unwrap()
            .clone(),
    )
    .unwrap();
    c.attributes.insert("base_url".into(), base_url.into());
    c
}

fn request(source: Format, body: impl Into<Bytes>, stream: bool, alt: Option<&str>) -> ExecRequest {
    let body = body.into();
    ExecRequest {
        operation: Operation::Generate,
        source_format: source,
        response_format: source,
        requested_model: "gpt-test".into(),
        model: "gpt-test".into(),
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

fn sse_reply() -> Reply {
    Reply {
        status: 200,
        headers: vec![("content-type".into(), "text/event-stream".into())],
        body: String::from_utf8(fixture("sse-completed").bytes().to_vec()).unwrap(),
    }
}

/// Every client-to-upstream JSON fixture passes, its routing members point into its own
/// bytes, and reading it changes nothing.
#[test]
fn contract_request_fixtures_pass_the_boundary() {
    let mut checked = 0;
    for f in manifest().fixtures {
        if f.direction != Direction::ToUpstream || f.format != FixtureFormat::Json {
            continue;
        }
        let bytes = f.bytes();
        let before = bytes.clone();
        let body = CodexBody::parse(&bytes).unwrap_or_else(|e| panic!("{}: {e}", f.name));
        if let Some(model) = body.get("model") {
            assert_eq!(model.kind, Kind::String, "{}", f.name);
            assert!(bytes.as_ptr_range().contains(&model.raw.as_ptr()), "{}", f.name);
        }
        assert_eq!(bytes, before, "{}", f.name);
        checked += 1;
    }
    assert!(checked >= 10, "only {checked} request fixtures");

    // The probe's unusual numbers come back with their spelling.
    let probe = fixture("probe-request-unknown-fields").bytes();
    let body = CodexBody::parse(&probe).unwrap();
    assert_eq!(body.get("service_tier").unwrap().raw, br#""flex""#);
    assert_eq!(body.get("future_top_level_field").unwrap().kind, Kind::Null);
}

/// Bodies the boundary refuses, with the status the client gets.
fn refused() -> Vec<(&'static str, Vec<u8>, u16)> {
    let full = fixture("http-request-full").bytes();
    let mut trailing = full.to_vec();
    trailing.extend_from_slice(b" {}");
    let mut duplicate = full.to_vec();
    // A second `model` member at the end of the fixture's root object.
    duplicate.truncate(duplicate.len() - 1);
    duplicate.extend_from_slice(br#","model":"other"}"#);
    let deep = format!(r#"{{"model":"m","x":{}{}}}"#, "[".repeat(200), "]".repeat(200)).into_bytes();
    vec![
        ("trailing content", trailing, 400),
        ("duplicate key", duplicate, 400),
        (
            "invalid UTF-8",
            b"{\"model\":\"gpt-test\",\"input\":\"\xc3\"}".to_vec(),
            400,
        ),
        ("model type", br#"{"model":5,"input":[]}"#.to_vec(), 400),
        ("stream type", br#"{"model":"m","stream":"yes"}"#.to_vec(), 400),
        ("depth", deep, 400),
        ("root type", br#"[{"model":"m"}]"#.to_vec(), 400),
    ]
}

#[tokio::test]
async fn refused_bodies_never_reach_the_upstream() {
    let mock = Mock::start().await;
    let executor = executor();
    let cfg = Config::default();
    let credential = credential(&mock.url);
    for source in [Format::OpenAIResponse, Format::Codex] {
        for (name, body, status) in refused() {
            for (stream, alt) in [(true, None), (false, None), (false, Some("responses/compact"))] {
                let error = executor
                    .execute(&credential, request(source, body.clone(), stream, alt), &cfg)
                    .await
                    .err()
                    .unwrap_or_else(|| panic!("{name}: accepted"));
                assert_eq!((error.status, error.scope), (status, FailureScope::Request), "{name}");
                assert!(
                    String::from_utf8_lossy(&error.body).starts_with("request "),
                    "{name}: {:?}",
                    error.body
                );
            }
            let session = ExecSession {
                id: "session-FAKE".into(),
                continuation: false,
                lease: None,
            };
            let error = executor
                .execute_in_session(&credential, request(source, body.clone(), true, None), &cfg, &session)
                .await
                .err()
                .unwrap_or_else(|| panic!("{name}: accepted in a session"));
            assert_eq!(error.status, status, "{name}");
        }
    }
    assert!(mock.take().is_empty(), "a refused body reached the upstream");
}

/// The boundary changes nothing for a valid body: it still reaches the upstream.
#[tokio::test]
async fn valid_fixture_bodies_still_reach_the_upstream() {
    let mock = Mock::start().await;
    let executor = executor();
    let cfg = Config::default();
    let credential = credential(&mock.url);
    for name in [
        "http-request-full",
        "probe-request-unknown-fields",
        "http-request-compaction",
    ] {
        mock.script("/responses", vec![sse_reply()]);
        let body = fixture(name).bytes();
        let response = executor
            .execute(&credential, request(Format::OpenAIResponse, body, true, None), &cfg)
            .await
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(response.status, 200, "{name}");
        drop(response);
        assert_eq!(mock.take().len(), 1, "{name}");
    }
}

/// Bodies from other client formats reach Codex through a cross-API translator and keep
/// that translator's parsing until #27, so the boundary does not judge them.
#[tokio::test]
async fn translated_formats_are_outside_the_boundary() {
    let mock = Mock::start().await;
    mock.script("/responses", vec![sse_reply()]);
    let body = br#"{"model":"gpt-test","model":"gpt-test","messages":[{"role":"user","content":"hi"}]}"#;
    let response = executor()
        .execute(
            &credential(&mock.url),
            request(Format::OpenAI, Bytes::from_static(body), true, None),
            &Config::default(),
        )
        .await
        .expect("a Chat Completions body is not refused by the Codex boundary");
    assert_eq!(response.status, 200);
    drop(response);
    assert_eq!(mock.take().len(), 1);
}

/// A client's Images API request on `path`, as the images handler hands it over.
fn images_request(path: &str, model: &str, content_type: &str, body: impl Into<Bytes>) -> ExecRequest {
    let mut req = request(Format::OpenAI, body, false, None);
    req.requested_model = model.into();
    req.model = model.into();
    req.request_path = path.into();
    req.headers
        .insert(http::header::CONTENT_TYPE, content_type.parse().unwrap());
    req
}

fn form(fields: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, value) in fields {
        out.extend_from_slice(format!("--B\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes());
        out.extend_from_slice(value);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"--B--\r\n");
    out
}

/// The Images adapter reads a client's JSON through the same boundary (#22): malformed,
/// non-UTF-8, repeated-key, too-deep and non-object bodies, and members of the wrong
/// type, are refused before anything is sent, on the tool and the direct path. Nothing is
/// coerced: Go reads `"n":"1"` and `"size":1024` as a count and a size.
#[tokio::test]
async fn images_json_passes_the_boundary_before_the_send() {
    let mock = Mock::start().await;
    let executor = executor();
    let cfg = Config::default();
    let credential = credential(&mock.url);
    let deep = format!(r#"{{"prompt":"p","x":{}{}}}"#, "[".repeat(200), "]".repeat(200));
    let both: Vec<(&str, Vec<u8>)> = vec![
        ("invalid UTF-8", b"{\"prompt\":\"\xff\"}".to_vec()),
        ("repeated key", br#"{"prompt":"a","prompt":"b"}"#.to_vec()),
        (
            "nested repeated key",
            br#"{"prompt":"a","images":[{"image_url":"u","image_url":"v"}]}"#.to_vec(),
        ),
        ("trailing content", br#"{"prompt":"p"} {}"#.to_vec()),
        ("lone surrogate", br#"{"prompt":"\ud800"}"#.to_vec()),
        ("depth", deep.into_bytes()),
        ("root type", br#"[{"prompt":"p"}]"#.to_vec()),
        ("model type", br#"{"model":5,"prompt":"p"}"#.to_vec()),
        ("stream type", br#"{"prompt":"p","stream":"yes"}"#.to_vec()),
    ];
    let tool_only: Vec<(&str, Vec<u8>)> = vec![
        ("prompt type", br#"{"prompt":5}"#.to_vec()),
        ("size type", br#"{"prompt":"p","size":1024}"#.to_vec()),
        ("n as text", br#"{"prompt":"p","n":"1"}"#.to_vec()),
        ("n as a flag", br#"{"prompt":"p","n":true}"#.to_vec()),
        ("count as text", br#"{"prompt":"p","output_compression":"50"}"#.to_vec()),
        (
            "image reference type",
            br#"{"prompt":"p","images":[{"image_url":5}]}"#.to_vec(),
        ),
        (
            "mask reference type",
            br#"{"prompt":"p","mask":{"file_id":["f"]}}"#.to_vec(),
        ),
    ];
    let mut cases = Vec::new();
    for (name, body) in &both {
        for (path, model) in [
            ("/v1/images/generations", "gpt-5.4"),
            ("/v1/images/edits", "gpt-5.4"),
            ("/v1/images/generations", "gpt-image-2"),
            ("/v1/images/edits", "gpt-image-2"),
        ] {
            cases.push((*name, path, model, body.clone()));
        }
    }
    for (name, body) in &tool_only {
        cases.push((*name, "/v1/images/edits", "gpt-5.4", body.clone()));
    }
    for (name, path, model, body) in cases {
        let req = images_request(path, model, "application/json", body);
        let error = executor
            .images(&credential, req, path, &cfg)
            .await
            .err()
            .unwrap_or_else(|| panic!("{name} {path} {model}: accepted"));
        assert_eq!(
            (error.status, error.scope),
            (400, FailureScope::Request),
            "{name} {path} {model}"
        );
        let message = String::from_utf8_lossy(&error.body);
        assert!(message.starts_with("request "), "{name} {path} {model}: {message}");
    }
    assert!(mock.take().is_empty(), "a refused Images body reached the upstream");
}

/// A valid Images body is read without loss: the tool copies count fields with their
/// number spelling and reads escaped text, and the direct path forwards the client's own
/// bytes, unknown members and number spellings included, when nothing needs routing.
#[tokio::test]
async fn images_json_is_read_without_loss() {
    let mock = Mock::start().await;
    let executor = executor();
    let cfg = Config::default();
    let credential = credential(&mock.url);
    mock.script("/responses", vec![sse_reply()]);
    let tool = br#"{"prompt":"\u0061 cat","n":1.0,"output_compression":50.0,"partial_images":2,"size":null}"#;
    let path = "/v1/images/generations";
    // The fixture's completion has no image output: the adapter answers 502 after the send.
    let _ = executor
        .images(
            &credential,
            images_request(path, "gpt-5.4", "application/json", &tool[..]),
            path,
            &cfg,
        )
        .await;
    let sent = mock.take();
    assert_eq!(sent.len(), 1);
    let body = std::str::from_utf8(&sent[0].body).unwrap();
    assert!(body.contains(r#""output_compression":50.0"#), "{body}");
    assert!(body.contains(r#""partial_images":2"#), "{body}");
    assert!(body.contains(r#""text":"a cat""#), "{body}");
    assert!(!body.contains("size"), "{body}");

    mock.script(
        "/images/generations",
        vec![crate::codex_testkit::json(200, r#"{"created":1,"data":[]}"#)],
    );
    let direct = br#"{"model":"gpt-image-2","prompt":"x","future":{"n":1e400,"big":123456789012345678901234567890,"s":"\u00e9"}}"#;
    executor
        .images(
            &credential,
            images_request(path, "gpt-image-2", "application/json", &direct[..]),
            path,
            &cfg,
        )
        .await
        .unwrap();
    let sent = mock.take();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].path, "/images/generations");
    assert_eq!(&sent[0].body[..], &direct[..]);
}

/// A multipart edit's text fields are read as UTF-8 and once: a value that is not UTF-8
/// is refused rather than replaced, and the tool refuses a repeated field it would read
/// only once.
#[tokio::test]
async fn images_forms_are_read_without_loss() {
    let mock = Mock::start().await;
    let executor = executor();
    let cfg = Config::default();
    let credential = credential(&mock.url);
    let path = "/v1/images/edits";
    let content_type = "multipart/form-data; boundary=B";
    for (name, model, fields) in [
        ("invalid UTF-8", "gpt-5.4", vec![("prompt", &b"a \xff"[..])]),
        (
            "repeated field",
            "gpt-5.4",
            vec![("prompt", &b"a"[..]), ("prompt", &b"b"[..])],
        ),
        (
            "direct invalid UTF-8",
            "gpt-image-2",
            vec![("prompt", &b"a"[..]), ("quality", &b"\xc3"[..])],
        ),
    ] {
        let req = images_request(path, model, content_type, form(&fields));
        let error = executor
            .images(&credential, req, path, &cfg)
            .await
            .err()
            .unwrap_or_else(|| panic!("{name}: accepted"));
        assert_eq!((error.status, error.scope), (400, FailureScope::Request), "{name}");
        assert!(
            String::from_utf8_lossy(&error.body).starts_with("request form field `"),
            "{name}: {:?}",
            error.body
        );
    }
    assert!(mock.take().is_empty(), "a refused form reached the upstream");
}

/// Form text fields, in order: name and raw value.
type Fields<'a> = Vec<(&'a str, &'a [u8])>;

/// A present form control is read or refused, never dropped (#22). On both paths the
/// routing fields `model` and `stream` are read once, as UTF-8, and `stream` as a
/// boolean, though the routed values replace them. The tool path refuses a count that is
/// not an integer and an `n` that is not exactly one; Go omits the count, so the provider
/// default applies, and rounds `n` through a float.
#[tokio::test]
async fn images_form_controls_are_refused_not_dropped() {
    let mock = Mock::start().await;
    let executor = executor();
    let cfg = Config::default();
    let credential = credential(&mock.url);
    let path = "/v1/images/edits";
    let content_type = "multipart/form-data; boundary=B";
    let prompt: (&str, &[u8]) = ("prompt", b"p");
    let mut cases: Vec<(&str, Fields, &str)> = Vec::new();
    for model in ["gpt-5.4", "gpt-image-2"] {
        for (fields, message) in [
            (
                vec![("stream", &b"\xff"[..])],
                "request form field `stream` is not valid UTF-8 at byte 0",
            ),
            (
                vec![("stream", &b"nope"[..])],
                "request form field `stream` must be a boolean",
            ),
            (
                vec![("stream", &b"false"[..]), ("stream", &b"true"[..])],
                "request form field `stream` is repeated",
            ),
            (
                vec![("model", &b"gpt-image-2"[..]), ("model", &b"gpt-image-1.5"[..])],
                "request form field `model` is repeated",
            ),
            (
                vec![("model", &b"\xff"[..])],
                "request form field `model` is not valid UTF-8 at byte 0",
            ),
        ] {
            cases.push((model, fields, message));
        }
    }
    for (field, text) in [
        ("partial_images", &b"nope"[..]),
        ("partial_images", b"1.5"),
        ("partial_images", b"9223372036854775808"),
        ("partial_images", b""),
        ("output_compression", b"nope"),
        ("output_compression", b"-9223372036854775809"),
    ] {
        let message = if field == "partial_images" {
            "request form field `partial_images` must be an integer"
        } else {
            "request form field `output_compression` must be an integer"
        };
        cases.push(("gpt-5.4", vec![(field, text)], message));
    }
    for n in [&b"1.0000000000000001"[..], b"2", b"nope", b""] {
        cases.push((
            "gpt-5.4",
            vec![("n", n)],
            "unsupported conversion: `n` cannot be expressed by the Codex image generation tool; remove it or send the request to a provider that accepts it",
        ));
    }
    for (model, mut fields, message) in cases {
        fields.insert(0, prompt);
        let req = images_request(path, model, content_type, form(&fields));
        let error = executor
            .images(&credential, req, path, &cfg)
            .await
            .err()
            .unwrap_or_else(|| panic!("{model} {fields:?}: accepted"));
        assert_eq!(
            (error.status, error.scope),
            (400, FailureScope::Request),
            "{model} {fields:?}"
        );
        assert_eq!(String::from_utf8_lossy(&error.body), message, "{model} {fields:?}");
    }
    assert!(mock.take().is_empty(), "a refused form reached the upstream");
}

/// The values the policy keeps: an integer count reaches the tool with its value, `n`
/// equal to one in another spelling is the tool's one image, a valid `stream` spelling
/// passes, and the direct path keeps count text it cannot read as an integer as a JSON
/// string for the Images endpoint to judge (docs/codex/CONTRACT.md "Request boundary").
#[tokio::test]
async fn images_form_controls_keep_their_values() {
    let mock = Mock::start().await;
    let executor = executor();
    let cfg = Config::default();
    let credential = credential(&mock.url);
    let path = "/v1/images/edits";
    let content_type = "multipart/form-data; boundary=B";
    let fields: Vec<(&str, &[u8])> = vec![
        ("prompt", b"p"),
        ("n", b"+1.0"),
        ("stream", b" TRUE "),
        ("partial_images", b" 2 "),
        ("output_compression", b"-0"),
    ];
    mock.script("/responses", vec![sse_reply()]);
    // The fixture's completion has no image output: the adapter answers 502 after the send.
    let _ = executor
        .images(
            &credential,
            images_request(path, "gpt-5.4", content_type, form(&fields)),
            path,
            &cfg,
        )
        .await;
    let sent = mock.take();
    assert_eq!(sent.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(
        body["tools"][0],
        serde_json::json!({"type":"image_generation","action":"edit","model":"gpt-5.4","output_compression":0,"partial_images":2})
    );

    mock.script(
        "/images/edits",
        vec![crate::codex_testkit::json(200, r#"{"created":1,"data":[]}"#)],
    );
    let fields: Vec<(&str, &[u8])> = vec![
        ("prompt", b"p"),
        ("stream", b"false"),
        ("partial_images", b"nope"),
        ("output_compression", b"70"),
    ];
    executor
        .images(
            &credential,
            images_request(path, "gpt-image-2", content_type, form(&fields)),
            path,
            &cfg,
        )
        .await
        .unwrap();
    let sent = mock.take();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].path, "/images/edits");
    assert_eq!(
        std::str::from_utf8(&sent[0].body).unwrap(),
        r#"{"model":"gpt-image-2","prompt":"p","partial_images":"nope","output_compression":70}"#
    );
}

/// `n` in JSON is compared with one by its decimal digits (#22): a float rounds
/// `1.0000000000000001` to one, and the tool would have dropped it as the default.
/// Other spellings of one are still the tool's one image.
#[tokio::test]
async fn images_json_n_is_one_only_exactly() {
    let mock = Mock::start().await;
    let executor = executor();
    let cfg = Config::default();
    let credential = credential(&mock.url);
    let path = "/v1/images/generations";
    for n in ["1.0000000000000001", "0.99999999999999999", "1e-1", "2"] {
        let body = format!(r#"{{"prompt":"p","n":{n}}}"#);
        let error = executor
            .images(
                &credential,
                images_request(path, "gpt-5.4", "application/json", body),
                path,
                &cfg,
            )
            .await
            .err()
            .unwrap_or_else(|| panic!("n {n}: accepted"));
        assert_eq!((error.status, error.scope), (400, FailureScope::Request), "n {n}");
        assert!(
            String::from_utf8_lossy(&error.body).starts_with("unsupported conversion: `n`"),
            "n {n}"
        );
    }
    assert!(mock.take().is_empty(), "a refused count reached the upstream");
    for n in ["1", "1.0", "1e0", "10e-1", "0.1E+1"] {
        mock.script("/responses", vec![sse_reply()]);
        let body = format!(r#"{{"prompt":"p","n":{n}}}"#);
        let _ = executor
            .images(
                &credential,
                images_request(path, "gpt-5.4", "application/json", body),
                path,
                &cfg,
            )
            .await;
        let sent = mock.take();
        assert_eq!(sent.len(), 1, "n {n}");
        assert!(!std::str::from_utf8(&sent[0].body).unwrap().contains("\"n\""), "n {n}");
    }
}

/// A valid streaming multipart edit on the tool path, written by hand: `partial_images`
/// reaches the tool, and the client gets the partial image and the completed image as
/// Images API events. This keeps streaming coverage independent of the Go recording
/// whose `partial_images=nope` is now refused.
#[tokio::test]
async fn images_tool_stream_delivers_partial_and_completed_images() {
    let mock = Mock::start().await;
    let executor = executor();
    let cfg = Config::default();
    let credential = credential(&mock.url);
    let events = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_hand\"}}\n\n",
        "data: {\"type\":\"response.image_generation_call.partial_image\",\"partial_image_index\":0,\"partial_image_b64\":\"UEFSVA==\",\"output_format\":\"png\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_hand\",\"created_at\":7,\"output\":[{\"type\":\"image_generation_call\",\"id\":\"ig_hand\",\"status\":\"completed\",\"result\":\"RklOQUw=\",\"output_format\":\"png\"}],\"tool_usage\":{\"image_gen\":{\"input_tokens\":3,\"output_tokens\":4,\"total_tokens\":7}}}}\n\n",
    );
    mock.script(
        "/responses",
        vec![Reply {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: events.into(),
        }],
    );
    let path = "/v1/images/edits";
    let mut body = form(&[("prompt", b"add a hat"), ("partial_images", b"2")]);
    // One image file, after the text fields.
    body.truncate(body.len() - b"--B--\r\n".len());
    body.extend_from_slice(
        b"--B\r\nContent-Disposition: form-data; name=\"image\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\nPNG\r\n--B--\r\n",
    );
    let mut req = images_request(path, "gpt-5.4", "multipart/form-data; boundary=B", body);
    req.stream = true;
    let response = executor.images(&credential, req, path, &cfg).await.unwrap();
    let ResponseBody::Stream(mut stream) = response.body else {
        panic!("a streaming edit answers with a stream");
    };
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        out.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(
        String::from_utf8(out).unwrap(),
        concat!(
            "event: image_edit.partial_image\n",
            "data: {\"type\":\"image_edit.partial_image\",\"partial_image_index\":0,\"b64_json\":\"UEFSVA==\"}\n\n",
            "event: image_edit.completed\n",
            "data: {\"type\":\"image_edit.completed\",\"usage\":{\"input_tokens\":3,\"output_tokens\":4,\"total_tokens\":7},\"b64_json\":\"RklOQUw=\"}\n\n",
        )
    );
    let sent = mock.take();
    assert_eq!(sent.len(), 1);
    let sent: serde_json::Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(sent["tools"][0]["partial_images"], 2);
    assert_eq!(
        sent["input"][0]["content"][1],
        serde_json::json!({"type":"input_image","image_url":"data:image/png;base64,UE5H"})
    );
}

/// A local token count reads a Responses body through the same boundary as the routes
/// that would send it.
#[tokio::test]
async fn token_counts_read_responses_bodies_through_the_boundary() {
    let executor = executor();
    let cfg = Config::default();
    // Nothing is sent: the base URL is unroutable.
    let credential = credential("http://127.0.0.1:1");
    for source in [Format::OpenAIResponse, Format::Codex] {
        for (name, body, status) in refused() {
            let mut req = request(source, body, false, None);
            req.operation = Operation::CountTokens;
            let error = executor
                .execute(&credential, req, &cfg)
                .await
                .err()
                .unwrap_or_else(|| panic!("{name}: counted"));
            assert_eq!((error.status, error.scope), (status, FailureScope::Request), "{name}");
        }
        let mut req = request(source, fixture("http-request-full").bytes(), false, None);
        req.operation = Operation::CountTokens;
        let response = executor.execute(&credential, req, &cfg).await.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.headers[crate::codex::LOCAL_TOKEN_COUNT], "local-estimate");
    }
}
