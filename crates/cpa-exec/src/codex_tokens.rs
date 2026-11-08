//! Local input-token counting for Codex request bodies (codex_executor_tokens.go): the
//! text a Responses request carries, joined and counted with tiktoken.

use std::borrow::Cow;

use cpa_common::gostr::trim_space;
use cpa_common::json::{self as gj, Kind, Res};
use cpa_core::gostr::GoStr;

use crate::tokenizer::{Encoding, encoder};

/// `tokenizerForCodexModel`: o200k for GPT-5, GPT-4.1 and GPT-4o, cl100k otherwise.
pub(crate) fn encoding_for_model(model: &str) -> Encoding {
    let model = model.trim().go_lower();
    if ["gpt-5", "gpt-4.1", "gpt-4o"].iter().any(|p| model.starts_with(p)) {
        Encoding::O200kBase
    } else {
        Encoding::Cl100kBase
    }
}

/// `countCodexInputTokens`: instructions, message text, function calls and outputs, other
/// items' `text`, tool names, descriptions and parameters, and the text format's name
/// and schema, each trimmed, joined with newlines.
pub(crate) fn count_input_tokens(encoding: Encoding, body: &[u8]) -> Result<i64, String> {
    if body.is_empty() {
        return Ok(0);
    }
    // Each segment borrows the body unless it had to be unescaped.
    let mut segments: Vec<Cow<'_, [u8]>> = Vec::new();
    let mut push = |s| segments.push(s);
    let root = gj::parse(body);
    push(root.get("instructions").bytes());
    let input = root.get("input");
    if input.is_array() {
        for item in input.array() {
            match &*item.get("type").bytes() {
                b"message" => {
                    let content = item.get("content");
                    if content.is_array() {
                        for part in content.array() {
                            push(part.get("text").bytes());
                        }
                    }
                }
                b"function_call" => {
                    push(item.get("name").bytes());
                    push(item.get("arguments").bytes());
                }
                b"function_call_output" => push(item.get("output").bytes()),
                _ => push(item.get("text").bytes()),
            }
        }
    }
    let tools = root.get("tools");
    if tools.is_array() {
        for tool in tools.array() {
            push(tool.get("name").bytes());
            push(tool.get("description").bytes());
            let params = tool.get("parameters");
            if params.exists() {
                push(raw_or_string(params));
            }
        }
    }
    let format = root.get("text.format");
    if format.exists() {
        push(format.get("name").bytes());
        let schema = format.get("schema");
        if schema.exists() {
            push(raw_or_string(schema));
        }
    }
    let joined = segments
        .iter()
        .map(|s| trim_space(s))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(&b'\n');
    // The text is UTF-8 whenever the body was: a Responses body passed the request
    // boundary, and a translated one was serialized here.
    let text = String::from_utf8(joined).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
    if text.is_empty() {
        return Ok(0);
    }
    let count = encoder(encoding)?.encode_ordinary(&text).len();
    Ok(i64::try_from(count).unwrap_or(i64::MAX))
}

/// `params.Raw`, or the string value for a string.
fn raw_or_string(v: Res<'_>) -> Cow<'_, [u8]> {
    if v.kind == Kind::String { v.bytes() } else { v.raw }
}

#[cfg(test)]
#[path = "codex_tokens_tests.rs"]
mod tests;
