//! The buffered (non-streaming) xAI Responses body: its SSE frames and the terminal
//! failures they can carry (Go v8.0.67 `XAIExecutor.Execute`, 949b43b3; the frozen
//! v8.0.35 reference split the body on `\n` and read each `data:` line alone).
//!
//! A frame is the data of one event: `data:` lines joined with `\n` until a blank line,
//! the next `data:` line once the held data is complete JSON (or `[DONE]`), a named
//! `event:` line that follows named data, or the end of the body. `event:` names the
//! frame it belongs to, before or after its data. Lines end at `\n`, a trailing `\r`
//! dropped, so CRLF bodies frame alike. Other fields and comments are ignored.
//!
//! Frames borrow the body; only an event whose data spans several lines is copied, once.
//! Whether held multi-line data is complete is decided incrementally (the bracket
//! nesting of the joined text, validated once when its outermost value closes), so a
//! body is scanned in time linear in its size.

use std::borrow::Cow;

use cpa_common::gostr::{GoStr, trim_space};
use cpa_common::json::{self as gj, Kind};

const DATA: &[u8] = b"data:";
const EVENT: &[u8] = b"event:";
const DONE: &[u8] = b"[DONE]";

pub(crate) const EMPTY_STREAM: &str = "xai stream error: empty upstream stream";
pub(crate) const INCOMPLETE_STREAM: &str =
    "xai stream error: stream closed before response.completed or response.incomplete";

/// One SSE event of the body: its `event:` name (trimmed, possibly empty) and data.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Frame<'a> {
    pub event: &'a [u8],
    pub data: Cow<'a, [u8]>,
}

/// Data held for the frame being collected.
enum Held<'a> {
    None,
    /// One data line, borrowed.
    Line(&'a [u8]),
    /// Several data lines joined with `\n`.
    Lines {
        joined: Vec<u8>,
        nesting: Nesting,
        /// Validity of `joined`, decided once when its outermost value closed. A text
        /// whose first value has closed only grows trailing bytes, so it never turns
        /// valid later.
        complete: Option<bool>,
    },
}

impl<'a> Held<'a> {
    /// The held data is a whole event (Go: `ValidBytes(pendingData) || [DONE]`).
    fn complete(&self) -> bool {
        match self {
            Held::None => false,
            Held::Line(line) => *line == DONE || gj::valid(line),
            Held::Lines { complete, .. } => complete.unwrap_or(false),
        }
    }

    fn append(&mut self, line: &'a [u8]) {
        match std::mem::replace(self, Held::None) {
            Held::None => *self = Held::Line(line),
            Held::Line(first) => {
                let mut nesting = Nesting::default();
                let closed = nesting.feed(first) | nesting.feed(b"\n") | nesting.feed(line);
                let joined = [first, b"\n", line].concat();
                let complete = closed.then(|| gj::valid(&joined));
                *self = Held::Lines {
                    joined,
                    nesting,
                    complete,
                };
            }
            Held::Lines {
                mut joined,
                mut nesting,
                mut complete,
            } => {
                joined.push(b'\n');
                joined.extend_from_slice(line);
                if complete.is_none() && (nesting.feed(b"\n") | nesting.feed(line)) {
                    complete = Some(gj::valid(&joined));
                }
                *self = Held::Lines {
                    joined,
                    nesting,
                    complete,
                };
            }
        }
    }

    fn take(&mut self) -> Option<Cow<'a, [u8]>> {
        match std::mem::replace(self, Held::None) {
            Held::None => None,
            Held::Line(line) => Some(Cow::Borrowed(line)),
            Held::Lines { joined, .. } => Some(Cow::Owned(joined)),
        }
    }
}

/// The frames of a buffered SSE body, in order.
pub(crate) struct Frames<'a> {
    rest: &'a [u8],
    event: &'a [u8],
    held: Held<'a>,
}

impl<'a> Frames<'a> {
    pub(crate) fn new(body: &'a [u8]) -> Self {
        Self {
            rest: body,
            event: b"",
            held: Held::None,
        }
    }

    /// Go `flushPendingEvent`: the held frame, if any; the event name is reset either way.
    fn flush(&mut self) -> Option<Frame<'a>> {
        let event = std::mem::take(&mut self.event);
        self.held.take().map(|data| Frame { event, data })
    }

    /// The next line without its `\n` and a trailing `\r`; a final `\n` starts no line.
    fn next_line(&mut self) -> Option<&'a [u8]> {
        if self.rest.is_empty() {
            return None;
        }
        let line = match self.rest.iter().position(|b| *b == b'\n') {
            Some(i) => {
                let line = &self.rest[..i];
                self.rest = &self.rest[i + 1..];
                line
            }
            None => std::mem::take(&mut self.rest),
        };
        Some(line.strip_suffix(b"\r").unwrap_or(line))
    }
}

impl<'a> Iterator for Frames<'a> {
    type Item = Frame<'a>;

    fn next(&mut self) -> Option<Frame<'a>> {
        while let Some(line) = self.next_line() {
            if trim_space(line).is_empty() {
                if let Some(frame) = self.flush() {
                    return Some(frame);
                }
                continue;
            }
            if let Some(name) = line.strip_prefix(EVENT) {
                let flushed = if !matches!(self.held, Held::None) && !self.event.is_empty() {
                    self.flush()
                } else {
                    None
                };
                self.event = trim_space(name);
                if flushed.is_some() {
                    return flushed;
                }
                continue;
            }
            let Some(value) = line.strip_prefix(DATA) else {
                continue;
            };
            let value = trim_space(value);
            if matches!(self.held, Held::None) {
                self.held = Held::Line(value);
                continue;
            }
            if self.held.complete() {
                let frame = self.flush();
                self.held = Held::Line(value);
                return frame;
            }
            self.held.append(value);
        }
        self.flush()
    }
}

/// `codexJSONNesting` (as in codex_sse.rs): the bracket nesting of a JSON text fed in
/// pieces; reports when the outermost object or array closes.
#[derive(Default)]
struct Nesting {
    depth: i64,
    in_string: bool,
    escaped: bool,
}

impl Nesting {
    fn feed(&mut self, p: &[u8]) -> bool {
        let mut closed = false;
        for &c in p {
            closed |= self.byte(c);
        }
        closed
    }

    fn byte(&mut self, c: u8) -> bool {
        if self.in_string {
            if self.escaped {
                self.escaped = false;
            } else if c == b'\\' {
                self.escaped = true;
            } else if c == b'"' {
                self.in_string = false;
            }
            return false;
        }
        match c {
            b'"' => self.in_string = true,
            b'{' | b'[' => self.depth += 1,
            b'}' | b']' => {
                self.depth -= 1;
                return self.depth <= 0;
            }
            _ => {}
        }
        false
    }
}

// --- terminal failures (xaiTerminalFailureErr) -------------------------------------------

/// `xaiTerminalFailureErr` without the status error itself: the status a terminal
/// failure maps to and the OpenAI-style error document it is reported with, or `None`
/// when `data` (an event's data, or a whole body without SSE data) is no failure.
pub(crate) fn terminal_failure(event: &[u8], data: &[u8]) -> Option<(u16, Vec<u8>)> {
    let doc = failure_doc(event, data)?;
    Some((failure_status(data, &doc), doc))
}

/// `xaiTerminalFailureDoc`.
fn failure_doc(event: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    let data = trim_space(data);
    if data.is_empty() {
        return None;
    }
    let event = String::from_utf8_lossy(trim_space(event));
    if !gj::valid(data) {
        if !event.go_eq_fold("error") {
            return None;
        }
        let mut doc = br#"{"error":{}}"#.to_vec();
        gj::set_str(&mut doc, "error.message", data);
        return Some(doc);
    }
    let mut kind = gj::get(data, "type").str().trim().to_owned();
    if kind.is_empty() {
        let error = gj::get(data, "error");
        if event == "error" || event == "response.failed" {
            kind = event.into_owned();
        } else if error.exists()
            && error.kind != Kind::Null
            && !gj::get(data, "output").exists()
            && (error.kind == Kind::Json || !error.str().trim().is_empty())
        {
            kind = "error".to_owned();
        }
    }
    let doc = match kind.as_str() {
        "error" => error_doc(data, "error")
            .or_else(|| error_doc(data, "body.error"))
            .or_else(|| top_level_error_doc(data)),
        "response.failed" => error_doc(data, "response.error").or_else(|| error_doc(data, "error")),
        _ => return None,
    };
    let mut doc =
        doc.unwrap_or_else(|| br#"{"error":{"message":"upstream stream failed without error details"}}"#.to_vec());
    let sequence = gj::get(data, "sequence_number");
    if sequence.exists() && sequence.kind == Kind::Number {
        gj::set_int(&mut doc, "sequence_number", sequence.int());
    }
    Some(doc)
}

fn trimmed(json: &[u8], path: &str) -> String {
    gj::get(json, path).str().trim().to_owned()
}

/// The first non-empty trimmed string of `paths` in `json`.
fn first(json: &[u8], paths: &[&str]) -> Option<String> {
    paths.iter().map(|p| trimmed(json, p)).find(|s| !s.is_empty())
}

/// `xaiTerminalErrorDoc`: the error at `path`, completed from its sibling fields.
fn error_doc(data: &[u8], path: &str) -> Option<Vec<u8>> {
    let error = gj::get(data, path);
    if !error.exists() || error.kind == Kind::Null {
        return None;
    }
    let mut doc = br#"{"error":{}}"#.to_vec();
    let mut has_field = false;
    if error.kind == Kind::Json {
        if error.is_object() && !error.map().is_empty() {
            gj::set_raw(&mut doc, "error", error.raw());
            has_field = true;
        }
    } else {
        let message = error.str().trim().to_owned();
        if !message.is_empty() {
            gj::set_str(&mut doc, "error.message", message);
            has_field = true;
        }
    }
    for (field, paths) in [
        ("error.code", ["response.error.code", "code", "body.error.code"]),
        ("error.type", ["response.error.type", "error_type", "body.error.type"]),
        ("error.param", ["response.error.param", "param", "body.error.param"]),
        (
            "error.message",
            ["response.error.message", "message", "body.error.message"],
        ),
    ] {
        if trimmed(&doc, field).is_empty()
            && let Some(value) = first(data, &paths)
        {
            gj::set_str(&mut doc, field, value);
            has_field = true;
        }
    }
    let fallback = first(&doc, &["error.code", "error.type"]);
    if trimmed(&doc, "error.message").is_empty()
        && let Some(message) = fallback
    {
        gj::set_str(&mut doc, "error.message", message);
        has_field = true;
    }
    has_field.then_some(doc)
}

/// `xaiTerminalTopLevelErrorDoc`: a flat `{message, code, error_type, param}` error.
fn top_level_error_doc(data: &[u8]) -> Option<Vec<u8>> {
    let message = trimmed(data, "message");
    let code = trimmed(data, "code");
    let error_type = trimmed(data, "error_type");
    let param = trimmed(data, "param");
    if message.is_empty() && code.is_empty() && error_type.is_empty() && param.is_empty() {
        return None;
    }
    let mut doc = br#"{"error":{}}"#.to_vec();
    for (field, value) in [
        ("error.message", &message),
        ("error.code", &code),
        ("error.type", &error_type),
        ("error.param", &param),
    ] {
        if !value.is_empty() {
            gj::set_str(&mut doc, field, value);
        }
    }
    if message.is_empty() {
        if !code.is_empty() {
            gj::set_str(&mut doc, "error.message", &code);
        } else if !error_type.is_empty() {
            gj::set_str(&mut doc, "error.message", &error_type);
        }
    }
    Some(doc)
}

/// An HTTP error status, from a number or a numeric string.
fn error_status(value: &gj::Res<'_>) -> Option<u16> {
    let status = match value.kind {
        Kind::Number => value.int(),
        Kind::String => value.str().trim().parse::<i64>().ok()?,
        _ => return None,
    };
    (400..=599).contains(&status).then_some(status as u16)
}

/// `xaiTerminalFailureStatus`: an explicit status in the event, else the status the
/// error's type, code or message names; 502 when none does.
fn failure_status(data: &[u8], doc: &[u8]) -> u16 {
    for path in [
        "status",
        "status_code",
        "error.status",
        "error.status_code",
        "response.error.status",
        "response.error.status_code",
        "body.status",
        "body.error.status",
    ] {
        if let Some(status) = error_status(&gj::get(data, path)) {
            return status;
        }
    }
    let kind = trimmed(doc, "error.type").go_lower();
    let code = trimmed(doc, "error.code").go_lower();
    let message = trimmed(doc, "error.message").go_lower();
    if let Ok(status) = code.parse::<i64>()
        && (400..=599).contains(&status)
    {
        return status as u16;
    }
    let code_is = |names: &[&str]| names.contains(&code.as_str());
    if kind == "authentication_error"
        || code_is(&["invalid_api_key", "unauthorized"])
        || code.starts_with("unauthenticated")
        || code.contains("bad-credentials")
        || message.contains("access token could not be validated")
    {
        401
    } else if kind == "permission_error"
        || code_is(&["forbidden", "permission_denied", "permission-denied"])
        || code.starts_with("permission_denied:")
        || code.starts_with("permission-denied:")
    {
        403
    } else if kind == "not_found_error" || code_is(&["not_found", "model_not_found"]) {
        404
    } else if kind == "rate_limit_error"
        || code_is(&["rate_limit_exceeded", "rate_limit"])
        || code.contains("free-usage-exhausted")
        || code.contains("resource_exhausted")
        || code.contains("resource-exhausted")
        || message.contains("free-usage-exhausted")
        || message.contains("included free usage")
    {
        429
    } else if code_is(&[
        "cyber_policy",
        "content_policy_violation",
        "invalid_value",
        "invalid_request_error",
        "invalid_argument",
        "invalid-argument",
        "context_length_exceeded",
        "context_too_large",
    ]) || [
        "invalid_request_error",
        "invalid_request",
        "bad_request_error",
        "image_generation_user_error",
    ]
    .contains(&kind.as_str())
    {
        400
    } else if kind == "service_unavailable_error" || code_is(&["server_is_overloaded", "service_unavailable"]) {
        503
    } else {
        502
    }
}

#[cfg(test)]
#[path = "xai_sse_tests.rs"]
mod tests;
