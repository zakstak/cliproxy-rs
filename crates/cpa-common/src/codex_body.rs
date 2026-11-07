//! The Codex JSON request boundary (#22, `docs/codex/CONTRACT.md` "Request boundary").
//!
//! [`CodexBody::parse`] reads a body once, in place, and either accepts it or returns a
//! [`BodyError`]. It never copies, decodes into a value tree, re-serializes or edits the
//! body, so a caller that forwards the body forwards the bytes it already holds. Numbers
//! are checked against the JSON grammar and never converted, so `1e400` and a 40-digit
//! integer pass with their spelling intact.
//!
//! The pass enforces, in document order:
//!
//! - size, before anything else ([`MAX_BODY_BYTES`]);
//! - UTF-8;
//! - the JSON grammar (RFC 8259), with nothing after the root value;
//! - string escapes that decode to Unicode scalar values: a lone surrogate escape is
//!   malformed;
//! - unique keys in every object, compared after unescaping, so an escaped spelling of
//!   a key collides with the plain one;
//! - nesting depth ([`MAX_DEPTH`]);
//! - a top-level object.
//!
//! After the pass, the members the proxy reads for routing must have the types the
//! Responses API gives them: `model` a string, `stream` a boolean and
//! `previous_response_id` a string or null. Any other member, at any depth, is accepted
//! as valid JSON. An unknown or future field is never a reason to reject.
//!
//! Error messages carry byte offsets and protocol field names, never body content, so
//! they are safe to return to the client and to log.

use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt;

use cpa_core::exec::{ExecError, FailureScope};

/// The largest body the boundary reads: the server's request limit
/// (`cpa_server::MAX_REQUEST_BYTES`), so a body the server accepted never fails here on
/// size alone.
pub const MAX_BODY_BYTES: usize = 64 << 20;

/// The deepest nesting of arrays and objects, counting the root object as 1. First-party
/// request bodies, tool schemas included, nest far less.
pub const MAX_DEPTH: usize = 100;

/// Why a body was refused. `Display` is the client-facing message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyError {
    TooLarge {
        limit: usize,
    },
    /// `offset` is the length of the valid UTF-8 prefix.
    InvalidUtf8 {
        offset: usize,
    },
    Malformed {
        offset: usize,
    },
    /// `offset` is where the repeated key starts.
    DuplicateKey {
        offset: usize,
    },
    TooDeep {
        limit: usize,
        offset: usize,
    },
    NotObject,
    FieldType {
        field: &'static str,
        expected: &'static str,
    },
}

impl BodyError {
    pub fn status(self) -> u16 {
        match self {
            Self::TooLarge { .. } => 413,
            _ => 400,
        }
    }
}

impl fmt::Display for BodyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::TooLarge { limit } => write!(f, "request body exceeds {limit} bytes"),
            Self::InvalidUtf8 { offset } => write!(f, "request body is not valid UTF-8 at byte {offset}"),
            Self::Malformed { offset } => write!(f, "request body is not valid JSON at byte {offset}"),
            Self::DuplicateKey { offset } => write!(f, "request body repeats an object key at byte {offset}"),
            Self::TooDeep { limit, offset } => {
                write!(f, "request body nests deeper than {limit} levels at byte {offset}")
            }
            Self::NotObject => f.write_str("request body must be a JSON object"),
            Self::FieldType { field, expected } => write!(f, "request field `{field}` must be {expected}"),
        }
    }
}

impl From<BodyError> for ExecError {
    /// A request fault: the same body fails on every credential, so no failover.
    fn from(error: BodyError) -> Self {
        ExecError::local(error.status(), FailureScope::Request, error.to_string())
    }
}

/// The JSON type of a member's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Object,
    Array,
    String,
    Number,
    True,
    False,
    Null,
}

/// One top-level member: its unescaped key, its type, and its value's exact bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member<'a> {
    pub key: Cow<'a, str>,
    pub kind: Kind,
    pub raw: &'a [u8],
}

impl<'a> Member<'a> {
    /// The unescaped text of a string value, borrowed when it has no escapes.
    pub fn text(&self) -> Option<Cow<'a, str>> {
        if self.kind != Kind::String {
            return None;
        }
        // The value passed the scanner, so it is UTF-8 and a well-formed string.
        let text = std::str::from_utf8(self.raw).ok()?;
        Scanner { text, at: 0 }.string(true).ok().flatten()
    }
}

/// A body that passed the boundary: the top-level members in document order, each
/// pointing into the caller's bytes.
#[derive(Debug)]
pub struct CodexBody<'a> {
    raw: &'a [u8],
    members: Vec<Member<'a>>,
}

impl<'a> CodexBody<'a> {
    pub fn parse(raw: &'a [u8]) -> Result<Self, BodyError> {
        Self::parse_limited(raw, MAX_BODY_BYTES)
    }

    fn parse_limited(raw: &'a [u8], max_bytes: usize) -> Result<Self, BodyError> {
        if raw.len() > max_bytes {
            return Err(BodyError::TooLarge { limit: max_bytes });
        }
        let text = std::str::from_utf8(raw).map_err(|e| BodyError::InvalidUtf8 {
            offset: e.valid_up_to(),
        })?;
        let mut scanner = Scanner { text, at: 0 };
        scanner.whitespace();
        match scanner.peek() {
            Some(b'{') => {}
            Some(b'[' | b'"' | b'-' | b'0'..=b'9' | b't' | b'f' | b'n') => {
                // Still reject a malformed non-object as malformed.
                scanner.value(1)?;
                scanner.end()?;
                return Err(BodyError::NotObject);
            }
            _ => return Err(scanner.malformed()),
        }
        let mut members = Vec::new();
        scanner.object(1, Some(&mut members))?;
        scanner.end()?;
        for member in &members {
            let (field, expected, ok) = match &*member.key {
                "model" => ("model", "a string", member.kind == Kind::String),
                "stream" => ("stream", "a boolean", matches!(member.kind, Kind::True | Kind::False)),
                "previous_response_id" => (
                    "previous_response_id",
                    "a string or null",
                    matches!(member.kind, Kind::String | Kind::Null),
                ),
                _ => continue,
            };
            if !ok {
                return Err(BodyError::FieldType { field, expected });
            }
        }
        Ok(Self { raw, members })
    }

    /// The top-level member named `key`. Keys are unique, so there is at most one.
    pub fn get(&self, key: &str) -> Option<&Member<'a>> {
        self.members.iter().find(|m| m.key == key)
    }

    /// The body with top-level `key` set to `value`, a JSON value the caller wrote. An
    /// existing member keeps its position and only its value bytes change; a missing one
    /// is added as the first member. Every other byte is copied unchanged, once. This is
    /// the edit behind the contract's permitted transformations (`model` routing, the
    /// buffered `stream` adaptation); it is not a general editing tool.
    pub fn with_member(&self, key: &str, value: &str) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.raw.len() + key.len() + value.len() + 4);
        match self.get(key) {
            Some(member) => {
                let start = member.raw.as_ptr() as usize - self.raw.as_ptr() as usize;
                out.extend_from_slice(&self.raw[..start]);
                out.extend_from_slice(value.as_bytes());
                out.extend_from_slice(&self.raw[start + member.raw.len()..]);
            }
            None => {
                // The root object's `{` is the first byte that is not whitespace.
                let open = self.raw.iter().position(|&b| b == b'{').unwrap_or(0) + 1;
                out.extend_from_slice(&self.raw[..open]);
                out.extend_from_slice(serde_json::to_string(key).unwrap_or_default().as_bytes());
                out.push(b':');
                out.extend_from_slice(value.as_bytes());
                if !self.members.is_empty() {
                    out.push(b',');
                }
                out.extend_from_slice(&self.raw[open..]);
            }
        }
        out
    }

    /// The body without top-level `key`, every other byte copied unchanged, once. The
    /// member goes with its separating comma: after the previous member's value for any
    /// member but the first, else up to the next member's key. A missing key leaves the
    /// bytes as they are. This serves the WebSocket `type` envelope, the one member a
    /// contract transformation removes; it is not a general editing tool.
    pub fn without_member(&self, key: &str) -> Vec<u8> {
        let Some(index) = self.members.iter().position(|m| m.key == key) else {
            return self.raw.to_vec();
        };
        let offset = |m: &Member<'_>| m.raw.as_ptr() as usize - self.raw.as_ptr() as usize;
        let value_end = |m: &Member<'_>| offset(m) + m.raw.len();
        let member = &self.members[index];
        let (cut_start, cut_end) = if index > 0 {
            (value_end(&self.members[index - 1]), value_end(member))
        } else {
            let open = self.raw.iter().position(|&b| b == b'{').unwrap_or(0) + 1;
            let end = match self.members.get(1) {
                // Past the comma and the whitespace before the next key.
                Some(_) => {
                    let mut at = value_end(member);
                    while at < self.raw.len() && self.raw[at] != b',' {
                        at += 1;
                    }
                    at += 1;
                    while at < self.raw.len() && matches!(self.raw[at], b' ' | b'\t' | b'\n' | b'\r') {
                        at += 1;
                    }
                    at
                }
                None => value_end(member),
            };
            (open, end)
        };
        let mut out = Vec::with_capacity(self.raw.len() - (cut_end - cut_start));
        out.extend_from_slice(&self.raw[..cut_start]);
        out.extend_from_slice(&self.raw[cut_end..]);
        out
    }
}

/// A strict JSON scanner over text already known to be UTF-8. It splits the text only at
/// ASCII bytes, so every slice it takes is on a character boundary.
struct Scanner<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> Scanner<'a> {
    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.at).copied()
    }

    fn malformed(&self) -> BodyError {
        BodyError::Malformed { offset: self.at }
    }

    fn whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn eat(&mut self, byte: u8) -> Result<(), BodyError> {
        if self.peek() != Some(byte) {
            return Err(self.malformed());
        }
        self.at += 1;
        Ok(())
    }

    /// Only whitespace may follow the root value.
    fn end(&mut self) -> Result<(), BodyError> {
        self.whitespace();
        match self.peek() {
            None => Ok(()),
            Some(_) => Err(self.malformed()),
        }
    }

    /// A value whose depth, if it is a container, is `depth`.
    fn value(&mut self, depth: usize) -> Result<Kind, BodyError> {
        match self.peek() {
            Some(b'{') => self.object(depth, None).map(|()| Kind::Object),
            Some(b'[') => self.array(depth).map(|()| Kind::Array),
            Some(b'"') => self.string(false).map(|_| Kind::String),
            Some(b'-' | b'0'..=b'9') => self.number().map(|()| Kind::Number),
            Some(b't') => self.literal("true", Kind::True),
            Some(b'f') => self.literal("false", Kind::False),
            Some(b'n') => self.literal("null", Kind::Null),
            _ => Err(self.malformed()),
        }
    }

    fn literal(&mut self, word: &str, kind: Kind) -> Result<Kind, BodyError> {
        if !self.text[self.at..].starts_with(word) {
            return Err(self.malformed());
        }
        self.at += word.len();
        Ok(kind)
    }

    fn enter(&self, depth: usize) -> Result<(), BodyError> {
        if depth > MAX_DEPTH {
            return Err(BodyError::TooDeep {
                limit: MAX_DEPTH,
                offset: self.at,
            });
        }
        Ok(())
    }

    /// An object; `top` collects its members when it is the root.
    fn object(&mut self, depth: usize, mut top: Option<&mut Vec<Member<'a>>>) -> Result<(), BodyError> {
        self.enter(depth)?;
        self.eat(b'{')?;
        self.whitespace();
        if self.peek() == Some(b'}') {
            self.at += 1;
            return Ok(());
        }
        let mut keys = KeySet::new();
        loop {
            self.whitespace();
            if self.peek() != Some(b'"') {
                return Err(self.malformed());
            }
            let key_at = self.at;
            let mut key = self.string(true)?.unwrap_or_default();
            // Only root members keep their key after the duplicate check.
            let fresh = if top.is_some() {
                keys.insert(key.clone())
            } else {
                keys.insert(std::mem::take(&mut key))
            };
            if !fresh {
                return Err(BodyError::DuplicateKey { offset: key_at });
            }
            self.whitespace();
            self.eat(b':')?;
            self.whitespace();
            let start = self.at;
            let kind = self.value(depth + 1)?;
            if let Some(top) = top.as_deref_mut() {
                let raw = &self.text.as_bytes()[start..self.at];
                top.push(Member { key, kind, raw });
            }
            self.whitespace();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    return Ok(());
                }
                _ => return Err(self.malformed()),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<(), BodyError> {
        self.enter(depth)?;
        self.eat(b'[')?;
        self.whitespace();
        if self.peek() == Some(b']') {
            self.at += 1;
            return Ok(());
        }
        loop {
            self.whitespace();
            self.value(depth + 1)?;
            self.whitespace();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b']') => {
                    self.at += 1;
                    return Ok(());
                }
                _ => return Err(self.malformed()),
            }
        }
    }

    /// A string. With `decode`, returns its unescaped text, borrowed when it has no
    /// escapes; without, only checks it.
    fn string(&mut self, decode: bool) -> Result<Option<Cow<'a, str>>, BodyError> {
        self.eat(b'"')?;
        let mut run = self.at;
        let mut owned: Option<String> = None;
        loop {
            match self.peek() {
                None => return Err(self.malformed()),
                Some(b'"') => {
                    let tail = &self.text[run..self.at];
                    self.at += 1;
                    if !decode {
                        return Ok(None);
                    }
                    return Ok(Some(match owned {
                        Some(mut s) => {
                            s.push_str(tail);
                            Cow::Owned(s)
                        }
                        None => Cow::Borrowed(tail),
                    }));
                }
                Some(b'\\') => {
                    if decode {
                        owned.get_or_insert_with(String::new).push_str(&self.text[run..self.at]);
                    }
                    self.at += 1;
                    let c = self.escape()?;
                    if let Some(s) = owned.as_mut() {
                        s.push(c);
                    }
                    run = self.at;
                }
                Some(0x00..=0x1f) => return Err(self.malformed()),
                Some(_) => self.at += 1,
            }
        }
    }

    /// The character an escape stands for; `at` is just past the backslash.
    fn escape(&mut self) -> Result<char, BodyError> {
        let c = match self.peek() {
            Some(b'"') => '"',
            Some(b'\\') => '\\',
            Some(b'/') => '/',
            Some(b'b') => char::from(0x08),
            Some(b'f') => char::from(0x0c),
            Some(b'n') => '\n',
            Some(b'r') => '\r',
            Some(b't') => '\t',
            Some(b'u') => {
                let start = self.at - 1;
                self.at += 1;
                let unit = self.hex4()?;
                let scalar = match unit {
                    0xd800..=0xdbff => {
                        // A high surrogate must be followed by an escaped low surrogate.
                        if !self.text[self.at..].starts_with('\\')
                            || self.text.as_bytes().get(self.at + 1) != Some(&b'u')
                        {
                            return Err(BodyError::Malformed { offset: start });
                        }
                        self.at += 2;
                        let low = self.hex4()?;
                        if !(0xdc00..=0xdfff).contains(&low) {
                            return Err(BodyError::Malformed { offset: start });
                        }
                        0x10000 + ((u32::from(unit) - 0xd800) << 10) + (u32::from(low) - 0xdc00)
                    }
                    0xdc00..=0xdfff => return Err(BodyError::Malformed { offset: start }),
                    unit => u32::from(unit),
                };
                return char::from_u32(scalar).ok_or(BodyError::Malformed { offset: start });
            }
            _ => return Err(self.malformed()),
        };
        self.at += 1;
        Ok(c)
    }

    fn hex4(&mut self) -> Result<u16, BodyError> {
        let digits = self.text.get(self.at..self.at + 4).ok_or_else(|| self.malformed())?;
        if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(self.malformed());
        }
        let unit = u16::from_str_radix(digits, 16).map_err(|_| self.malformed())?;
        self.at += 4;
        Ok(unit)
    }

    /// The number grammar only: the value is never converted.
    fn number(&mut self) -> Result<(), BodyError> {
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        match self.peek() {
            Some(b'0') => self.at += 1,
            Some(b'1'..=b'9') => self.digits(),
            _ => return Err(self.malformed()),
        }
        if self.peek() == Some(b'.') {
            self.at += 1;
            self.required_digits()?;
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.at += 1;
            }
            self.required_digits()?;
        }
        Ok(())
    }

    fn digits(&mut self) {
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.at += 1;
        }
    }

    fn required_digits(&mut self) -> Result<(), BodyError> {
        if !matches!(self.peek(), Some(b'0'..=b'9')) {
            return Err(self.malformed());
        }
        self.digits();
        Ok(())
    }
}

/// The keys of one object. The first few stay on the stack; a larger object spills to a
/// hash set, so a key check stays constant time however many keys an object has.
struct KeySet<'a> {
    small: [Option<Cow<'a, str>>; 8],
    len: usize,
    large: HashSet<Cow<'a, str>>,
}

impl<'a> KeySet<'a> {
    fn new() -> Self {
        Self {
            small: Default::default(),
            len: 0,
            large: HashSet::new(),
        }
    }

    /// `false` when `key` was already present.
    fn insert(&mut self, key: Cow<'a, str>) -> bool {
        if self.len < self.small.len() {
            if self.small[..self.len].iter().flatten().any(|k| *k == key) {
                return false;
            }
            self.small[self.len] = Some(key);
            self.len += 1;
            return true;
        }
        if self.large.is_empty() {
            self.large.extend(self.small.iter_mut().filter_map(Option::take));
        }
        self.large.insert(key)
    }
}

#[cfg(test)]
#[path = "codex_body_tests.rs"]
mod tests;
