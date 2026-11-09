//! Account safety for a frame the pinned client cannot decode (#25 E2, #6): whether some
//! reading of its repeated keys could make it an account failure.
//!
//! A frame that does not decode is skipped by the pinned client, so it is never
//! classified: no cooling, usage, acknowledgement or ownership comes of it. It still
//! reaches a WebSocket client as sent, unless it could be an account failure, whose frame
//! can name the pooled account or carry the credential's own token
//! (`codex_ws::account_shaped`). A JSON reader keeps the first or the last of a repeated
//! key, and a reader that acts on members as they stream past can act on any of them. So
//! when a member the failure readings read repeats, every occurrence counts: the frame is
//! withheld when any occurrence of any of those members could make it an account failure.
//!
//! The analysis is one tolerant pass that collects the occurrences of those members,
//! then one check of each occurrence on its own; occurrences are not combined, so the
//! work stays linear in the frame. That is a superset of the combinations a reader could
//! make: an ambiguous frame can be withheld that no single reading classifies as an
//! account failure. The pinned client skips it either way.

use std::ops::Range;

use crate::codex_response as response;
use crate::rawjson;

/// Where an object sits among those the failure readings read: the frame, its `error`
/// object, its `response` object, and the `error` object inside that.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Object {
    Root,
    Error,
    Response,
    ResponseError,
    /// Anything else, which no failure reading looks into.
    Other,
}

/// A member a failure reading reads. `Top` is a member of the frame
/// (`codex_ws::ws_error`, and `response::terminal_failure` for `type` and the status-less
/// fallbacks); `ResponseError` is `response.error`; `Field` is a member of either error
/// object, which `response::terminal_failure` classifies.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    Top(&'static str),
    ResponseError,
    Field(&'static str),
}

const TOP: [&str; 7] = [
    "type",
    "status",
    "status_code",
    "error",
    "message",
    "code",
    "error_type",
];
const FIELDS: [&str; 5] = ["type", "code", "status", "status_code", "message"];

/// The slot of member `key` of an object at `object`, and the object its value opens if
/// it is one.
fn member(object: Object, key: &str) -> (Option<Slot>, Object) {
    let named = |names: &[&'static str]| names.iter().copied().find(|n| *n == key);
    match object {
        Object::Root => match key {
            "error" => (Some(Slot::Top("error")), Object::Error),
            "response" => (None, Object::Response),
            _ => (named(&TOP).map(Slot::Top), Object::Other),
        },
        Object::Response if key == "error" => (Some(Slot::ResponseError), Object::ResponseError),
        Object::Error | Object::ResponseError => (named(&FIELDS).map(Slot::Field), Object::Other),
        Object::Response | Object::Other => (None, Object::Other),
    }
}

/// One open container of the pass.
struct Open {
    object: Object,
    is_object: bool,
    /// An object's next string is a key.
    key_next: bool,
    /// The member whose value comes next: its slot and the object it opens.
    member: Option<(Option<Slot>, Object)>,
    /// A container value of a slot: its slot and where it starts.
    value: Option<(Slot, usize)>,
}

/// Every occurrence of a slot's value in `payload`, read tolerantly: invalid JSON is read
/// as far as it goes, and a value the input cuts off ends with it. Keys are compared
/// after unescaping. One pass.
fn occurrences(payload: &str) -> Vec<(Slot, Range<usize>)> {
    let b = payload.as_bytes();
    let mut found = Vec::new();
    let mut stack: Vec<Open> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' | b'\n' | b'\r' | b':' => i += 1,
            b',' => {
                if let Some(open) = stack.last_mut() {
                    open.key_next = open.is_object;
                    open.member = None;
                }
                i += 1;
            }
            b'}' | b']' => {
                stack.pop();
                if let Some(parent) = stack.last_mut()
                    && let Some((slot, start)) = parent.value.take()
                {
                    found.push((slot, start..i + 1));
                }
                i += 1;
            }
            _ => {
                let start = i;
                let end = match c {
                    b'"' => string_end(b, i + 1),
                    b'{' | b'[' => i + 1,
                    _ => scalar_end(b, i),
                };
                // A key: remember the member it names.
                if c == b'"'
                    && let Some(open) = stack.last_mut()
                    && open.key_next
                {
                    let key = key_text(&payload[start..end]);
                    open.member = Some(member(open.object, &key));
                    open.key_next = false;
                    i = end;
                    continue;
                }
                // A value: of a member when its object named one, else an array element
                // or the frame itself.
                let (slot, object) = match stack.last_mut() {
                    Some(open) => open.member.take().unwrap_or((None, Object::Other)),
                    None => (None, Object::Root),
                };
                if matches!(c, b'{' | b'[') {
                    if let (Some(slot), Some(open)) = (slot, stack.last_mut()) {
                        open.value = Some((slot, start));
                    }
                    stack.push(Open {
                        object: if c == b'{' { object } else { Object::Other },
                        is_object: c == b'{',
                        key_next: c == b'{',
                        member: None,
                        value: None,
                    });
                } else if let Some(slot) = slot {
                    found.push((slot, start..end));
                }
                i = end;
            }
        }
    }
    // Values the input cut off end with it.
    for open in stack {
        if let Some((slot, start)) = open.value {
            found.push((slot, start..b.len()));
        }
    }
    found
}

/// The end (exclusive) of the string whose contents start at `i`: after its closing
/// quote, or the end of the input.
fn string_end(b: &[u8], mut i: usize) -> usize {
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    b.len()
}

/// The end (exclusive) of a number or literal starting at `i`, which is none of the
/// delimiters, so the pass always moves on.
fn scalar_end(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && !matches!(b[i], b',' | b'}' | b']' | b':' | b'"' | b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// A key as a reader compares it: unescaped; as written when it does not unescape (it
/// then names no member a reader would match either).
fn key_text(quoted: &str) -> std::borrow::Cow<'_, str> {
    if quoted.len() >= 2 && quoted.ends_with('"') {
        if let Ok(key) = serde_json::from_str::<String>(quoted) {
            return key.into();
        }
        return quoted[1..quoted.len() - 1].into();
    }
    quoted.get(1..).unwrap_or_default().into()
}

/// Whether `payload`, a frame the pinned client cannot decode, repeats a member the
/// failure readings read and any occurrence could make it an account failure: a frame
/// `type` of `error` or `response.failed`, with an account status (401, 402, 403 or 429)
/// as the status frame reads it, or an error object `response::terminal_failure`
/// classifies as one (an account `type`, `code` or status, a usage limit, a capacity
/// message). Each occurrence is read as the readings read a value (`rawjson`); the
/// capacity check is the readings' own (`response::is_capacity`).
pub(crate) fn ambiguous_account(payload: &str) -> bool {
    let found = occurrences(payload);
    // At most 13 distinct slots, so this stays linear in the occurrences.
    let mut seen: Vec<Slot> = Vec::new();
    let repeated = found.iter().any(|(slot, _)| {
        let again = seen.contains(slot);
        if !again {
            seen.push(*slot);
        }
        again
    });
    if !repeated {
        return false;
    }
    let values = |slots: &[Slot]| -> Vec<&str> {
        found
            .iter()
            .filter(|(slot, _)| slots.contains(slot))
            .map(|(_, range)| &payload[range.clone()])
            .collect()
    };
    let lower = |raw: &str| rawjson::parse(raw).str().trim().to_ascii_lowercase();
    let account = |raw: &str| {
        let status = rawjson::parse(raw).i64();
        matches!(status, 401 | 402 | 403 | 429)
    };
    let kinds: Vec<String> = values(&[Slot::Top("type")]).into_iter().map(lower).collect();
    let error = kinds.iter().any(|k| k == "error");
    if !error && !kinds.iter().any(|k| k == "response.failed") {
        return false;
    }
    // The status frame reading of an `error` frame.
    if error
        && values(&[Slot::Top("status"), Slot::Top("status_code")])
            .into_iter()
            .any(account)
    {
        return true;
    }
    // The terminal failure reading of the error object, or of the status-less fallback.
    values(&[Slot::Field("type"), Slot::Top("error_type")])
        .into_iter()
        .any(|raw| {
            matches!(
                lower(raw).as_str(),
                "usage_limit_reached" | "authentication_error" | "permission_error" | "rate_limit_error"
            )
        })
        || values(&[Slot::Field("code"), Slot::Top("code")])
            .into_iter()
            .any(|raw| {
                matches!(
                    lower(raw).as_str(),
                    "invalid_api_key" | "unauthorized" | "forbidden" | "permission_denied" | "rate_limit_exceeded"
                )
            })
        || values(&[Slot::Field("status"), Slot::Field("status_code")])
            .into_iter()
            .any(account)
        || values(&[
            Slot::Top("error"),
            Slot::ResponseError,
            Slot::Field("message"),
            Slot::Top("message"),
        ])
        .into_iter()
        .any(|raw| response::is_capacity(&format!(r#"{{"error":{raw}}}"#)))
}

#[cfg(test)]
#[path = "codex_ambiguous_tests.rs"]
mod tests;
