//! The authentication values one upstream request or socket actually carried, and how
//! to find them in what came back (#25, acceptance criterion 7; `docs/codex/CONTRACT.md`
//! "Credential isolation").
//!
//! An upstream can write the value it was sent into its answer: an error message that
//! quotes the bearer token, a close reason, a header. The native Codex routes check every
//! delivery against [`SentCredentials`] (`cpa_exec::codex_secret`), and the request log
//! and the usage record remove them before they keep anything ([`SentCredentials::scrub`]).
//! The values are bound to one upstream request or socket: never a global set of
//! configured accounts, saved logins or a credential name.

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;

/// Unescape rounds after the bytes themselves: a JSON string, a raw value's string inside
/// a JSON string, and so on. Each round halves a run of backslashes, so a value hidden
/// under more rounds than this needs a run of 65,536 backslashes; such bytes count as
/// carrying a value ([`SentCredentials::exposes`]).
// ponytail: at most 16 unescape rounds bound allocation and work; deeper encodings
// fail closed. Raise this ceiling only with an updated work bound and nested-escape tests.
const ROUNDS: usize = 16;

/// What [`SentCredentials::scrub`] writes in place of a value.
pub const REDACTED: &str = "[REDACTED]";

/// The authentication values one upstream request or socket carried. Empty values are
/// none. Its `Debug` names only how many there are; the default carries none.
// A thin pointer: a failure carries it (`Delivery::Confidential`), and `ExecError`
// travels in `Result`s and must stay small.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SentCredentials(Option<Arc<Vec<Box<str>>>>);

impl fmt::Debug for SentCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SentCredentials({} redacted)", self.values().len())
    }
}

impl SentCredentials {
    /// `values`, without empty ones and repeats (surrounding whitespace is no part of a
    /// value).
    pub fn new<'a>(values: impl IntoIterator<Item = &'a str>) -> Self {
        let mut out: Vec<Box<str>> = Vec::new();
        for value in values {
            let value = value.trim();
            if !value.is_empty() && !out.iter().any(|s| &**s == value) {
                out.push(value.into());
            }
        }
        if out.is_empty() {
            Self(None)
        } else {
            Self(Some(Arc::new(out)))
        }
    }

    fn values(&self) -> &[Box<str>] {
        self.0.as_deref().map_or(&[], Vec::as_slice)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_none()
    }

    /// Whether `bytes` carry one of the values: as written, or under up to [`ROUNDS`]
    /// rounds of JSON unescaping (`\"`, `\\`, `\/`, the control escapes and `\uXXXX`,
    /// surrogate pairs included). Unescaping runs over every byte, inside strings or not,
    /// so a frame no JSON reader accepts is read too. Bytes still unescaping after the
    /// last round count as carrying one. Linear in `bytes` for each round, and a round
    /// runs only while the previous one changed something.
    pub fn exposes(&self, bytes: &[u8]) -> bool {
        let values = self.values();
        if values.is_empty() || bytes.is_empty() {
            return false;
        }
        let mut layer = String::from_utf8_lossy(bytes);
        for _ in 0..=ROUNDS {
            if values.iter().any(|value| layer.contains(&**value)) {
                return true;
            }
            match unescape(&layer) {
                Some(next) => layer = Cow::Owned(next),
                None => return false,
            }
        }
        true
    }

    /// `bytes` as a log or a record may keep them. Bytes that carry no value
    /// ([`Self::exposes`]) stay as they are, borrowed. Otherwise each value written as
    /// sent becomes [`REDACTED`], which keeps the rest readable; when a value is still
    /// there after that, in an escaped spelling, nothing of `bytes` is kept but their
    /// length. Bytes that are not UTF-8 are read as [`String::from_utf8_lossy`] reads them
    /// once they carry a value.
    pub fn scrub<'a>(&self, bytes: &'a [u8]) -> Cow<'a, [u8]> {
        if !self.exposes(bytes) {
            return Cow::Borrowed(bytes);
        }
        let mut text = String::from_utf8_lossy(bytes).into_owned();
        // The longest first, so a value inside another is not split from it.
        let mut values: Vec<&str> = self.values().iter().map(|v| &**v).collect();
        values.sort_by_key(|v| std::cmp::Reverse(v.len()));
        for value in values {
            if text.contains(value) {
                text = text.replace(value, REDACTED);
            }
        }
        if self.exposes(text.as_bytes()) {
            return Cow::Owned(
                format!("[REDACTED: {} bytes carried an upstream credential]", bytes.len()).into_bytes(),
            );
        }
        Cow::Owned(text.into_bytes())
    }

    /// [`Self::scrub`] for text.
    pub fn scrub_str<'a>(&self, text: &'a str) -> Cow<'a, str> {
        match self.scrub(text.as_bytes()) {
            Cow::Borrowed(_) => Cow::Borrowed(text),
            // `scrub` builds owned bytes from a `String`.
            Cow::Owned(bytes) => Cow::Owned(String::from_utf8(bytes).unwrap_or_default()),
        }
    }
}

/// One round of JSON unescaping over all of `text`, or `None` when nothing changes. An
/// escape JSON does not define, and a lone surrogate, stay as written.
fn unescape(text: &str) -> Option<String> {
    let b = text.as_bytes();
    let first = b.iter().position(|&c| c == b'\\')?;
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..first]);
    let mut changed = false;
    let mut i = first;
    let mut start = first;
    while i < b.len() {
        if b[i] != b'\\' {
            i += 1;
            continue;
        }
        out.push_str(&text[start..i]);
        let (decoded, used) = match b.get(i + 1) {
            Some(b'"') => (Some('"'), 2),
            Some(b'\\') => (Some('\\'), 2),
            Some(b'/') => (Some('/'), 2),
            Some(b'b') => (Some('\u{8}'), 2),
            Some(b'f') => (Some('\u{c}'), 2),
            Some(b'n') => (Some('\n'), 2),
            Some(b'r') => (Some('\r'), 2),
            Some(b't') => (Some('\t'), 2),
            Some(b'u') => unicode(b, i),
            _ => (None, 1),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                changed = true;
            }
            None => out.push_str(&text[i..i + used]),
        }
        i += used;
        start = i;
    }
    out.push_str(&text[start..]);
    changed.then_some(out)
}

/// The `\uXXXX` escape at `i` (a surrogate pair takes two), and the bytes it spans.
fn unicode(b: &[u8], i: usize) -> (Option<char>, usize) {
    let unit = |at: usize| -> Option<u32> {
        let hex = b.get(at..at + 4)?;
        let hex = std::str::from_utf8(hex).ok()?;
        u32::from_str_radix(hex, 16)
            .ok()
            .filter(|_| hex.bytes().all(|c| c.is_ascii_hexdigit()))
    };
    let Some(high) = unit(i + 2) else {
        return (None, 2);
    };
    if (0xD800..0xDC00).contains(&high) {
        if b.get(i + 6) == Some(&b'\\')
            && b.get(i + 7) == Some(&b'u')
            && let Some(low) = unit(i + 8).filter(|l| (0xDC00..0xE000).contains(l))
        {
            let c = 0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00);
            return (char::from_u32(c), 12);
        }
        return (None, 6);
    }
    (char::from_u32(high), 6)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent() -> SentCredentials {
        SentCredentials::new(["at-FAKE", "", "  ", "at-FAKE", "cookie-AUTH"])
    }

    #[test]
    fn empty_and_repeated_values_are_none() {
        assert_eq!(format!("{:?}", sent()), "SentCredentials(2 redacted)");
        assert!(SentCredentials::new(["", " "]).is_empty());
        assert!(!SentCredentials::default().exposes(b"anything"));
        assert_eq!(
            SentCredentials::default().scrub(b"at-FAKE"),
            Cow::Borrowed(b"at-FAKE".as_slice())
        );
    }

    #[test]
    fn scrub_keeps_safe_bytes_and_replaces_values_written_as_sent() {
        let s = sent();
        let safe = br#"{ "message":"at-OTHER", "n":1.50 }"#;
        assert!(matches!(s.scrub(safe), Cow::Borrowed(b) if b == safe));
        assert_eq!(
            s.scrub(br#"{"message":"quoted at-FAKE and cookie-AUTH at-FAKE","n":1.50}"#)
                .as_ref(),
            br#"{"message":"quoted [REDACTED] and [REDACTED] [REDACTED]","n":1.50}"#
        );
        assert_eq!(s.scrub_str("error: at-FAKE"), "error: [REDACTED]");
        assert_eq!(s.scrub_str("error: at-OTHER"), "error: at-OTHER");
    }

    #[test]
    fn scrub_keeps_only_the_length_of_an_escaped_spelling() {
        let s = sent();
        for raw in [
            r#"{"message":"at\u002dFAKE"}"#,
            r#"{"raw":"{\"m\":\"at\\u002dFAKE\"}"}"#,
            r#"{"m":"\u0061t-FAKE plus at-FAKE"}"#,
        ] {
            let out = s.scrub(raw.as_bytes());
            assert!(!s.exposes(&out), "{raw}");
            assert_eq!(
                String::from_utf8(out.into_owned()).unwrap(),
                format!("[REDACTED: {} bytes carried an upstream credential]", raw.len())
            );
        }
    }

    #[test]
    fn a_value_inside_another_is_redacted_whole() {
        let s = SentCredentials::new(["abc", "xxabcxx"]);
        assert_eq!(s.scrub_str("1 xxabcxx 2 abc"), "1 [REDACTED] 2 [REDACTED]");
    }
}
