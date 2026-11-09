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
//! ([`Event::outcome`]); a decoded `response.completed` without a `response`, or a
//! `response.done`, which the pinned reader does not handle, ends nothing. The two
//! pinned readers part on a processing error, such as a `response` that does not read as
//! `ResponseCompleted` or a content filter ([`Outcome::Recorded`]): the SSE reader reads
//! on, the WebSocket reader ends the turn. Each route follows the reader of its client.
//!
//! The WebSocket reader has one more reading (`endpoint/responses_websocket.rs:864-884`).
//! A text frame that does not decode, or whose `type` is `error`, can still be a wrapped
//! status error (`WrappedWebsocketErrorEvent`, lines 684-709), which the reader maps to an
//! error or to nothing (lines 712-764). [`wrapped`] makes both decisions.
//!
//! Failure classification and response identity read a decoded event as the pinned
//! `Value` does: with the last of a repeated key and its private numbers and raw values
//! resolved ([`pinned_view`], [`Event::response_id`]). The client still receives the
//! bytes as sent.

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

/// What the pinned readers do on a decoded event (`process_responses_event`,
/// `sse/responses.rs:375-562` at 44ccf394573c, and its two callers). Only a
/// `response.completed` or a `response.incomplete` can end a response; failures
/// (`response.failed`, `error`) are classified separately.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The reader goes on: no event, or one that ends nothing. A `response.completed`
    /// without a `response` (missing or `null`), any other kind, and `response.done`,
    /// which the reader does not handle.
    Continues,
    /// Both readers end the response here: a `response` that reads as
    /// `ResponseCompleted` ([`completes`]) on a `response.completed` or an interrupted
    /// `response.incomplete` (`Completed`), or a `response.incomplete` with any reason
    /// but those two, `unknown` when there is none (a `ResponseIncomplete` error, which
    /// the SSE reader returns at once, lines 680-688).
    Ends,
    /// A processing error: a `response` that does not read as `ResponseCompleted` on a
    /// `response.completed` or an interrupted `response.incomplete` (`Stream`, lines
    /// 515-519), or a content filter (`ContentFilter`, lines 461-463). The two readers
    /// part here. The SSE reader records it and reads on (lines 677-690): a later
    /// terminal event ends the response, and at the end of the stream it reports the
    /// last error it recorded (lines 616-621). The WebSocket reader returns it, which
    /// ends the turn and retires the socket (`endpoint/responses_websocket.rs:968-978`
    /// and 387-395).
    Recorded,
}

impl Outcome {
    /// Whether a WebSocket client's turn ends on the event: on a terminal event and on a
    /// processing error alike.
    pub fn ends_turn(self) -> bool {
        self != Self::Continues
    }
}

impl Event<'_> {
    /// What the pinned readers do on this event ([`Outcome`]). The members it reads are
    /// read as the pinned `Value` holds them: the last of a repeated key, with the
    /// private tokens resolved ([`resolved`]).
    pub fn outcome(&self) -> Outcome {
        let completion = |response: Option<&str>| match response {
            None => Outcome::Continues,
            Some(response) if completes(response) => Outcome::Ends,
            Some(_) => Outcome::Recorded,
        };
        match self.kind.as_ref() {
            "response.completed" => completion(self.response),
            "response.incomplete" => {
                let reason = self
                    .response
                    .and_then(|response| get(response, "incomplete_details"))
                    .and_then(|details| get(&details, "reason"))
                    .and_then(|reason| serde_json::from_str::<String>(&reason).ok());
                match reason.as_deref().unwrap_or("unknown") {
                    "content_filter" => Outcome::Recorded,
                    // The reason came from the `response`, so there is one.
                    "interrupted" => completion(self.response),
                    _ => Outcome::Ends,
                }
            }
            _ => Outcome::Continues,
        }
    }

    /// Whether usage observation reads this event. The shared usage reader takes the
    /// first `response.completed`, `response.incomplete` or `response.done` as the
    /// response's end (Go's Codex executor publishes that event's usage). Two of those
    /// end nothing for the pinned reader ([`Outcome::Continues`]): `response.done` and a
    /// `response.completed` without a `response`. Usage skips them, so the record keeps
    /// the usage of the event the response does end on. A route whose client reads past
    /// a processing error ([`Outcome::Recorded`]) holds that event's usage until it knows
    /// no later terminal event follows (`codex_native.rs`).
    pub fn accounted(&self) -> bool {
        match self.kind.as_ref() {
            "response.done" => false,
            "response.completed" => self.response.is_some(),
            _ => true,
        }
    }

    /// The `response.id` the pinned reader reads (`Value::get`, the last `id`, with the
    /// private values of the `response` and of the `id` resolved), when it is a string.
    /// Every native route names a response by it: settlement, usage ownership and
    /// interrupt tracking.
    pub fn response_id(&self) -> Option<String> {
        if self.kind == "response.completed" {
            let response = resolved(self.response?);
            if response.trim_start().starts_with('[') && completes(&response) {
                // ResponseCompleted is a serde struct: its positional form names the
                // completed response in the first element, unlike response.created's
                // object-member reading (sse/responses.rs:109-118,494-520).
                let fields: Vec<&RawValue> = serde_json::from_str(&response).ok()?;
                return serde_json::from_str::<String>(&resolved(fields.first()?.get())).ok();
            }
        }
        self.response_str("id")
    }

    /// The string member `key` of `response`, as [`Event::response_id`] reads `id`.
    pub(crate) fn response_str(&self, key: &str) -> Option<String> {
        let value = get(self.response?, key)?;
        serde_json::from_str::<String>(&value).ok()
    }

    /// The string member of `error` as the pinned `Value` reads it, including private
    /// values. A wrapped status notice can still carry a terminal provider error.
    pub(crate) fn error_str(&self, key: &str) -> Option<String> {
        let value = get(self.error?, key)?;
        serde_json::from_str::<String>(&value).ok()
    }

    /// A context failure preserved by the executor. The relay must use the same
    /// logical reading before delivering it and consuming its classified stream error.
    pub fn is_context_window_failure(&self) -> bool {
        self.kind == "response.failed"
            && self
                .response
                .and_then(|response| get(response, "error"))
                .and_then(|error| get(&error, "code"))
                .and_then(|code| serde_json::from_str::<String>(&code).ok())
                .as_deref()
                == Some("context_length_exceeded")
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

/// `raw`, one JSON value of a decoded event, as the first-party `Value` holds it: an
/// object that is a private number (`arbitrary_precision`) is that number, and one that
/// is a private raw value (`raw_value`) is the JSON text it holds, read in turn
/// (`KeyClassifier`, `value/de.rs`). [`decode`] already refused every malformed one, so
/// an object whose first key is a private token is exactly one of them. `raw` itself,
/// trimmed, for any other value.
fn resolved(raw: &str) -> Cow<'_, str> {
    let mut text = Cow::Borrowed(raw.trim());
    while let Some(inner) = private_text(&text) {
        text = Cow::Owned(inner.trim().to_owned());
    }
    text
}

/// The string a private-token object holds: the number's digits, or the raw value's
/// JSON text. `None` for any other value.
fn private_text(raw: &str) -> Option<String> {
    let b = raw.as_bytes();
    if b.first() != Some(&b'{') {
        return None;
    }
    let key_at = skip_space(b, 1);
    if b.get(key_at) != Some(&b'"') {
        return None;
    }
    let key_end = string_end(b, key_at + 1)?;
    if !matches!(
        unescape(&raw[key_at..=key_end]).as_deref(),
        Some(NUMBER_TOKEN | RAW_VALUE_TOKEN)
    ) {
        return None;
    }
    let value_at = skip_space(b, skip_space(b, key_end + 1) + 1);
    let value_end = string_end(b, value_at + 1)?;
    unescape(&raw[value_at..=value_end]).map(Cow::into_owned)
}

/// The member `key` of the object `raw` as the pinned `Value::get` reads it: the last
/// one, with the private tokens of both resolved. `None` when `raw` is no object or has
/// no such member.
fn get(raw: &str, key: &str) -> Option<String> {
    let object = resolved(raw);
    member(&object, key).map(|value| resolved(value).into_owned())
}

/// Whether the pinned reader reads `response`, a decoded event's raw `response` member,
/// as `ResponseCompleted` (`sse/responses.rs:494-520`).
pub(crate) fn completes(response: &str) -> bool {
    Completed::reads(&resolved(response))
}

/// One first-party type, reading one JSON value: `text`, with its own private tokens
/// already resolved ([`resolved`]).
trait Reads {
    fn reads(text: &str) -> bool;
}

/// A derived struct, as `serde_json::from_value` reads it from the pinned `Value`: an
/// object with the last of a repeated key ([`pinned_view`]; `from_str` refuses a repeated
/// field, the `Value` keeps the last), unknown members ignored, a missing `Option` as
/// `None`, and a JSON array by position. The members are checked by [`Member`] and
/// [`Optional`].
fn struct_reads<T: serde::de::DeserializeOwned>(text: &str) -> bool {
    serde_json::from_str::<T>(&pinned_view(text)).is_ok()
}

/// `ResponseCompleted` (`sse/responses.rs:109-118`).
#[derive(Deserialize)]
#[allow(dead_code)] // Members are decoded for their acceptance, not read.
struct Completed {
    id: Member<Str>,
    #[serde(default)]
    usage: Optional<Usage>,
    usage_metadata: Optional<UsageMetadata>,
    #[serde(default)]
    end_turn: Optional<Bool>,
}

/// `ResponseCompletedUsage` (lines 120-129).
#[derive(Deserialize)]
#[allow(dead_code)]
struct Usage {
    input_tokens: Member<I64>,
    input_tokens_details: Optional<InputDetails>,
    output_tokens: Member<I64>,
    output_tokens_details: Optional<OutputDetails>,
    total_tokens: Member<I64>,
    #[serde(default)]
    codex_rollout_budget_units: Optional<Number>,
}

/// `ResponseCompletedInputTokensDetails` (lines 149-154).
#[derive(Deserialize)]
#[allow(dead_code)]
struct InputDetails {
    cached_tokens: Member<I64>,
    #[serde(default)]
    cache_write_tokens: Member<I64>,
}

/// `ResponseCompletedOutputTokensDetails` (lines 156-159).
#[derive(Deserialize)]
#[allow(dead_code)]
struct OutputDetails {
    reasoning_tokens: Member<I64>,
}

/// `ResponseUsageMetadata` (`protocol/src/response_usage.rs:10-14`).
#[derive(Deserialize)]
#[allow(dead_code)]
struct UsageMetadata {
    amount: Optional<Str>,
    metadata: Optional<AnyValue>,
}

macro_rules! struct_reads {
    ($($t:ty),*) => {$(
        impl Reads for $t {
            fn reads(text: &str) -> bool {
                struct_reads::<$t>(text)
            }
        }
    )*};
}
struct_reads!(Completed, Usage, InputDetails, OutputDetails, UsageMetadata);

/// A `String`.
struct Str;
impl Reads for Str {
    fn reads(text: &str) -> bool {
        serde_json::from_str::<String>(text).is_ok()
    }
}

/// An `i64`. The pinned `Number` keeps its digits and visits an `i64` exactly when they
/// parse as one (`number.rs`, `deserialize_any` with `arbitrary_precision`): `-0` does,
/// a fraction, an exponent or anything out of range does not.
struct I64;
impl Reads for I64 {
    fn reads(text: &str) -> bool {
        text.parse::<i64>().is_ok()
    }
}

/// A `bool`.
struct Bool;
impl Reads for Bool {
    fn reads(text: &str) -> bool {
        matches!(text, "true" | "false")
    }
}

/// A `serde_json::Number`: any JSON number, of any size (`arbitrary_precision`).
struct Number;
impl Reads for Number {
    fn reads(text: &str) -> bool {
        text.starts_with(|c: char| c == '-' || c.is_ascii_digit())
    }
}

/// A `serde_json::Value`: anything that decoded.
struct AnyValue;
impl Reads for AnyValue {
    fn reads(_: &str) -> bool {
        true
    }
}

/// A member of type `T` that must be present (or that `#[serde(default)]` fills in).
struct Member<T>(std::marker::PhantomData<T>);

impl<T> Default for Member<T> {
    fn default() -> Self {
        Self(std::marker::PhantomData)
    }
}

impl<'de, T: Reads> Deserialize<'de> for Member<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = <&RawValue>::deserialize(deserializer)?;
        if T::reads(&resolved(raw.get())) {
            Ok(Self::default())
        } else {
            Err(serde::de::Error::custom("unreadable member"))
        }
    }
}

/// An `Option<T>` member: missing from an object, or `null`, it is `None`
/// (`deserialize_option`, as serde's derive asks of a missing `Option`).
struct Optional<T>(std::marker::PhantomData<T>);

impl<T> Default for Optional<T> {
    fn default() -> Self {
        Self(std::marker::PhantomData)
    }
}

impl<'de, T: Reads> Deserialize<'de> for Optional<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Present<T>(std::marker::PhantomData<T>);
        impl<'de, T: Reads> Visitor<'de> for Present<T> {
            type Value = Optional<T>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an optional value")
            }
            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(Optional::default())
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(Optional::default())
            }
            fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
                let raw = <&RawValue>::deserialize(deserializer)?;
                // A private raw value holding `null` is a `null` in the pinned `Value`.
                let text = resolved(raw.get());
                if text == "null" || T::reads(&text) {
                    Ok(Optional::default())
                } else {
                    Err(serde::de::Error::custom("unreadable member"))
                }
            }
        }
        deserializer.deserialize_option(Present(std::marker::PhantomData))
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

/// `payload`, an event, as the pinned client's `serde_json::Value` holds it, for
/// classification only (#25). Two readings differ from the bytes as sent:
///
/// - Every object keeps the last of a repeated key (compared after unescaping); each
///   earlier occurrence is cut out with its separator.
/// - A member value that is serde_json's private number or raw value
///   ([`NUMBER_TOKEN`], [`RAW_VALUE_TOKEN`]) is what the first-party `Value` reads it
///   as (`value/de.rs`, `visit_map`): the number's own digits, or the JSON text it holds,
///   itself read this way (with its own repeated keys and private values, and its root
///   classified, as `serde_json::from_str::<Value>` reads it). Only one the pinned
///   `Value` accepts is replaced; [`decode`] already refused an event with any other in
///   a known member, and one in an unknown member or an undecoded frame stays as sent.
///
/// The event's own keys are plain: the pinned event is a struct, not a `Value`, so a
/// private token as its first key is an ordinary unknown member. Nothing else changes:
/// no other byte is re-spelled, and the client still receives `payload` as sent.
/// `payload` itself when nothing differs, and when it is not valid JSON.
///
/// One validating pass and one scan, with a key table per object. The JSON text of a
/// private raw value is unescaped and read once more at its own level, as
/// [`value_decodes`] reads it; each such level at least doubles the escapes of the one
/// inside it, so the levels stay logarithmic in the payload.
pub(crate) fn pinned_view(payload: &str) -> Cow<'_, str> {
    if serde_json::from_str::<IgnoredAny>(payload).is_err() {
        return Cow::Borrowed(payload);
    }
    view(payload, false, false, true)
}

/// `payload` with only the private values [`pinned_view`] resolves resolved: every
/// repeated key stays. For the account safety of a frame the pinned client cannot decode
/// (`crate::codex_ambiguous`), whose readers may keep any occurrence of a repeated key.
pub(crate) fn private_values_resolved(payload: &str) -> Cow<'_, str> {
    if serde_json::from_str::<IgnoredAny>(payload).is_err() {
        return Cow::Borrowed(payload);
    }
    view(payload, false, false, false)
}

/// [`pinned_view`] of valid JSON `payload`: `classify_root` when its outermost object is
/// itself a `Value` (the text of a private raw value), `validated` when every private
/// token object inside it is known to be one the pinned `Value` accepts (inside an
/// accepted raw value, whose whole text [`private_object`] checked), and `cut` to keep
/// only the last of a repeated key.
fn view(payload: &str, classify_root: bool, validated: bool, cut: bool) -> Cow<'_, str> {
    #[derive(Default)]
    struct Object<'a> {
        /// Where each member starts (its key's opening quote).
        members: Vec<usize>,
        keys: HashMap<Cow<'a, str>, usize>,
    }
    let b = payload.as_bytes();
    // Earlier occurrences to cut, and private values to replace, by their spans.
    let mut edits: Vec<(Range<usize>, Option<String>)> = Vec::new();
    // `None` for an array.
    let mut stack: Vec<Option<Object<'_>>> = Vec::new();
    let mut key_next = false;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'{' => {
                if (classify_root || !stack.is_empty())
                    && let Some((end, value)) = private_value(payload, i, validated, cut)
                {
                    edits.push((i..end, Some(value)));
                    key_next = false;
                    i = end;
                    continue;
                }
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
                    if let Some(earlier) = object.keys.insert(key, index)
                        && cut
                    {
                        edits.push((object.members[earlier]..object.members[earlier + 1], None));
                    }
                }
                key_next = false;
                i = end;
            }
            _ => {}
        }
        i += 1;
    }
    if edits.is_empty() {
        return Cow::Borrowed(payload);
    }
    // Spans never cross: a cut member can hold a private value, never the reverse, since
    // the scan does not enter a private value.
    edits.sort_by_key(|(r, _)| r.start);
    let mut out = String::with_capacity(payload.len());
    let mut at = 0;
    for (range, replacement) in edits {
        // An edit inside a cut already made goes with it.
        if range.start < at {
            continue;
        }
        out.push_str(&payload[at..range.start]);
        if let Some(replacement) = replacement {
            out.push_str(&replacement);
        }
        at = range.end;
    }
    out.push_str(&payload[at..]);
    Cow::Owned(out)
}

/// The object starting at `open` (its `{`) in valid JSON `raw`, when it is a private
/// number or raw value the pinned `Value` accepts: where it ends (exclusive), and what
/// that `Value` reads it as, in [`view`]'s terms. `validated` skips the acceptance check
/// an enclosing raw value already made.
fn private_value(raw: &str, open: usize, validated: bool, cut: bool) -> Option<(usize, String)> {
    if !validated && !matches!(private_object(raw, open + 1), Private::Accepted(_)) {
        return None;
    }
    let b = raw.as_bytes();
    let key_at = skip_space(b, open + 1);
    if b.get(key_at) != Some(&b'"') {
        return None;
    }
    let key_end = quote_end(b, key_at + 1);
    let number = match unescape(&raw[key_at..=key_end]).as_deref() {
        Some(NUMBER_TOKEN) => true,
        Some(RAW_VALUE_TOKEN) => false,
        _ => return None,
    };
    let value_at = skip_space(b, skip_space(b, key_end + 1) + 1);
    let value_end = quote_end(b, value_at + 1);
    let text = unescape(&raw[value_at..=value_end])?;
    let close = skip_space(b, value_end + 1);
    let value = if number {
        text.into_owned()
    } else {
        view(text.trim(), true, true, cut).into_owned()
    };
    Some((close + 1, value))
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
