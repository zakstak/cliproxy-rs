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
//! `Value` member raw and checks it for what the first-party build's `Value` refuses
//! beyond JSON syntax ([`value_decodes`]). That build enables serde_json's
//! `arbitrary_precision` and `raw_value` (`codex-rs/exec-server-protocol/Cargo.toml:25`,
//! unified into `codex-cli` through `codex-exec-server`; `Cargo.lock` pins serde_json
//! 1.0.149). This build keeps its own serde_json features: enabling those two here would
//! change every provider's numbers, so the boundary checks their effect on `Value`
//! instead. Only a decoded event can end the response, fail it, be converted (#5) or feed
//! usage. Every other event passes as sent, and the stream goes on. Nothing here changes
//! a byte that reaches the client, and nothing is re-serialized for delivery.
//!
//! A decoded event ends the response only when the pinned reader ends it on that event
//! ([`Event::terminal`]); a decoded `response.completed` without a `response`, or a
//! `response.done`, which the pinned reader does not handle, ends nothing.
//!
//! The WebSocket reader has one more reading (`endpoint/responses_websocket.rs:864-884`).
//! A text frame that does not decode, or whose `type` is `error`, can still be a wrapped
//! status error (`WrappedWebsocketErrorEvent`, lines 684-709), which the reader maps to an
//! error or to nothing (lines 712-764). [`wrapped`] makes both decisions.
//!
//! Failure classification reads a decoded event as the pinned `Value` does: with the last
//! of a repeated key ([`last_wins`]). The client still receives the bytes as sent.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::ops::Range;

use serde::Deserialize;
use serde::de::{Deserializer, IgnoredAny, MapAccess, Visitor};
use serde_json::value::RawValue;

/// An upstream event the pinned client decodes: its `type`, and its `response` and
/// `error` members as sent (a JSON `null` counts as absent, as for an `Option<Value>`).
pub struct Event<'a> {
    pub kind: Cow<'a, str>,
    pub response: Option<&'a str>,
    pub error: Option<&'a str>,
}

impl Event<'_> {
    /// Whether the response ends on this event (`process_responses_event`,
    /// `sse/responses.rs:375-562`, and the readers that call it, lines 645-692 and
    /// `endpoint/responses_websocket.rs:946-995`):
    ///
    /// - `response.completed` with a `response`. Without one (missing or `null`) the
    ///   pinned reader returns no event and keeps reading.
    /// - Every `response.incomplete`, which the contract keeps as the provider's terminal
    ///   event (#25): an interrupted one completes the turn, a content filter is its own
    ///   error, and any other reason, `unknown` when there is no `response`, is a
    ///   terminal `ResponseIncomplete`.
    ///
    /// `response.done` is not among them: the pinned reader does not handle it.
    /// Failures (`response.failed`, `error`) are classified separately.
    ///
    /// ponytail: a `response` the pinned reader cannot read as `ResponseCompleted` (no
    /// string `id`, a malformed `usage`), and a content-filter or unreadable interrupted
    /// `response.incomplete`, end the WebSocket reader's turn with an error; the SSE
    /// reader records that error and reads on until the stream ends. The proxy ends the
    /// response on them on both transports, so a further terminal event after one is not
    /// forwarded over HTTP. Matching it needs the SSE route to hold a recorded error until
    /// the upstream closes, and copies of `ResponseCompleted`'s nested types.
    pub fn terminal(&self) -> bool {
        match self.kind.as_ref() {
            "response.completed" => self.response.is_some(),
            "response.incomplete" => true,
            _ => false,
        }
    }

    /// Whether usage observation reads this event. The shared usage reader takes the
    /// first `response.completed`, `response.incomplete` or `response.done` as the
    /// response's end (Go's Codex executor publishes that event's usage). Two of those
    /// end nothing for the pinned reader ([`Event::terminal`]): `response.done` and a
    /// `response.completed` without a `response`. Usage skips them, so the record keeps
    /// the usage of the event the response does end on.
    pub fn accounted(&self) -> bool {
        match self.kind.as_ref() {
            "response.done" => false,
            "response.completed" => self.response.is_some(),
            _ => true,
        }
    }

    /// The `response.id` the pinned reader reads (`Value::get`, the last `id`), when it
    /// is a string.
    pub fn response_id(&self) -> Option<String> {
        let id = member(self.response?, "id")?;
        serde_json::from_str::<String>(id).ok()
    }
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

/// The deepest nesting `serde_json` decodes into a `Value`, counting the outermost
/// object or array of the text it parses: its recursion budget of 128 runs out at the
/// 128th level.
const MAX_DEPTH: usize = 127;

/// The level of a `ResponsesStreamEvent` member's value: inside the event's object.
const MEMBER_LEVEL: usize = 2;

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
    if !values
        .iter()
        .flatten()
        .all(|raw| value_decodes(raw.get(), MEMBER_LEVEL, true))
    {
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
struct WrappedShape<'a> {
    #[serde(rename = "type", borrow)]
    kind: Cow<'a, str>,
    #[serde(alias = "status_code")]
    status: Option<u16>,
    #[serde(default, borrow)]
    error: Option<WrappedErrorShape<'a>>,
    #[serde(default, borrow)]
    headers: Option<&'a RawValue>,
}

/// `WrappedWebsocketError`'s members.
#[derive(Deserialize)]
#[allow(dead_code)]
struct WrappedErrorShape<'a> {
    #[serde(borrow)]
    code: Option<Cow<'a, str>>,
    message: Option<Text>,
    #[serde(default, borrow)]
    headers: Option<&'a RawValue>,
}

/// How the pinned WebSocket reader maps a frame that decodes as a wrapped status error
/// (`map_wrapped_websocket_error_event`, `endpoint/responses_websocket.rs:712-764`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Wrapped {
    /// `error.code` is `websocket_connection_limit_reached` or
    /// `previous_response_not_found`: a retryable error, whatever the status says. The
    /// status, when it is an [`error_status`].
    Retryable(Option<u16>),
    /// An [`error_status`]: an HTTP error with it.
    Status(u16),
    /// A status that maps to no error: a success (2xx) or not an HTTP status. The reader
    /// goes on as if the frame were not wrapped.
    Notice,
    /// Neither a status nor a recognised code: no error either.
    Statusless,
}

/// The pinned WebSocket reader's wrapped reading of the text frame `payload`: `None`
/// unless it decodes as `WrappedWebsocketErrorEvent` with `type` `error` (`status` or
/// `status_code` a `u16` if present, `error` and `headers` of their pinned types:
/// `headers` an object of `Value`s, `error.headers` a `Value`).
pub(crate) fn wrapped(payload: &str) -> Option<Wrapped> {
    let shape = serde_json::from_str::<WrappedShape<'_>>(payload).ok()?;
    if shape.kind != "error" {
        return None;
    }
    // `headers` is a `Map<String, Value>`, which reads its own keys plainly: only the
    // values inside are `Value`s. `error.headers` is a `Value` inside the `error` struct.
    let headers = shape.headers.map(RawValue::get);
    if !headers.is_none_or(|h| h.starts_with('{') && value_decodes(h, MEMBER_LEVEL, false)) {
        return None;
    }
    let code = shape.error.as_ref().and_then(|e| e.code.as_deref());
    if let Some(h) = shape.error.as_ref().and_then(|e| e.headers)
        && !value_decodes(h.get(), MEMBER_LEVEL + 1, true)
    {
        return None;
    }
    let status = shape.status.filter(|s| error_status(*s));
    Some(
        if matches!(
            code,
            Some("websocket_connection_limit_reached" | "previous_response_not_found")
        ) {
            Wrapped::Retryable(status)
        } else {
            match (shape.status, status) {
                (None, _) => Wrapped::Statusless,
                (Some(_), Some(status)) => Wrapped::Status(status),
                (Some(_), None) => Wrapped::Notice,
            }
        },
    )
}

/// A status the pinned mapping turns into an HTTP error: one `http::StatusCode` accepts
/// (100 to 999) that is not a success (2xx).
pub(crate) fn error_status(status: u16) -> bool {
    (100..=999).contains(&status) && !(200..=299).contains(&status)
}

/// serde_json's private keys that make an object decode as something else when they are
/// its first key (`value/de.rs`, `KeyClassifier`): a number written as a string
/// (`arbitrary_precision`), and JSON text written as a string (`raw_value`).
const NUMBER_TOKEN: &str = "$serde_json::private::Number";
const RAW_VALUE_TOKEN: &str = "$serde_json::private::RawValue";

/// Whether `raw`, one valid JSON value whose outermost container would sit at nesting
/// `level`, also decodes as the first-party build's `serde_json::Value`. Beyond syntax,
/// that `Value` refuses:
///
/// - nesting past [`MAX_DEPTH`];
/// - a `\u` escape of a surrogate outside a pair, in a string or a key;
/// - an object whose first key is [`NUMBER_TOKEN`] unless it has that one member and
///   its value is a string holding exactly one JSON number;
/// - an object whose first key is [`RAW_VALUE_TOKEN`] unless it has that one member and
///   its value is a string holding JSON text that itself decodes as a `Value`, with a
///   fresh recursion budget.
///
/// A number of any size counts (`arbitrary_precision`). `classify_root` is false for a
/// `Map<String, Value>`, whose own keys are plain. One pass over `raw`; the JSON text of
/// a [`RAW_VALUE_TOKEN`] string is decoded once and passed over once more.
fn value_decodes(raw: &str, level: usize, classify_root: bool) -> bool {
    let b = raw.as_bytes();
    let mut depth = level - 1;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'[' => {
                depth += 1;
                if depth > MAX_DEPTH {
                    return false;
                }
                i += 1;
            }
            b'{' => {
                depth += 1;
                if depth > MAX_DEPTH {
                    return false;
                }
                i += 1;
                if depth == level && !classify_root {
                    continue;
                }
                match private_object(raw, i) {
                    Private::No => {}
                    Private::Refused => return false,
                    // The object ended with its one member.
                    Private::Accepted(end) => {
                        depth -= 1;
                        i = end;
                    }
                }
            }
            b']' | b'}' => {
                depth -= 1;
                i += 1;
            }
            b'"' => match string_end(b, i + 1) {
                Some(end) => i = end + 1,
                None => return false,
            },
            _ => i += 1,
        }
    }
    true
}

enum Private {
    /// The object's first key is an ordinary one, or it has none.
    No,
    /// The object decodes as the private value; it ends just before the index.
    Accepted(usize),
    Refused,
}

/// The object whose members start at `start` (just after its `{`), read as
/// `KeyClassifier` reads its first key.
fn private_object(raw: &str, start: usize) -> Private {
    let b = raw.as_bytes();
    let key_at = skip_space(b, start);
    if b.get(key_at) != Some(&b'"') {
        return Private::No;
    }
    let Some(key_end) = string_end(b, key_at + 1) else {
        return Private::Refused;
    };
    let number = match unescape(&raw[key_at..=key_end]).as_deref() {
        Some(NUMBER_TOKEN) => true,
        Some(RAW_VALUE_TOKEN) => false,
        Some(_) => return Private::No,
        None => return Private::Refused,
    };
    // Valid JSON: a colon, then the value.
    let value_at = skip_space(b, skip_space(b, key_end + 1) + 1);
    if b.get(value_at) != Some(&b'"') {
        return Private::Refused;
    }
    let Some(value_end) = string_end(b, value_at + 1) else {
        return Private::Refused;
    };
    let Some(text) = unescape(&raw[value_at..=value_end]) else {
        return Private::Refused;
    };
    let accepted = if number {
        json_number(text.as_bytes())
    } else {
        serde_json::from_str::<IgnoredAny>(&text).is_ok() && value_decodes(&text, 1, true)
    };
    // The private value is the whole object: anything after it is refused.
    let close = skip_space(b, value_end + 1);
    if accepted && b.get(close) == Some(&b'}') {
        Private::Accepted(close + 1)
    } else {
        Private::Refused
    }
}

/// The contents of the JSON string `quoted` (quotes included).
fn unescape(quoted: &str) -> Option<Cow<'_, str>> {
    let inner = &quoted[1..quoted.len() - 1];
    if inner.contains('\\') {
        serde_json::from_str::<String>(quoted).ok().map(Cow::Owned)
    } else {
        Some(Cow::Borrowed(inner))
    }
}

fn skip_space(b: &[u8], mut i: usize) -> usize {
    while matches!(b.get(i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        i += 1;
    }
    i
}

/// Whether `s` is exactly one JSON number, as serde_json's `Number::from_str` reads it
/// (`de.rs`, `parse_any_signed_number`): no sign but a leading `-`, no space, no
/// leading zero before a digit, digits after a `.` and after an exponent.
fn json_number(s: &[u8]) -> bool {
    let digits = |i: &mut usize| {
        let start = *i;
        while matches!(s.get(*i), Some(b'0'..=b'9')) {
            *i += 1;
        }
        *i > start
    };
    let mut i = usize::from(s.first() == Some(&b'-'));
    match s.get(i) {
        Some(b'0') => i += 1,
        Some(b'1'..=b'9') => {
            digits(&mut i);
        }
        _ => return false,
    }
    if s.get(i) == Some(&b'.') {
        i += 1;
        if !digits(&mut i) {
            return false;
        }
    }
    if matches!(s.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(s.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if !digits(&mut i) {
            return false;
        }
    }
    i == s.len()
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

/// The raw value of the last `name` member of the JSON object `object`, as
/// `Value::get` reads it; `None` when `object` is not an object or has no such member.
pub(crate) fn member<'a>(object: &'a str, name: &str) -> Option<&'a str> {
    struct Last<'n>(&'n str);
    impl<'de, 'n> Visitor<'de> for Last<'n> {
        type Value = Option<&'de RawValue>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an object")
        }
        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
            let mut found = None;
            while let Some(key) = map.next_key::<Cow<'de, str>>()? {
                if key == self.0 {
                    found = Some(map.next_value::<&'de RawValue>()?);
                } else {
                    map.next_value::<IgnoredAny>()?;
                }
            }
            Ok(found)
        }
    }
    let mut de = serde_json::Deserializer::from_str(object);
    de.deserialize_map(Last(name)).ok().flatten().map(RawValue::get)
}

/// `payload` as the pinned client's `serde_json::Value` holds it, for classification
/// only: every object keeps the last of a repeated key (compared after unescaping), and
/// each earlier occurrence is cut out with its separator. Nothing else changes: no byte
/// is re-spelled. `payload` itself when no object repeats a key, and when it is not valid
/// JSON. One validating pass and one scan, with a key table per object.
pub(crate) fn last_wins(payload: &str) -> Cow<'_, str> {
    if serde_json::from_str::<IgnoredAny>(payload).is_err() {
        return Cow::Borrowed(payload);
    }
    #[derive(Default)]
    struct Object<'a> {
        /// Where each member starts (its key's opening quote).
        members: Vec<usize>,
        keys: HashMap<Cow<'a, str>, usize>,
    }
    let b = payload.as_bytes();
    let mut cut: Vec<Range<usize>> = Vec::new();
    // `None` for an array.
    let mut stack: Vec<Option<Object<'_>>> = Vec::new();
    let mut key_next = false;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'{' => {
                stack.push(Some(Object::default()));
                key_next = true;
            }
            b'[' => {
                stack.push(None);
                key_next = false;
            }
            b'}' | b']' => {
                stack.pop();
                key_next = false;
            }
            b',' => key_next = matches!(stack.last(), Some(Some(_))),
            b'"' => {
                let end = quote_end(b, i + 1);
                if key_next && let Some(Some(object)) = stack.last_mut() {
                    let quoted = &payload[i..=end];
                    let key = unescape(quoted).unwrap_or(Cow::Borrowed(quoted));
                    let index = object.members.len();
                    object.members.push(i);
                    if let Some(earlier) = object.keys.insert(key, index) {
                        cut.push(object.members[earlier]..object.members[earlier + 1]);
                    }
                }
                key_next = false;
                i = end;
            }
            _ => {}
        }
        i += 1;
    }
    if cut.is_empty() {
        return Cow::Borrowed(payload);
    }
    cut.sort_by_key(|r| r.start);
    let mut out = String::with_capacity(payload.len());
    let mut at = 0;
    for range in cut {
        // A cut inside one already made goes with it.
        if range.start < at {
            continue;
        }
        out.push_str(&payload[at..range.start]);
        at = range.end;
    }
    out.push_str(&payload[at..]);
    Cow::Owned(out)
}

/// The index of the quote closing the string whose contents start at `i`, in valid JSON.
fn quote_end(b: &[u8], mut i: usize) -> usize {
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return i,
            _ => i += 1,
        }
    }
    b.len() - 1
}

#[cfg(test)]
#[path = "codex_event_tests.rs"]
mod tests;
