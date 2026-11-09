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
//! A member repeats when its path repeats: the same key twice in one object, or in two
//! occurrences of a repeated ancestor (two `error` objects of the frame, say). The same
//! name in two different objects is no repeat: `error.type` and `response.error.type`
//! are two members, and a frame with each once is read by its first and last readings
//! alone. Once some member repeats, every occurrence of every member counts.
//!
//! The analysis is one tolerant pass that collects the occurrences of those members,
//! then one check of each occurrence on its own, for the checks that read one member. The
//! capacity check reads the error body a failure reading composes from several members
//! (`response::terminal_failure`): the error object as sent, the frame's `message`,
//! `code`, `error_type` and `param`, and a message taken from the other error object.
//! For it, the pass keeps one summary of every piece any reading could compose, and no
//! combination is enumerated, so the work stays linear in the frame. Both are a superset
//! of the combinations a reader could make: an ambiguous frame can be withheld that no
//! single reading classifies as an account failure. The pinned client skips it either
//! way. A private number or raw value (`crate::codex_event::pinned_view`) is read both as
//! sent and as the pinned `Value` reads it.

use std::ops::Range;

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

/// A member a failure reading reads, by its path from the frame. `Top` is a member of the
/// frame (`codex_ws::ws_error`, and `response::terminal_failure` for `type` and the
/// status-less fallbacks); `Response` is the frame's `response`, which no reading
/// classifies itself but which holds `ResponseError`, `response.error`; `Field` is a
/// member of one of the two error objects ([`Object::Error`] or
/// [`Object::ResponseError`]), which `response::terminal_failure` classifies. A slot
/// occurs more than once exactly when a key on its path repeats within one object.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    Top(&'static str),
    Response,
    ResponseError,
    Field(Object, &'static str),
}

impl Slot {
    /// A member of either error object, whichever object it is in.
    fn field(self) -> Option<&'static str> {
        match self {
            Self::Field(_, name) => Some(name),
            Self::Top(_) | Self::Response | Self::ResponseError => None,
        }
    }
}

const TOP: [&str; 8] = [
    "type",
    "status",
    "status_code",
    "error",
    "message",
    "code",
    "error_type",
    "param",
];
const FIELDS: [&str; 5] = ["type", "code", "status", "status_code", "message"];

/// The slot of member `key` of an object at `object`, and the object its value opens if
/// it is one.
fn member(object: Object, key: &str) -> (Option<Slot>, Object) {
    let named = |names: &[&'static str]| names.iter().copied().find(|n| *n == key);
    match object {
        Object::Root => match key {
            "error" => (Some(Slot::Top("error")), Object::Error),
            "response" => (Some(Slot::Response), Object::Response),
            _ => (named(&TOP).map(Slot::Top), Object::Other),
        },
        Object::Response if key == "error" => (Some(Slot::ResponseError), Object::ResponseError),
        Object::Error | Object::ResponseError => (named(&FIELDS).map(|name| Slot::Field(object, name)), Object::Other),
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
/// message in the body it composes, [`Capacity`]). Each occurrence is read as the
/// readings read a value (`rawjson`). The frame is read as sent and, when it holds
/// private values the pinned `Value` resolves, once more with them resolved.
pub(crate) fn ambiguous_account(payload: &str) -> bool {
    ambiguous(payload)
        || match crate::codex_event::private_values_resolved(payload) {
            std::borrow::Cow::Owned(resolved) => ambiguous(&resolved),
            std::borrow::Cow::Borrowed(_) => false,
        }
}

/// What the capacity check (`response::is_capacity`) can find in the error body some
/// reading composes (`response::terminal_failure_body`). That body holds, as sent, an
/// error object or the text of an error string, and it can take a message from the other
/// error object, the error object's own `code` or `type`, or the frame's `message`,
/// `code`, `error_type` and `param` (the text a reading takes from each). The check
/// looks for its phrases in each message and in the whole body, so `model` in one piece
/// and `at capacity` in another can be one capacity message. Each phrase lies within one
/// piece: the body adds only keys and punctuation, which hold neither. So the union of
/// what every piece holds is a superset of what any one body holds.
#[derive(Default)]
struct Capacity {
    model: bool,
    at_capacity: bool,
    /// A phrase that is a capacity message on its own.
    whole: bool,
}

impl Capacity {
    fn note(&mut self, text: &str) {
        let l = text.to_ascii_lowercase();
        self.whole |= l.contains("model_at_capacity") || l.contains("model_is_at_capacity");
        self.model |= l.contains("model");
        self.at_capacity |= l.contains("at capacity");
    }

    fn possible(&self) -> bool {
        self.whole || (self.model && self.at_capacity)
    }
}

fn ambiguous(payload: &str) -> bool {
    let found = occurrences(payload);
    // At most 20 distinct slots, so this stays linear in the occurrences.
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
    // The values of the given frame members and of the given fields of either error
    // object: a conservative union once anything repeats.
    let values = |slots: &[Slot], fields: &[&str]| -> Vec<&str> {
        found
            .iter()
            .filter(|(slot, _)| slots.contains(slot) || slot.field().is_some_and(|f| fields.contains(&f)))
            .map(|(_, range)| &payload[range.clone()])
            .collect()
    };
    let lower = |raw: &str| rawjson::parse(raw).str().trim().to_ascii_lowercase();
    let account = |raw: &str| {
        let status = rawjson::parse(raw).i64();
        matches!(status, 401 | 402 | 403 | 429)
    };
    let kinds: Vec<String> = values(&[Slot::Top("type")], &[]).into_iter().map(lower).collect();
    let error = kinds.iter().any(|k| k == "error");
    if !error && !kinds.iter().any(|k| k == "response.failed") {
        return false;
    }
    // The status frame reading of an `error` frame.
    if error
        && values(&[Slot::Top("status"), Slot::Top("status_code")], &[])
            .into_iter()
            .any(account)
    {
        return true;
    }
    // The terminal failure reading of the error object, or of the status-less fallback.
    values(&[Slot::Top("error_type")], &["type"]).into_iter().any(|raw| {
        matches!(
            lower(raw).as_str(),
            "usage_limit_reached" | "authentication_error" | "permission_error" | "rate_limit_error"
        )
    }) || values(&[Slot::Top("code")], &["code"]).into_iter().any(|raw| {
        matches!(
            lower(raw).as_str(),
            "invalid_api_key" | "unauthorized" | "forbidden" | "permission_denied" | "rate_limit_exceeded"
        )
    }) || values(&[], &["status", "status_code"]).into_iter().any(account)
        || {
            let mut capacity = Capacity::default();
            for (slot, range) in &found {
                let raw = &payload[range.clone()];
                match slot {
                    // An error object is composed as sent; an error string as its text.
                    Slot::Top("error") | Slot::ResponseError if raw.starts_with(['{', '[']) => capacity.note(raw),
                    Slot::Top("error" | "message" | "code" | "error_type" | "param")
                    | Slot::ResponseError
                    | Slot::Field(_, "message" | "code" | "type") => capacity.note(rawjson::parse(raw).str()),
                    _ => {}
                }
            }
            capacity.possible()
        }
}

#[cfg(test)]
#[path = "codex_ambiguous_tests.rs"]
mod tests;
