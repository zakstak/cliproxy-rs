//! Go's table-driven rune functions: `unicode.ToLower`, `unicode.ToUpper`,
//! `strings.ToLower`, `strings.ToUpper`, `strings.EqualFold`, `strconv.IsPrint` and
//! `strconv.Quote`. They run on Go's own
//! Unicode tables (gostr_tables.rs, generated from Go 1.26), so results do not drift
//! with Rust's Unicode version or its full (multi-character) case mappings.

use crate::protocol::json::decode_rune;
use crate::text_tables::{CASE_ORBIT, CASE_RANGES, PRINT_RANGES};

const MAX_RUNE: u32 = 0x10FFFF;
const UPPER: usize = 0;
const LOWER: usize = 1;

/// `unicode.To(case, r)` over `unicode.CaseRanges`.
fn to_case(case: usize, r: u32) -> u32 {
    let Ok(i) = CASE_RANGES.binary_search_by(|&(lo, hi, ..)| {
        if hi < r {
            std::cmp::Ordering::Less
        } else if lo > r {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    }) else {
        return r;
    };
    let (lo, _, upper, lower, title) = CASE_RANGES[i];
    let delta = [upper, lower, title][case];
    if delta > MAX_RUNE as i32 {
        // Alternating Upper/Lower run: even offsets are upper case, odd are lower.
        return lo + (((r - lo) & !1) | (case as u32 & 1));
    }
    r.wrapping_add_signed(delta)
}

/// `unicode.ToLower`.
pub fn to_lower_rune(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    char::from_u32(to_case(LOWER, c as u32)).unwrap_or(c)
}

/// `unicode.ToUpper`.
pub fn to_upper_rune(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_uppercase();
    }
    char::from_u32(to_case(UPPER, c as u32)).unwrap_or(c)
}

/// `unicode.SimpleFold`.
fn simple_fold(r: u32) -> u32 {
    if let Ok(i) = CASE_ORBIT.binary_search_by_key(&r, |&(from, _)| from) {
        return CASE_ORBIT[i].1;
    }
    let Some(c) = char::from_u32(r) else { return r };
    let lower = to_lower_rune(c);
    if lower != c {
        lower as u32
    } else {
        to_upper_rune(c) as u32
    }
}

/// Preallocates the input's length; usually one allocation, though a few simple
/// mappings grow a rune's UTF-8 length (`Ⱥ` to `ⱥ` and `ȿ` to `Ȿ` go from 2 to 3 bytes).
fn map_runes(s: &str, f: fn(char) -> char) -> String {
    let mut out = String::with_capacity(s.len());
    out.extend(s.chars().map(f));
    out
}

/// Go's string comparisons, as methods so call sites read like the Go they port.
pub trait GoStr {
    /// `strings.ToLower`: rune-wise simple lowercase (no context, no expansion).
    fn go_lower(&self) -> String;
    /// `strings.ToUpper`: rune-wise simple uppercase (`ß` stays `ß`).
    fn go_upper(&self) -> String;
    /// `strings.EqualFold`: simple Unicode case folding.
    fn go_eq_fold(&self, other: &str) -> bool;
}

impl GoStr for str {
    fn go_lower(&self) -> String {
        map_runes(self, to_lower_rune)
    }

    fn go_upper(&self) -> String {
        map_runes(self, to_upper_rune)
    }

    fn go_eq_fold(&self, other: &str) -> bool {
        let mut t = other.chars();
        for s in self.chars() {
            let Some(t) = t.next() else { return false };
            if s == t {
                continue;
            }
            let (mut sr, mut tr) = (s as u32, t as u32);
            if tr < sr {
                std::mem::swap(&mut sr, &mut tr);
            }
            if tr < 0x80 {
                if (u32::from(b'A')..=u32::from(b'Z')).contains(&sr) && tr == sr + 32 {
                    continue;
                }
                return false;
            }
            let mut r = simple_fold(sr);
            while r != sr && r < tr {
                r = simple_fold(r);
            }
            if r != tr {
                return false;
            }
        }
        t.next().is_none()
    }
}

/// `strconv.IsPrint`.
pub fn is_print(c: char) -> bool {
    let r = c as u32;
    PRINT_RANGES
        .binary_search_by(|&(lo, hi)| {
            if hi < r {
                std::cmp::Ordering::Less
            } else if lo > r {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// `strconv.Quote`, which is what `%q` prints for a Go string (bytes; invalid UTF-8
/// prints as `\xNN`).
pub fn quote(s: impl AsRef<[u8]>) -> String {
    let s = s.as_ref();
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for chunk in s.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '"' | '\\' => {
                    out.push('\\');
                    out.push(c);
                }
                c if is_print(c) => out.push(c),
                '\u{7}' => out.push_str("\\a"),
                '\u{8}' => out.push_str("\\b"),
                '\u{c}' => out.push_str("\\f"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '\u{b}' => out.push_str("\\v"),
                c if c < ' ' || c == '\u{7f}' => out.push_str(&format!("\\x{:02x}", c as u32)),
                c if (c as u32) < 0x10000 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push_str(&format!("\\U{:08x}", c as u32)),
            }
        }
        for b in chunk.invalid() {
            out.push_str(&format!("\\x{b:02x}"));
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known provider names have symmetric case identity and idempotent normalization.
    #[test]
    fn ascii_protocol_names_have_stable_case_identity() {
        // Unicode alias compatibility is covered where names enter credentials and
        // routing. Do not freeze an entire language runtime's Unicode tables here.
        for (lower, upper) in [
            ("claude", "CLAUDE"),
            ("codex", "CODEX"),
            ("xai", "XAI"),
            ("meta", "META"),
        ] {
            assert_eq!(upper.go_lower(), lower);
            assert_eq!(lower.go_upper(), upper);
            assert_eq!(lower.go_lower().go_lower(), lower);
            assert!(lower.go_eq_fold(upper) && upper.go_eq_fold(lower));
            assert!(!lower.go_eq_fold(&format!("{lower}-other")));
        }
    }
}

/// `strings.TrimSpace` on a Go byte string: Unicode white space at both ends; invalid
/// UTF-8 is never space.
pub fn trim_space(b: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < b.len() {
        match decode_rune(&b[start..]) {
            (Some(c), n) if c.is_whitespace() => start += n,
            _ => break,
        }
    }
    let mut end = b.len();
    while end > start {
        let mut i = end - 1;
        while i > start && end - i < 4 && b[i] & 0xC0 == 0x80 {
            i -= 1;
        }
        match decode_rune(&b[i..end]) {
            (Some(c), n) if i + n == end && c.is_whitespace() => end = i,
            _ => break,
        }
    }
    &b[start..end]
}

/// `strings.ToLower` on a Go byte string. Invalid bytes become U+FFFD as in Go.
pub fn lower_bytes(b: &[u8]) -> String {
    String::from_utf8_lossy(b).go_lower()
}

/// Go `url.QueryEscape`: unreserved bytes kept, space as `+`, everything else `%XX`.
pub fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(char::from(b)),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Go `url.Values.Encode` for single-valued pairs: sorted by key, `key=value` joined with
/// `&`, both query-escaped.
pub fn values_encode(pairs: &[(&str, &str)]) -> String {
    let mut sorted: Vec<&(&str, &str)> = pairs.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    sorted
        .iter()
        .map(|(k, v)| format!("{}={}", query_escape(k), query_escape(v)))
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod byte_tests {
    use super::*;
    #[test]
    fn form_encoding_escapes_delimiters_utf8_and_spaces() {
        assert_eq!(query_escape("a b/c?d=é~"), "a+b%2Fc%3Fd%3D%C3%A9~");
        assert_eq!(values_encode(&[("z", "1"), ("a b", "x&y")]), "a+b=x%26y&z=1");
        for plain in ["", "abcXYZ012-_.~"] {
            assert_eq!(query_escape(plain), plain);
        }
    }
    #[test]
    fn diagnostic_quotes_hide_controls_and_trim_preserves_invalid_bytes() {
        for input in ["line\nnext", "\r\t\0", "\u{1f}quote\""] {
            let output = quote(input);
            assert!(output.starts_with('"') && output.ends_with('"'));
            assert!(!output.chars().any(char::is_control));
        }
        assert_eq!(trim_space(b"\xc2\xa0 a b\t\xe3\x80\x80"), b"a b");
        assert_eq!(trim_space(b" \xff "), b"\xff");
        assert_eq!(trim_space(b"\xe3\x80"), b"\xe3\x80");
    }
}
