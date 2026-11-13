//! #80 for Go goldens recorded at v8.0.35 (docs/DIFFERENCES-FROM-GO.md, "Output limit and
//! parallel tool calls on Grok and Muse"): the registered client -> Codex converters that
//! the xAI and Meta executors share keep the client's output limit and an explicit
//! `parallel_tool_calls: false`, as Go v8.0.67 does (`01b24f1f`, `99d1698f`). Go v8.0.35
//! drops the limit (Grok restored it at the end of the body for Chat Completions and
//! Responses clients) and forces `true`.
//!
//! [`expected_body`] turns Go's recorded upstream body into the one cliproxy-rs sends:
//! the limit at Go v8.0.67's position and the client's `false`. Everything else in the
//! recorded body stays as Go wrote it, so the goldens still compare every other byte.

use cpa_common::json::{self as gj, Kind};
use cpa_core::format::Format;

/// The client's output limit as the converter reads it (cpa_translate
/// `responses_controls`): the first number among the format's fields.
fn client_limit(from: Format, client: &[u8]) -> Option<Vec<u8>> {
    let paths: &[&str] = match from {
        Format::Claude => &["max_tokens", "max_output_tokens"],
        Format::OpenAI => &["max_completion_tokens", "max_tokens", "max_output_tokens"],
        Format::OpenAIResponse => &["max_output_tokens", "max_completion_tokens", "max_tokens"],
        _ => return None,
    };
    paths.iter().find_map(|path| {
        let value = gj::get(client, path);
        (value.kind == Kind::Number).then(|| value.raw.into_owned())
    })
}

/// Byte offset just after the top-level value of `key` in `doc`.
fn end_of(doc: &[u8], key: &str) -> Option<usize> {
    let value = gj::get(doc, key);
    if !value.exists() {
        return None;
    }
    let end = value.index + value.raw.len();
    assert_eq!(&doc[value.index..end], value.raw.as_ref(), "top-level offset of {key}");
    Some(end)
}

/// Go v8.0.35's upstream body `go`, for a `from` client that sent `client`, as the
/// registered converter now writes it.
pub(crate) fn expected_body(from: Format, client: &[u8], go: &[u8]) -> Vec<u8> {
    let mut out = go.to_vec();
    if matches!(from, Format::OpenAI | Format::OpenAIResponse)
        && gj::get(client, "parallel_tool_calls").kind == Kind::False
        && gj::get(&out, "parallel_tool_calls").kind == Kind::True
    {
        gj::set_bool(&mut out, "parallel_tool_calls", false);
    }
    let Some(limit) = client_limit(from, client) else {
        return out;
    };
    // Grok restored the limit at the end of the body; it now sits where the converter
    // writes it.
    if gj::get(&out, "max_output_tokens").exists() {
        gj::delete(&mut out, "max_output_tokens");
    }
    let at = match from {
        Format::Claude => end_of(&out, "input"),
        Format::OpenAI => end_of(&out, "stream"),
        // A Responses body keeps the client's own field in place: after the nearest
        // earlier client field Go kept, else first.
        _ => {
            let fields = gj::parse(client).map();
            let before = fields.iter().position(|(k, _)| k == b"max_output_tokens");
            assert!(before.is_some(), "a Responses limit filled from a legacy field");
            fields[..before.unwrap()]
                .iter()
                .rev()
                .find_map(|(k, _)| end_of(&out, std::str::from_utf8(k).unwrap()))
                .or(Some(1))
        }
    }
    .expect("the converter's anchor field");
    let first = at == 1;
    let mut field = format!(r#""max_output_tokens":{}"#, String::from_utf8_lossy(&limit)).into_bytes();
    if first {
        field.push(b',');
    } else {
        field.insert(0, b',');
    }
    out.splice(at..at, field);
    assert!(gj::valid(&out), "{}", String::from_utf8_lossy(&out));
    out
}
