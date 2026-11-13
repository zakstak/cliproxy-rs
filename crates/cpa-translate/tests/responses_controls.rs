//! #80: the registered client -> Codex pairs, which the Grok and Muse executors share,
//! keep a client's output limit and an explicit `parallel_tool_calls: false`, and refuse
//! a value of either that the Responses body cannot carry. The Codex translated route
//! (`codex_request`) keeps its own rules. Go v8.0.35 drops the limit and forces
//! parallel calls (docs/DIFFERENCES-FROM-GO.md); the final outbound bodies are proved in
//! cpa-server's `responses_controls_e2e`.

use cpa_common::json as gj;
use cpa_core::format::Format;
use cpa_translate::{RequestCtx, codex_request, translate_request};

const CTX: RequestCtx<'static> = RequestCtx {
    model: "grok-4.7",
    stream: true,
    tool_search: false,
};

fn field(out: &[u8], path: &str) -> Option<String> {
    let value = gj::get(out, path);
    value
        .exists()
        .then(|| String::from_utf8(value.raw.into_owned()).unwrap())
}

fn chat(extra: &str) -> String {
    format!(
        r#"{{"model":"grok-4.7","messages":[{{"role":"user","content":"hi"}}],"tools":[{{"type":"function","function":{{"name":"f","parameters":{{"type":"object"}}}}}}]{extra}}}"#
    )
}

fn claude(extra: &str) -> String {
    format!(
        r#"{{"model":"grok-4.7","messages":[{{"role":"user","content":"hi"}}],"tools":[{{"name":"f","input_schema":{{"type":"object"}}}}]{extra}}}"#
    )
}

#[test]
fn chat_keeps_the_output_limit_and_parallel_false() {
    for (extra, limit, parallel) in [
        ("", None, "true"),
        (r#","max_tokens":17"#, Some("17"), "true"),
        (r#","max_completion_tokens":17,"max_tokens":8"#, Some("17"), "true"),
        (r#","max_completion_tokens":null,"max_tokens":8"#, Some("8"), "true"),
        (r#","parallel_tool_calls":false"#, None, "false"),
        (r#","parallel_tool_calls":true"#, None, "true"),
        (r#","parallel_tool_calls":null"#, None, "true"),
        (
            r#","max_completion_tokens":17,"parallel_tool_calls":false"#,
            Some("17"),
            "false",
        ),
    ] {
        let out = translate_request(Format::OpenAI, Format::Codex, &CTX, chat(extra).as_bytes()).unwrap();
        assert_eq!(field(&out, "max_output_tokens").as_deref(), limit, "{extra}");
        assert_eq!(field(&out, "parallel_tool_calls").as_deref(), Some(parallel), "{extra}");
        for legacy in ["max_tokens", "max_completion_tokens"] {
            assert_eq!(field(&out, legacy), None, "{extra}: {legacy}");
        }
    }
    // Go v8.0.67's position: right after `stream`.
    let out = translate_request(
        Format::OpenAI,
        Format::Codex,
        &CTX,
        chat(r#","max_tokens":17"#).as_bytes(),
    )
    .unwrap();
    assert!(
        out.starts_with(br#"{"instructions":"","stream":true,"max_output_tokens":17,"#),
        "{}",
        String::from_utf8_lossy(&out)
    );
}

#[test]
fn claude_keeps_max_tokens_and_disable_parallel_tool_use() {
    for (extra, limit, parallel) in [
        ("", None, "true"),
        (r#","max_tokens":17"#, Some("17"), "true"),
        (r#","max_tokens":null"#, None, "true"),
        (
            r#","max_tokens":17,"tool_choice":{"type":"auto","disable_parallel_tool_use":true}"#,
            Some("17"),
            "false",
        ),
        (
            r#","max_tokens":17,"tool_choice":{"type":"auto","disable_parallel_tool_use":false}"#,
            Some("17"),
            "true",
        ),
    ] {
        let body = claude(extra);
        let out = translate_request(Format::Claude, Format::Codex, &CTX, body.as_bytes()).unwrap();
        assert_eq!(field(&out, "max_output_tokens").as_deref(), limit, "{extra}");
        assert_eq!(field(&out, "parallel_tool_calls").as_deref(), Some(parallel), "{extra}");
        // The compat variant (an is-compat xai-api-key model) converts the same way.
        let compat = cpa_translate::claude_to_codex_with_compat(&CTX, body.as_bytes()).unwrap();
        assert_eq!(field(&compat, "max_output_tokens").as_deref(), limit, "compat {extra}");
    }
    // Go v8.0.67's position: after the template's `input`, before every later field.
    let out = translate_request(
        Format::Claude,
        Format::Codex,
        &CTX,
        claude(r#","max_tokens":17"#).as_bytes(),
    )
    .unwrap();
    let text = String::from_utf8(out).unwrap();
    let at = |needle: &str| text.find(needle).unwrap_or_else(|| panic!("{needle} in {text}"));
    assert!(
        text.starts_with(r#"{"model":"grok-4.7","instructions":"","input":["#),
        "{text}"
    );
    assert!(at(r#"}]}],"max_output_tokens":17,"#) < at(r#""tools":["#), "{text}");
}

#[test]
fn unreadable_controls_are_refused() {
    for (format, body, message) in [
        (
            Format::OpenAI,
            chat(r#","max_tokens":"17""#),
            "`max_tokens` must be a number",
        ),
        (
            Format::OpenAI,
            chat(r#","max_completion_tokens":true"#),
            "`max_completion_tokens` must be a number",
        ),
        (
            Format::OpenAI,
            chat(r#","parallel_tool_calls":"false""#),
            "`parallel_tool_calls` must be a boolean",
        ),
        (
            Format::Claude,
            claude(r#","max_tokens":"17""#),
            "`max_tokens` must be a number",
        ),
        (
            Format::Claude,
            claude(r#","tool_choice":{"type":"auto","disable_parallel_tool_use":"yes"}"#),
            "`tool_choice.disable_parallel_tool_use` must be a boolean",
        ),
    ] {
        let err = translate_request(format, Format::Codex, &CTX, body.as_bytes()).unwrap_err();
        assert!(err.0.contains(message), "{body}: {err}");
    }
}

/// The Codex translated route is not the registered pair: its output is unchanged by #80.
#[test]
fn codex_translated_route_keeps_its_own_rules() {
    let out = codex_request(
        Format::OpenAI,
        &CTX,
        chat(r#","max_tokens":17,"parallel_tool_calls":false"#).as_bytes(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(field(&out, "parallel_tool_calls").as_deref(), Some("false"));
    // Mapped by codex_strict at the end of the body, not by the registered pair's rule.
    assert!(
        out.ends_with(br#","max_output_tokens":17}"#),
        "{}",
        String::from_utf8_lossy(&out)
    );
    let out = codex_request(Format::Claude, &CTX, claude(r#","max_tokens":17"#).as_bytes())
        .unwrap()
        .unwrap();
    assert!(
        out.ends_with(br#","max_output_tokens":17}"#),
        "{}",
        String::from_utf8_lossy(&out)
    );
}
