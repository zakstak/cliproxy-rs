//! Responses compaction through Claude (claude_executor_compaction.go, v8.0.35
//! 2bc236d2): the conversation is summarized by an ordinary non-stream Claude turn and
//! the summary sealed into the CPA compaction capsule Antigravity uses.

use cpa_common::json as gj;

use crate::compaction_capsule as capsule;

/// `prepareClaudeCompactionSummaryPayload`: Antigravity's summary request, keeping the
/// tool definitions Claude needs to accept `tool_use` history.
pub(super) fn summary_payload(source: &[u8]) -> Vec<u8> {
    let mut out = capsule::summary_payload(source);
    for field in ["tools", "additional_tools"] {
        let value = gj::get(source, field);
        if value.exists() {
            gj::set_raw(&mut out, field, value.raw());
        }
    }
    out
}

/// `finalizeClaudeCompactionSummaryBody`, after translation and before payload rules:
/// with tool definitions the summary turn may not call one (`tool_choice: none`);
/// without them tool blocks become text, which Anthropic accepts without definitions.
pub(super) fn finalize(body: String) -> String {
    let mut out = body.into_bytes();
    let tools = gj::get(&out, "tools");
    if tools.is_array() && !tools.array().is_empty() {
        gj::set_raw(&mut out, "tool_choice", br#"{"type":"none"}"#);
    } else if let Some(flattened) = flatten_tool_blocks(&out) {
        out = flattened;
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// `flattenClaudeToolBlocksForCompaction`; `None` when no message has a tool block.
fn flatten_tool_blocks(body: &[u8]) -> Option<Vec<u8>> {
    let messages = gj::get(body, "messages");
    if !messages.is_array() {
        return None;
    }
    let text_block = |text: Vec<u8>| {
        let mut block = br#"{"type":"text","text":""}"#.to_vec();
        gj::set_str(&mut block, "text", text);
        block
    };
    let mut changed = false;
    let rewritten: Vec<Vec<u8>> = messages
        .array()
        .iter()
        .map(|message| {
            let content = message.get("content");
            if !content.is_array() {
                return message.raw().to_vec();
            }
            let mut flattened = false;
            let parts: Vec<Vec<u8>> = content
                .array()
                .iter()
                .map(|part| match &*part.get("type").bytes() {
                    b"tool_use" => {
                        flattened = true;
                        text_block(
                            [
                                b"Tool call ".as_slice(),
                                &part.get("name").bytes(),
                                b" (",
                                &part.get("id").bytes(),
                                b"): ",
                                part.get("input").raw(),
                            ]
                            .concat(),
                        )
                    }
                    b"tool_result" => {
                        flattened = true;
                        let result = part.get("content");
                        let result = if result.kind == gj::Kind::String {
                            result.bytes().into_owned()
                        } else {
                            result.raw().to_vec()
                        };
                        text_block(
                            [
                                b"Tool result ".as_slice(),
                                &part.get("tool_use_id").bytes(),
                                b": ",
                                &result,
                            ]
                            .concat(),
                        )
                    }
                    _ => part.raw().to_vec(),
                })
                .collect();
            if !flattened {
                return message.raw().to_vec();
            }
            changed = true;
            let mut message = message.raw().to_vec();
            gj::set_raw(&mut message, "content", gj::join(&parts));
            message
        })
        .collect();
    changed.then(|| {
        let mut out = body.to_vec();
        gj::set_raw(&mut out, "messages", gj::join(&rewritten));
        out
    })
}

/// Responses usage of a Claude message (`claudeCompactionResponsesUsage`): input
/// counts cache writes and reads, `cached_tokens` the reads, total input plus output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Usage {
    input: i64,
    output: i64,
    total: i64,
    cached: i64,
}

impl Usage {
    pub(super) fn of(message: &[u8]) -> Self {
        let usage = gj::get(message, "usage");
        if !usage.exists() {
            // Go falls back to ParseOpenAIUsage, which finds no usage either.
            return Self {
                input: 0,
                output: 0,
                total: 0,
                cached: 0,
            };
        }
        let field = |path: &str| usage.get(path).int();
        let cached = field("cache_read_input_tokens");
        let input = field("input_tokens") + field("cache_creation_input_tokens") + cached;
        let output = field("output_tokens");
        Self {
            input,
            output,
            total: input + output,
            cached,
        }
    }

    fn counts(self) -> (i64, i64, i64) {
        (self.input, self.output, self.total)
    }
}

/// The non-stream `/responses/compact` answer.
pub(super) fn response(model: &str, sealed: &str, usage: Usage) -> Vec<u8> {
    let mut out = capsule::response(model, sealed, usage.counts());
    gj::set_int(&mut out, "usage.input_tokens_details.cached_tokens", usage.cached);
    out
}

/// The streamed answer: Antigravity's five events with the Claude usage
/// (`patchClaudeCompactionStreamUsage`).
pub(super) fn stream_chunks(model: &str, sealed: &str, usage: Usage) -> Vec<Vec<u8>> {
    capsule::stream_chunks(model, sealed, usage.counts())
        .into_iter()
        .map(|chunk| patch_stream_usage(chunk, usage))
        .collect()
}

fn patch_stream_usage(chunk: Vec<u8>, usage: Usage) -> Vec<u8> {
    const PREFIX: &[u8] = b"data: ";
    let Some(at) = chunk.windows(PREFIX.len()).position(|w| w == PREFIX) else {
        return chunk;
    };
    let mut data = chunk[at + PREFIX.len()..].trim_ascii().to_vec();
    let path = if gj::get(&data, "response.usage").exists() {
        "response.usage"
    } else if gj::get(&data, "usage").exists() {
        "usage"
    } else {
        return chunk;
    };
    gj::set_int(&mut data, &format!("{path}.input_tokens"), usage.input);
    gj::set_int(&mut data, &format!("{path}.output_tokens"), usage.output);
    gj::set_int(&mut data, &format!("{path}.total_tokens"), usage.total);
    gj::set_int(
        &mut data,
        &format!("{path}.input_tokens_details.cached_tokens"),
        usage.cached,
    );
    [&chunk[..at], PREFIX, &data, b"\n\n"].concat()
}
