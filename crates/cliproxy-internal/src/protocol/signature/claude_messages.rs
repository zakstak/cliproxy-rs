//! Claude Messages history sanitizing for a target provider (claude_messages_sanitize.go).

use std::borrow::Cow;

use super::claude::is_empty_thinking_placeholder;
use super::{Action, BlockKind, Decision, Provider, decide_compatibility_for_model, normalize_target};
use crate::protocol::json::{self, Res};

/// `ClaudeMessagesSignatureSanitizeOptions`.
#[derive(Debug, Clone, Default)]
pub struct ClaudeMessagesSanitizeOptions {
    /// `Provider::Empty` is Go's zero value.
    pub target_provider: Provider,
    pub target_model: String,
    pub drop_empty_messages: bool,
    pub drop_tool_signatures: bool,
    pub drop_empty_thinking_placeholders: bool,
    pub preserve_empty_thinking_blocks: bool,
}

/// `SignatureSanitizeReport`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitizeReport {
    pub target_provider: Provider,
    pub preserved: usize,
    pub dropped_blocks: usize,
    pub dropped_signatures: usize,
    pub replaced_signatures: usize,
    pub decisions: Vec<Decision>,
}

/// `SanitizeClaudeMessagesSignaturesForModel`.
pub fn sanitize_claude_messages_signatures_for_model(payload: &[u8], target_model: &str) -> (Vec<u8>, SanitizeReport) {
    sanitize_claude_messages_signatures_for_target(
        payload,
        &ClaudeMessagesSanitizeOptions {
            target_provider: super::provider_from_model_name(target_model),
            target_model: target_model.into(),
            drop_empty_messages: true,
            ..Default::default()
        },
    )
}

/// `SanitizeClaudeMessagesForClaudeUpstream`; `None` when the payload is unchanged.
pub fn sanitize_claude_messages_for_claude_upstream(
    payload: &[u8],
    target_model: &str,
    preserve_empty_thinking_blocks: bool,
) -> (Option<Vec<u8>>, SanitizeReport) {
    sanitize(
        payload,
        &ClaudeMessagesSanitizeOptions {
            target_provider: Provider::Claude,
            target_model: target_model.into(),
            drop_empty_messages: true,
            drop_tool_signatures: true,
            drop_empty_thinking_placeholders: !preserve_empty_thinking_blocks,
            preserve_empty_thinking_blocks,
        },
    )
}

/// `SanitizeClaudeMessagesSignaturesForTarget`.
pub fn sanitize_claude_messages_signatures_for_target(
    payload: &[u8],
    opts: &ClaudeMessagesSanitizeOptions,
) -> (Vec<u8>, SanitizeReport) {
    let (out, report) = sanitize(payload, opts);
    (out.unwrap_or_else(|| payload.to_vec()), report)
}

/// [`sanitize_claude_messages_signatures_for_target`], `None` when nothing changed.
/// Untouched messages and blocks are borrowed from the payload, so only the edited
/// ones and the rebuilt document are copied.
fn sanitize(payload: &[u8], opts: &ClaudeMessagesSanitizeOptions) -> (Option<Vec<u8>>, SanitizeReport) {
    let mut target = normalize_target(opts.target_provider);
    if target == Provider::Unknown && !opts.target_model.is_empty() {
        target = super::provider_from_model_name(&opts.target_model);
    }
    let mut report = SanitizeReport {
        target_provider: target,
        preserved: 0,
        dropped_blocks: 0,
        dropped_signatures: 0,
        replaced_signatures: 0,
        decisions: Vec::new(),
    };
    let messages = json::get(payload, "messages");
    if !messages.is_array() {
        return (None, report);
    }
    let mut kept_messages: Vec<Cow<'_, [u8]>> = Vec::new();
    let mut modified = false;
    for (i, message) in messages.array().iter().enumerate() {
        let content = message.get("content");
        if !content.is_array() {
            kept_messages.push(message.raw.clone());
            continue;
        }
        let mut kept_parts: Vec<Cow<'_, [u8]>> = Vec::new();
        let mut message_modified = false;
        for (j, part) in content.array().iter().enumerate() {
            let part_type = part.get("type").bytes();
            if &*part_type == b"tool_use" {
                if opts.drop_tool_signatures {
                    let (updated, changed) = strip_tool_use_signature_fields(part);
                    if changed {
                        message_modified = true;
                        report.dropped_signatures += 1;
                        kept_parts.push(Cow::Owned(updated));
                    } else {
                        kept_parts.push(part.raw.clone());
                    }
                    continue;
                }
                let (updated, changed, decisions) = sanitize_tool_use_signature(part, target, &opts.target_model, i, j);
                if changed {
                    message_modified = true;
                    kept_parts.push(Cow::Owned(updated));
                } else {
                    kept_parts.push(part.raw.clone());
                }
                for decision in &decisions {
                    match decision.action {
                        Action::Preserve => report.preserved += 1,
                        Action::ReplaceWithGeminiBypass => report.replaced_signatures += 1,
                        _ => report.dropped_signatures += 1,
                    }
                }
                report.decisions.extend(decisions);
                continue;
            }
            if &*part_type != b"thinking" {
                kept_parts.push(part.raw.clone());
                continue;
            }
            let raw_signature = part.get("signature").bytes();
            if opts.preserve_empty_thinking_blocks {
                report.preserved += 1;
                kept_parts.push(part.raw.clone());
                continue;
            }
            if target == Provider::Claude
                && is_empty_thinking_placeholder(part)
                && !opts.drop_empty_thinking_placeholders
            {
                kept_parts.push(part.raw.clone());
                continue;
            }
            let mut decision =
                decide_compatibility_for_model(target, &opts.target_model, &raw_signature, BlockKind::ClaudeThinking);
            decision.reason = format!("messages[{i}].content[{j}]: {}", decision.reason);
            let action = decision.action;
            let normalized = decision.normalized_signature.clone();
            let replacement = decision.replacement_signature.clone();
            report.decisions.push(decision);
            let updated = || part.raw().to_vec();
            match action {
                Action::Preserve => {
                    report.preserved += 1;
                    if !normalized.is_empty() && normalized.as_bytes() != &*raw_signature {
                        let mut updated = updated();
                        json::set_str(&mut updated, "signature", &normalized);
                        kept_parts.push(Cow::Owned(updated));
                        message_modified = true;
                    } else {
                        kept_parts.push(part.raw.clone());
                    }
                }
                Action::ReplaceWithGeminiBypass => {
                    report.replaced_signatures += 1;
                    let mut updated = updated();
                    json::set_str(&mut updated, "signature", &replacement);
                    kept_parts.push(Cow::Owned(updated));
                    message_modified = true;
                }
                Action::DropSignature => {
                    report.dropped_signatures += 1;
                    let mut updated = updated();
                    json::delete(&mut updated, "signature");
                    kept_parts.push(Cow::Owned(updated));
                    message_modified = true;
                }
                _ => {
                    report.dropped_blocks += 1;
                    message_modified = true;
                }
            }
        }
        if message_modified {
            modified = true;
            if kept_parts.is_empty() && opts.drop_empty_messages {
                continue;
            }
            // sjson leaves the document unchanged when it cannot set the path.
            let updated = json::try_set_raw(message.raw(), "content", json::join(&kept_parts))
                .unwrap_or_else(|_| message.raw().to_vec());
            kept_messages.push(Cow::Owned(updated));
            continue;
        }
        kept_messages.push(message.raw.clone());
    }
    if !modified {
        return (None, report);
    }
    let joined = json::join(&kept_messages);
    drop(kept_messages);
    let out = json::try_set_raw(payload, "messages", joined).unwrap_or_else(|_| payload.to_vec());
    (Some(out), report)
}

const TOOL_USE_SIGNATURE_PATHS: [&str; 4] = [
    "signature",
    "thoughtSignature",
    "thought_signature",
    "extra_content.google.thought_signature",
];

fn strip_tool_use_signature_fields(part: &Res<'_>) -> (Vec<u8>, bool) {
    let mut updated = part.raw().to_vec();
    let mut changed = false;
    for path in TOOL_USE_SIGNATURE_PATHS.iter().copied().chain(["model"]) {
        if !json::get(&updated, path).exists() {
            continue;
        }
        json::delete(&mut updated, path);
        changed = true;
    }
    for path in ["extra_content.google", "extra_content"] {
        changed |= delete_empty_object(&mut updated, path);
    }
    (updated, changed)
}

fn sanitize_tool_use_signature(
    part: &Res<'_>,
    target: Provider,
    target_model: &str,
    message_idx: usize,
    part_idx: usize,
) -> (Vec<u8>, bool, Vec<Decision>) {
    let mut updated = part.raw().to_vec();
    let mut changed = false;
    let mut decisions = Vec::new();
    for path in TOOL_USE_SIGNATURE_PATHS {
        let sig = part.get(path);
        if !sig.exists() {
            continue;
        }
        let block_kind = match target {
            Provider::Claude => BlockKind::ClaudeThinking,
            Provider::Gpt => BlockKind::GptReasoning,
            _ => BlockKind::GeminiFunctionCall,
        };
        let raw = sig.bytes();
        let mut decision = decide_compatibility_for_model(target, target_model, &raw, block_kind);
        decision.reason = format!(
            "messages[{message_idx}].content[{part_idx}].{path}: {}",
            decision.reason
        );
        match decision.action {
            Action::Preserve => {
                if !decision.normalized_signature.is_empty() && decision.normalized_signature.as_bytes() != &*raw {
                    json::set_str(&mut updated, path, &decision.normalized_signature);
                    changed = true;
                }
            }
            Action::ReplaceWithGeminiBypass => {
                json::set_str(&mut updated, path, &decision.replacement_signature);
                changed = true;
            }
            _ => {
                json::delete(&mut updated, path);
                changed = true;
            }
        }
        decisions.push(decision);
    }
    for path in ["extra_content.google", "extra_content"] {
        changed |= delete_empty_object(&mut updated, path);
    }
    (updated, changed, decisions)
}

/// `deleteEmptyJSONObjectPath`: true when an empty object at `path` was removed.
fn delete_empty_object(raw: &mut Vec<u8>, path: &str) -> bool {
    let value = json::get(raw, path);
    if !value.exists() || !value.is_object() || !value.map().is_empty() {
        return false;
    }
    json::try_delete(raw, path).map(|next| *raw = next).is_ok()
}
