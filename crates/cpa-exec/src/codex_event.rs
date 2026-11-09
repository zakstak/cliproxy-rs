//! The upstream event boundary of the native Codex routes (#25): which upstream
//! Responses events can change what the proxy does, on HTTP and on the WebSocket.
//!
//! The pinned client decodes every event into `ResponsesStreamEvent` before anything
//! reads it, and skips an event that does not decode (`codex-api/src/sse/responses.rs`
//! lines 161-179 and 633-643 at 44ccf394573c). The struct is serde-derived without
//! `deny_unknown_fields`. An event decodes when it is one JSON value whose known members
//! (`type`, `headers`, `metadata`, `response`, `response_id`, `error`, `item`, `item_id`,
//! `call_id`, `delta`, `text`, `summary_index`, `content_index`, `safety_buffering`) each
//! appear at most once, with the type the struct gives them, and whose `type` is a
//! string. Unknown members are ignored, repeated or not. Inside a known member's value,
//! `serde_json::Value` keeps the last of a repeated key.
//!
//! [`decode`] makes the same decision with the same serde-derived shape. It borrows each
//! `Value` member raw and checks it for what `Value` refuses beyond JSON syntax: nesting
//! past serde_json's recursion limit and an unpaired surrogate escape. A number of any
//! size counts. The first-party binary builds serde_json with `arbitrary_precision`,
//! which `codex-rs/exec-server-protocol/Cargo.toml:25` enables and Cargo unifies into
//! `codex-cli` through `codex-exec-server`. Only a decoded event can end the response,
//! fail it, be converted (#5) or feed usage. Every other event passes as sent, and the
//! stream goes on. Nothing here changes a byte, and nothing is re-serialized.
//!
//! The WebSocket reader has one more reading (`endpoint/responses_websocket.rs:864-884`).
//! A text frame that does not decode, or whose `type` is `error`, can still be a wrapped
//! status error (`WrappedWebsocketErrorEvent`, lines 684-709). `wrapped_error` decides
//! whether a frame has that shape.

use std::borrow::Cow;
use std::fmt;

use serde::Deserialize;
use serde::de::{Deserializer, Visitor};
use serde_json::value::RawValue;

/// An upstream event the pinned client decodes: its `type`, and its `response` and
/// `error` members as sent (a JSON `null` counts as absent, as for an `Option<Value>`).
pub struct Event<'a> {
    pub kind: Cow<'a, str>,
    pub response: Option<&'a str>,
    pub error: Option<&'a str>,
}

/// `ResponsesStreamEvent`'s members, in its order and with its types. Serde also decodes
/// a struct from a JSON array, by position, so the order matters as well.
#[derive(Deserialize)]
#[allow(dead_code)] // Members are decoded for their acceptance, not read.
struct Shape<'a> {
    #[serde(rename = "type", borrow)]
    kind: Cow<'a, str>,
    #[serde(borrow)]
    headers: Option<&'a RawValue>,
    #[serde(borrow)]
    metadata: Option<&'a RawValue>,
    #[serde(borrow)]
    response: Option<&'a RawValue>,
    #[serde(borrow)]
    response_id: Option<&'a RawValue>,
    #[serde(borrow)]
    error: Option<&'a RawValue>,
    #[serde(borrow)]
    item: Option<&'a RawValue>,
    item_id: Option<Text>,
    call_id: Option<Text>,
    delta: Option<Text>,
    text: Option<Text>,
    summary_index: Option<i64>,
    content_index: Option<i64>,
    #[serde(default, borrow)]
    safety_buffering: Option<&'a RawValue>,
}

/// A `String` member: any JSON string, read and dropped. Like `String`, it refuses an
/// unpaired surrogate escape.
struct Text;

impl<'de> Deserialize<'de> for Text {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Any;
        impl Visitor<'_> for Any {
            type Value = Text;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a string")
            }
            fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Text, E> {
                Ok(Text)
            }
        }
        deserializer.deserialize_string(Any)
    }
}

/// The deepest nesting `serde_json` decodes into a `Value`, counting the event's own
/// object or array: its recursion budget of 128 runs out at the 128th level.
const MAX_DEPTH: usize = 127;

/// The event in `payload`, an SSE `data` field as received, if the pinned client decodes
/// it.
pub fn decode(payload: &str) -> Option<Event<'_>> {
    let shape: Shape<'_> = serde_json::from_str(payload).ok()?;
    let values = [
        shape.headers,
        shape.metadata,
        shape.response,
        shape.response_id,
        shape.error,
        shape.item,
        shape.safety_buffering,
    ];
    if !values.iter().flatten().all(|raw| value_decodes(raw.get().as_bytes())) {
        return None;
    }
    Some(Event {
        kind: shape.kind,
        response: shape.response.map(RawValue::get),
        error: shape.error.map(RawValue::get),
    })
}

/// `WrappedWebsocketErrorEvent`'s members: `status` also answers to `status_code`, and
/// the two together are a repeated member.
#[derive(Deserialize)]
#[allow(dead_code)]
struct Wrapped<'a> {
    #[serde(rename = "type", borrow)]
    kind: Cow<'a, str>,
    #[serde(alias = "status_code")]
    status: Option<u16>,
    #[serde(default, borrow)]
    error: Option<WrappedError<'a>>,
    #[serde(default, borrow)]
    headers: Option<&'a RawValue>,
}

/// `WrappedWebsocketError`'s members.
#[derive(Deserialize)]
#[allow(dead_code)]
struct WrappedError<'a> {
    code: Option<Text>,
    message: Option<Text>,
    #[serde(default, borrow)]
    headers: Option<&'a RawValue>,
}

/// Whether the WebSocket text frame `payload` decodes as the pinned reader's wrapped
/// status error: `type` `error`, `status` (or `status_code`) a `u16` if present, and the
/// `error` and `headers` members of their pinned types. The `headers` member must be an
/// object. Whether the reader then maps it to an error depends on the values, which
/// `codex_ws::ws_error`'s classification reads.
pub(crate) fn wrapped_error(payload: &str) -> bool {
    let Ok(wrapped) = serde_json::from_str::<Wrapped<'_>>(payload) else {
        return false;
    };
    let headers = wrapped.headers.map(RawValue::get);
    let error_headers = wrapped.error.as_ref().and_then(|e| e.headers).map(RawValue::get);
    wrapped.kind == "error"
        && headers.is_none_or(|h| h.starts_with('{'))
        && [headers, error_headers]
            .into_iter()
            .flatten()
            .all(|raw| value_decodes(raw.as_bytes()))
}

/// Whether `raw`, one member's valid JSON value, also decodes as a `serde_json::Value`
/// at the second level of an event: no deeper than [`MAX_DEPTH`], and every `\u` escape
/// of a surrogate in a pair. One pass over `raw`.
fn value_decodes(raw: &[u8]) -> bool {
    let mut depth = 1;
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            b'[' | b'{' => {
                depth += 1;
                if depth > MAX_DEPTH {
                    return false;
                }
            }
            b']' | b'}' => depth -= 1,
            b'"' => match string_end(raw, i + 1) {
                Some(end) => i = end,
                None => return false,
            },
            _ => {}
        }
        i += 1;
    }
    true
}

/// The index of the quote that closes the valid JSON string starting at `i`, or `None`
/// when one of its escapes is a surrogate without its pair, which `serde_json` refuses
/// for a string or key of a `Value`.
fn string_end(raw: &[u8], mut i: usize) -> Option<usize> {
    let unit = |at: usize| -> Option<u16> {
        let hex = raw.get(at..at + 4)?;
        u16::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()
    };
    while i < raw.len() {
        match raw[i] {
            b'"' => return Some(i),
            b'\\' if raw.get(i + 1) == Some(&b'u') => {
                let n = unit(i + 2)?;
                i += 6;
                if (0xDC00..=0xDFFF).contains(&n) {
                    return None;
                }
                if (0xD800..=0xDBFF).contains(&n) {
                    if raw.get(i..i + 2) != Some(b"\\u") {
                        return None;
                    }
                    if !(0xDC00..=0xDFFF).contains(&unit(i + 2)?) {
                        return None;
                    }
                    i += 6;
                }
            }
            b'\\' => i += 2,
            _ => i += 1,
        }
    }
    None
}

#[cfg(test)]
#[path = "codex_event_tests.rs"]
mod tests;
