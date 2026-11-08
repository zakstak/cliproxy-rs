//! Request-level rules of the Codex translated route (`docs/codex/CONTRACT.md`
//! "Translated route"): every field a client sends either maps to its documented
//! Responses equivalent, keeps its API default (and so needs nothing), or refuses the
//! request before anything is sent. The built-in converters handle content; this module
//! handles the request's own controls, which they leave out.

use cpa_common::json::{self as gj, Kind, Res};
use cpa_core::format::Format;

use crate::Error;

/// What a source field means for the Responses request.
#[derive(Clone, Copy)]
enum Rule {
    /// The converter already expresses it.
    Converted,
    /// Copied unchanged to this Responses field (the same meaning, documented for both).
    Map(&'static str),
    /// Accepted only while it holds the API default (absent, `null`, or the value the
    /// check allows); any other value cannot be expressed.
    DefaultOnly(fn(&Res<'_>) -> bool),
    /// Not sent, because it does not change the generation, and the ChatGPT backend has
    /// no field that takes it (stated per field in the contract).
    NotSent,
    /// Converted for the values this check accepts (adding what the converter does not
    /// express itself to the Responses body); any other value is refused.
    Values(fn(&Res<'_>, &mut Vec<u8>) -> bool),
}

fn unsupported(field: &str, at: &str) -> Error {
    Error(format!(
        "unsupported conversion: `{field}`{at} has no Codex Responses equivalent; remove it or send the request to a provider that accepts it"
    ))
}

fn null(v: &Res<'_>) -> bool {
    v.kind == Kind::Null
}

fn empty(v: &Res<'_>) -> bool {
    null(v) || (v.is_array() && v.array().is_empty()) || (v.kind == Kind::String && v.s.is_empty())
}

fn zero(v: &Res<'_>) -> bool {
    null(v) || (v.kind == Kind::Number && v.float() == 0.0)
}

fn falsy(v: &Res<'_>) -> bool {
    null(v) || v.kind == Kind::False
}

fn one(v: &Res<'_>) -> bool {
    null(v) || (v.kind == Kind::Number && v.float() == 1.0)
}

fn string_in(v: &Res<'_>, allowed: &[&str]) -> bool {
    v.kind == Kind::String && allowed.iter().any(|a| v.s.eq_ignore_ascii_case(a.as_bytes()))
}

/// Claude `service_tier`: `auto` is the default; `fast` and `priority` become
/// `priority` (the converter). `standard_only` has no Responses equivalent.
fn claude_tier(v: &Res<'_>, _: &mut Vec<u8>) -> bool {
    null(v) || string_in(v, &["auto", "fast", "priority"])
}

/// Claude `speed`: `fast` becomes `priority` (the converter); `standard` is the default.
fn claude_speed(v: &Res<'_>, _: &mut Vec<u8>) -> bool {
    null(v) || string_in(v, &["fast", "standard"])
}

/// Claude `tool_choice` types the converter maps; anything else would widen to `auto`.
fn claude_tool_choice(v: &Res<'_>, _: &mut Vec<u8>) -> bool {
    if null(v) {
        return true;
    }
    let kind = v.get("type");
    match kind.s.as_ref() {
        b"auto" | b"any" | b"none" => true,
        b"tool" => !v.get("name").bytes().is_empty(),
        _ => false,
    }
}

/// Claude `thinking` types the converter maps.
fn claude_thinking(v: &Res<'_>, _: &mut Vec<u8>) -> bool {
    null(v) || string_in(&v.get("type"), &["enabled", "adaptive", "auto", "disabled"])
}

/// Chat `response_format`: `text` and `json_schema` (the converter), and `json_object`
/// as the Responses `text.format` of the same name.
fn chat_response_format(v: &Res<'_>, out: &mut Vec<u8>) -> bool {
    if null(v) {
        return true;
    }
    match v.get("type").s.as_ref() {
        b"text" => true,
        b"json_schema" => v.get("json_schema.schema").exists(),
        b"json_object" => {
            gj::set_raw(out, "text.format", br#"{"type":"json_object"}"#);
            true
        }
        _ => false,
    }
}

fn text_only(v: &Res<'_>) -> bool {
    null(v)
        || (v.is_array()
            && v.array()
                .iter()
                .all(|m| m.kind == Kind::String && m.s.eq_ignore_ascii_case(b"text")))
}

/// Applies `rules` to the members of `object`: maps, checks defaults, and refuses any
/// member no rule names. `at` names the object in the error.
fn apply(object: &Res<'_>, rules: &[(&str, Rule)], at: &str, out: &mut Vec<u8>) -> Result<(), Error> {
    if !object.is_object() {
        return Ok(());
    }
    for (key, value) in object.map() {
        let key = String::from_utf8_lossy(&key).into_owned();
        let Some((_, rule)) = rules.iter().find(|(name, _)| *name == key) else {
            return Err(unsupported(&key, at));
        };
        match *rule {
            Rule::Converted | Rule::NotSent => {}
            Rule::Map(target) => {
                if !null(&value) {
                    gj::set_raw(out, target, &value.raw);
                }
            }
            Rule::DefaultOnly(default) => {
                if !default(&value) {
                    return Err(unsupported(&key, at));
                }
            }
            Rule::Values(check) => {
                if !check(&value, out) {
                    return Err(unsupported(
                        &format!("{key}: {}", String::from_utf8_lossy(&value.raw)),
                        at,
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Claude Messages request fields (Anthropic Messages API).
const CLAUDE: &[(&str, Rule)] = &[
    ("model", Rule::Converted),
    ("messages", Rule::Converted),
    ("system", Rule::Converted),
    ("stream", Rule::Converted),
    ("tools", Rule::Converted),
    ("tool_choice", Rule::Values(claude_tool_choice)),
    ("thinking", Rule::Values(claude_thinking)),
    ("service_tier", Rule::Values(claude_tier)),
    ("speed", Rule::Values(claude_speed)),
    ("output_config", Rule::Converted),
    ("max_tokens", Rule::Map("max_output_tokens")),
    ("temperature", Rule::Map("temperature")),
    ("top_p", Rule::Map("top_p")),
    ("stop_sequences", Rule::DefaultOnly(empty)),
    // An end-user identifier for abuse detection: no generation effect, and the ChatGPT
    // backend takes no `safety_identifier` (Go removes it from every Codex request).
    ("metadata", Rule::NotSent),
];

const CLAUDE_OUTPUT_CONFIG: &[(&str, Rule)] = &[("effort", Rule::Converted), ("format", Rule::Converted)];

/// Chat Completions request fields (OpenAI Chat Completions API).
const CHAT: &[(&str, Rule)] = &[
    ("model", Rule::Converted),
    ("messages", Rule::Converted),
    ("stream", Rule::Converted),
    ("stream_options", Rule::NotSent),
    ("tools", Rule::Converted),
    ("tool_choice", Rule::Converted),
    ("parallel_tool_calls", Rule::Converted),
    ("reasoning_effort", Rule::Converted),
    // The same tier names in both APIs (`auto`, `default`, `flex`, `scale`, `priority`).
    ("service_tier", Rule::Map("service_tier")),
    ("response_format", Rule::Values(chat_response_format)),
    ("text", Rule::Converted),
    ("verbosity", Rule::Map("text.verbosity")),
    ("max_tokens", Rule::Map("max_output_tokens")),
    ("max_completion_tokens", Rule::Map("max_output_tokens")),
    ("temperature", Rule::Map("temperature")),
    ("top_p", Rule::Map("top_p")),
    ("user", Rule::Map("user")),
    ("safety_identifier", Rule::Map("safety_identifier")),
    ("metadata", Rule::Map("metadata")),
    ("prompt_cache_key", Rule::Map("prompt_cache_key")),
    ("prompt_cache_retention", Rule::Map("prompt_cache_retention")),
    // The Codex encrypted-reasoning round trip needs `store: false`.
    ("store", Rule::DefaultOnly(falsy)),
    ("n", Rule::DefaultOnly(one)),
    ("stop", Rule::DefaultOnly(empty)),
    ("presence_penalty", Rule::DefaultOnly(zero)),
    ("frequency_penalty", Rule::DefaultOnly(zero)),
    ("logprobs", Rule::DefaultOnly(falsy)),
    (
        "logit_bias",
        Rule::DefaultOnly(|v| null(v) || (v.is_object() && v.map().is_empty())),
    ),
    ("modalities", Rule::DefaultOnly(text_only)),
];

/// Chat message fields (OpenAI Chat Completions message objects).
const CHAT_MESSAGE: &[(&str, Rule)] = &[
    ("role", Rule::Converted),
    ("content", Rule::Converted),
    ("tool_calls", Rule::Converted),
    ("tool_call_id", Rule::Converted),
    ("name", Rule::DefaultOnly(empty)),
    ("refusal", Rule::DefaultOnly(empty)),
];

/// Checks `raw`, a `client` request, and adds its mapped controls to `out`, the
/// converted Responses body.
pub(crate) fn controls(client: Format, raw: &[u8], out: &mut Vec<u8>) -> Result<(), Error> {
    let root = gj::parse(raw);
    match client {
        Format::Claude => {
            apply(&root, CLAUDE, "", out)?;
            apply(
                &root.get("output_config"),
                CLAUDE_OUTPUT_CONFIG,
                " in `output_config`",
                out,
            )?;
        }
        Format::OpenAI => {
            apply(&root, CHAT, "", out)?;
            for message in root.get("messages").array() {
                apply(&message, CHAT_MESSAGE, " in a message", out)?;
            }
        }
        _ => {}
    }
    Ok(())
}
