//! Pure Responses request and response rewrites shared by the xAI, Meta and Codex
//! executors, each a port of the Go helper named in its doc comment.


use cpa_common::json::{self as gj, Kind};
use cpa_core::credential::Credential;
use http::HeaderMap;

use crate::go_std as go;

/// `Auth.AuthKind`, empty when unclassified.
pub(crate) fn auth_kind(credential: &Credential) -> &'static str {
    cpa_core::registry::dynamic::auth_kind(credential).unwrap_or_default()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || haystack.windows(needle.len()).any(|w| w == needle)
}

/// `helps.SetStringIfDifferent`.
pub(crate) fn set_str_if_different(body: &mut Vec<u8>, path: &str, value: &str) {
    let current = gj::get(body, path);
    if current.kind == Kind::String && *current.bytes() == *value.as_bytes() {
        return;
    }
    gj::set_str(body, path, value);
}

/// `helps.SetBoolIfDifferent`.
pub(crate) fn set_bool_if_different(body: &mut Vec<u8>, path: &str, value: bool) {
    let current = gj::get(body, path).kind;
    if current == if value { Kind::True } else { Kind::False } {
        return;
    }
    gj::set_bool(body, path, value);
}

/// `uuid.NewSHA1(uuid.NameSpaceOID, identity)`.
fn oid_uuid(identity: &str) -> String {
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, identity.as_bytes()).to_string()
}

/// `headerValueCaseInsensitive`: first non-empty trimmed value.
fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(str::trim)
        .find(|v| !v.is_empty())
        .map(str::to_owned)
}

/// `ClaudeCodePromptCache`: a stable key per Claude Code session, agent and model.
pub(crate) fn claude_code_prompt_cache(model: &str, payload: &[u8], headers: &HeaderMap) -> Option<String> {
    let model = model.trim();
    let session = header_value(headers, "x-claude-code-session-id").or_else(|| {
        let user_id = gj::get(payload, "metadata.user_id").str().into_owned();
        if let Some(pos) = user_id.rfind("_session_") {
            let id = &user_id[pos + "_session_".len()..];
            if !id.is_empty()
                && id
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase() || b == b'-')
            {
                return Some(id.to_owned());
            }
        }
        user_id
            .starts_with('{')
            .then(|| gj::get(user_id.as_bytes(), "session_id").str().trim().to_owned())
            .filter(|s| !s.is_empty())
    })?;
    if model.is_empty() {
        return None;
    }
    let agent = header_value(headers, "x-claude-code-agent-id").unwrap_or_else(|| "main".into());
    Some(oid_uuid(&format!(
        "cli-proxy-api:codex:claude-code\0{model}\0claude:{session}:agent:{agent}"
    )))
}

/// `EnsureResponsesUsageDetails`.
pub fn ensure_responses_usage_details(payload: &[u8]) -> Vec<u8> {
    let trimmed = go::trim_space(payload);
    if trimmed.is_empty() {
        return payload.to_vec();
    }
    let patch = |body: &[u8]| -> Option<Vec<u8>> {
        if *gj::get(body, "object").bytes() == *b"response.compaction" {
            return None;
        }
        let updated = usage_details_at(usage_details_at(body.to_vec(), "response.usage"), "usage");
        (updated != body).then_some(updated)
    };
    if trimmed[0] == b'{' {
        return patch(trimmed).unwrap_or_else(|| payload.to_vec());
    }
    if !contains(payload, b"data:") {
        return payload.to_vec();
    }
    let mut modified = false;
    let lines: Vec<Vec<u8>> = payload
        .split(|&b| b == b'\n')
        .map(|line| {
            if !go::trim_space(line).starts_with(b"data:") {
                return line.to_vec();
            }
            // Go keeps the 5-byte prefix for an indented `data:` line, which then never
            // yields a `{` payload.
            let prefix = if line.starts_with(b"data: ") { 6 } else { 5 };
            let data = go::trim_space(&line[prefix..]);
            if data.first() != Some(&b'{') {
                return line.to_vec();
            }
            match patch(data) {
                Some(updated) => {
                    modified = true;
                    [&line[..prefix], &updated[..]].concat()
                }
                None => line.to_vec(),
            }
        })
        .collect();
    if modified { lines.join(&b'\n') } else { payload.to_vec() }
}

/// `ensureUsageDetailsAt`.
fn usage_details_at(mut body: Vec<u8>, path: &str) -> Vec<u8> {
    if !gj::get(&body, path).is_object() {
        return body;
    }
    for (field, leaf, empty) in [
        ("output_tokens_details", "reasoning_tokens", r#"{"reasoning_tokens":0}"#),
        ("input_tokens_details", "cached_tokens", r#"{"cached_tokens":0}"#),
    ] {
        let details_path = format!("{path}.{field}");
        let leaf_path = format!("{details_path}.{leaf}");
        let (missing, wrong_type, leaf_missing) = {
            let details = gj::get(&body, &details_path);
            let value = details.get(leaf);
            (
                !details.exists(),
                !details.is_object(),
                !value.exists() || value.kind == Kind::Null,
            )
        };
        // Go assigns sjson's result even on error (a nil body); these paths never fail
        // on a document whose usage node is an object.
        if missing {
            gj::set_int(&mut body, &leaf_path, 0);
        } else if wrong_type {
            gj::set_raw(&mut body, &details_path, empty);
        } else if leaf_missing {
            gj::set_int(&mut body, &leaf_path, 0);
        }
    }
    body
}

/// `sanitizeOpenAIResponsesReasoningEncryptedContent` (isCompat false): reasoning
/// `content` is promoted into an empty summary and cleared, ids without usable
/// `encrypted_content` are dropped unless `store` is true, invalid signatures are removed.
pub(crate) fn sanitize_reasoning_encrypted_content(body: Vec<u8>) -> Vec<u8> {
    let input = gj::get(&body, "input");
    if !input.is_array() {
        return body;
    }
    let strip_ids = !gj::get(&body, "store").bool();
    let items = input.array();
    let mut rebuilt: Option<Vec<Vec<u8>>> = None;
    for (index, item) in items.iter().enumerate() {
        let keep = |rebuilt: &mut Option<Vec<Vec<u8>>>, raw: &[u8]| {
            if let Some(list) = rebuilt {
                list.push(raw.to_vec());
            }
        };
        let edit = |rebuilt: &mut Option<Vec<Vec<u8>>>, raw: Vec<u8>| {
            let list = rebuilt.get_or_insert_with(|| items[..index].iter().map(|i| i.raw().to_vec()).collect());
            list.push(raw);
        };
        if item.get("type").str().trim() != "reasoning" {
            keep(&mut rebuilt, item.raw());
            continue;
        }
        let mut next = item.raw().to_vec();
        let mut changed = false;
        let content = item.get("content");
        if content.is_array() && !content.array().is_empty() {
            let summary = item.get("summary");
            let summary_empty =
                !summary.exists() || summary.kind == Kind::Null || (summary.is_array() && summary.array().is_empty());
            if summary_empty {
                let parts: Vec<Vec<u8>> = content
                    .array()
                    .iter()
                    .filter(|p| p.get("type").str().trim() == "reasoning_text")
                    .map(|p| p.get("text").bytes().into_owned())
                    .filter(|text| !text.is_empty())
                    .map(|text| {
                        let mut part = br#"{"type":"summary_text"}"#.to_vec();
                        gj::set_str(&mut part, "text", text);
                        part
                    })
                    .collect();
                if !parts.is_empty() {
                    gj::set_raw(&mut next, "summary", gj::join(&parts));
                }
            }
            if gj::set_raw(&mut next, "content", "[]") {
                changed = true;
            }
        }
        let encrypted = item.get("encrypted_content");
        if !encrypted.exists() {
            if strip_ids && item.get("id").exists() && gj::delete(&mut next, "id") {
                changed = true;
            }
            if changed {
                edit(&mut rebuilt, next);
            } else {
                keep(&mut rebuilt, item.raw());
            }
            continue;
        }
        let invalid = match encrypted.kind {
            Kind::String => {
                let raw = encrypted.str();
                raw != raw.trim() || cpa_common::signature::inspect_gpt_reasoning_signature(raw.as_bytes()).is_err()
            }
            _ => true,
        };
        if !invalid || !gj::delete(&mut next, "encrypted_content") {
            if changed {
                edit(&mut rebuilt, next);
            } else {
                keep(&mut rebuilt, item.raw());
            }
            continue;
        }
        if strip_ids && item.get("id").exists() {
            gj::delete(&mut next, "id");
        }
        edit(&mut rebuilt, next);
    }
    match rebuilt {
        Some(list) => {
            let mut body = body;
            gj::set_raw(&mut body, "input", gj::join(&list));
            body
        }
        None => body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_code_session_from_payload_user_id() {
        let headers = HeaderMap::new();
        let payload = br#"{"metadata":{"user_id":"user_abc_account__session_0f-9a"}}"#;
        let a = claude_code_prompt_cache("m", payload, &headers).unwrap();
        let mut h = HeaderMap::new();
        h.insert("x-claude-code-session-id", "0f-9a".parse().unwrap());
        assert_eq!(claude_code_prompt_cache("m", b"{}", &h).unwrap(), a);
        assert!(claude_code_prompt_cache("m", br#"{"metadata":{"user_id":"x_session_ABC"}}"#, &headers).is_none());
        assert_eq!(
            claude_code_prompt_cache(
                "m",
                br#"{"metadata":{"user_id":"{\"session_id\":\"0f-9a\"}"}}"#,
                &headers
            )
            .unwrap(),
            a
        );
    }
}
