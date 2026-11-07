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
        stream_payloads: false,
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
