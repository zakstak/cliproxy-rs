//! Local token-estimate behavior uses independently specified text examples.
//! These estimates are never evidence of provider-reported usage.

use super::*;
use crate::config::Config;
use crate::credential::{Credential, Source};
use crate::execution::{Caller, ExecRequest, Operation, ResponseBody};
use crate::format::Format;
use bytes::Bytes;
use serde_json::Value;

fn request(v: &Value) -> ExecRequest {
    let source = Format::parse(v["from"].as_str().unwrap()).unwrap();
    let mut headers = http::HeaderMap::new();
    for (k, val) in v["headers"].as_object().into_iter().flatten() {
        headers.insert(
            http::HeaderName::try_from(k.as_str()).unwrap(),
            val.as_str().unwrap().parse().unwrap(),
        );
    }
    let body = Bytes::from(v["in"].as_str().unwrap().to_owned());
    ExecRequest {
        operation: Operation::CountTokens,
        source_format: source,
        response_format: Format::parse(v["response"].as_str().unwrap()).unwrap(),
        requested_model: v["model"].as_str().unwrap().into(),
        model: v["model"].as_str().unwrap().into(),
        original_body: body.clone(),
        body,
        stream: false,
        alt: None,
        session: None,
        headers,
        execution_session: None,
        derived_session: None,
        request_path: String::new(),
        registry: None,
        resolved_model: None,
        usage: Default::default(),
        caller: Caller {
            principal: "client-key-FAKE".into(),
            source: "authorization",
            provider: String::new(),
        },
    }
}

/// These counts use tiktoken's ordinary-text vocabulary: "hello" is one token and
/// "hello world" is two in both supported encodings. Each extraction position is
/// tested alone so an unrelated field cannot compensate for a missing segment.
#[test]
fn token_text_extraction_counts_each_supported_segment_and_ignores_nontext_inputs() {
    for encoding in [Encoding::O200kBase, Encoding::Cl100kBase] {
        for body in [
            serde_json::json!({"instructions":"  hello  "}),
            serde_json::json!({"input":[{"type":"message","content":[{"type":"input_text","text":"hello"}]}]}),
            serde_json::json!({"input":[{"type":"function_call","name":"hello"}]}),
            serde_json::json!({"input":[{"type":"function_call","arguments":"hello"}]}),
            serde_json::json!({"input":[{"type":"function_call_output","output":"hello"}]}),
            serde_json::json!({"input":[{"type":"reasoning","text":"hello"}]}),
            serde_json::json!({"tools":[{"name":"hello"}]}),
            serde_json::json!({"tools":[{"description":"hello"}]}),
            serde_json::json!({"tools":[{"parameters":"hello"}]}),
            serde_json::json!({"tools":[{"parameters":{}}]}),
            serde_json::json!({"text":{"format":{"name":"hello"}}}),
            serde_json::json!({"text":{"format":{"schema":"hello"}}}),
            serde_json::json!({"text":{"format":{"schema":{}}}}),
        ] {
            assert_eq!(
                count_input_tokens(encoding, body.to_string().as_bytes()).unwrap(),
                1,
                "{body}"
            );
        }
        for body in [
            serde_json::json!({}),
            serde_json::json!({"instructions":" \t\n "}),
            serde_json::json!({"input":[{"type":"message","content":[{"type":"input_image","image_url":"https://example.invalid/image.png"}]}]}),
        ] {
            assert_eq!(
                count_input_tokens(encoding, body.to_string().as_bytes()).unwrap(),
                0,
                "{body}"
            );
        }
        assert_eq!(
            count_input_tokens(encoding, br#"{"instructions":"hello world"}"#).unwrap(),
            2
        );
        assert_eq!(
            count_input_tokens(
                encoding,
                br#"{"instructions":" hello ","input":[{"type":"function_call_output","output":" world "}]}"#
            )
            .unwrap(),
            3
        );
        assert_eq!(count_input_tokens(encoding, b"").unwrap(), 0);
    }
}

#[tokio::test]
async fn token_estimates_cover_native_and_translated_clients_without_upstream_traffic() {
    let mock = crate::providers::codex_testkit::Mock::start().await;
    let credential = Credential {
        id: "local-count.json".into(),
        provider: "codex".into(),
        source: Source::File("/fake/local-count.json".into()),
        disabled: false,
        label: "local".into(),
        attributes: [("base_url".to_owned(), mock.url.clone())].into(),
        metadata: Default::default(),
        revision: 0,
    };
    let executor = crate::providers::codex::CodexExecutor::new().unwrap();
    let cfg = Config::default();
    // Each source describes exactly the same text. Counts are local estimates, not
    // provider-reported usage; no real credential or auth directory is loaded.
    for model in ["gpt-5.5", "gpt-5.5(high)", "gpt-4.1-mini", "gpt-4o-mini", "gpt-4", "o3"] {
        for (format, body) in [
            (
                "openai-response",
                r#"{"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello world"}]}]}"#,
            ),
            ("openai", r#"{"messages":[{"role":"user","content":"hello world"}]}"#),
            (
                "claude",
                r#"{"max_tokens":32,"messages":[{"role":"user","content":"hello world"}]}"#,
            ),
        ] {
            let vector = serde_json::json!({"from":format,"response":"openai-response","model":model,"in":body});
            let response = executor.execute(&credential, request(&vector), &cfg).await.unwrap();
            assert_eq!(response.status, 200);
            assert_eq!(
                response.headers[crate::providers::codex::LOCAL_TOKEN_COUNT],
                "local-estimate"
            );
            let ResponseBody::Buffered(body) = response.body else {
                panic!("local token count is buffered")
            };
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(
                body,
                serde_json::json!({"response":{"usage":{"input_tokens":2,"output_tokens":0,"total_tokens":2}}})
            );
        }
    }
    assert!(
        mock.take().is_empty(),
        "local counting never contacts the selected provider"
    );
}

/// This is the proxy's local-estimate model policy, not a claim about provider usage.
#[test]
fn local_estimate_models_choose_the_documented_tokenizer_family() {
    for (model, want) in [
        ("gpt-5.5", Encoding::O200kBase),
        (" GPT-4.1-mini", Encoding::O200kBase),
        ("gpt-4o", Encoding::O200kBase),
        ("gpt-4", Encoding::Cl100kBase),
        ("gpt-3.5-turbo", Encoding::Cl100kBase),
        ("o3", Encoding::Cl100kBase),
        ("", Encoding::Cl100kBase),
    ] {
        assert_eq!(encoding_for_model(model), want, "{model}");
    }
}
