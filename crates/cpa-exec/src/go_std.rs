//! Go standard-library behaviour the executors' byte handling depends on, where Rust's
//! defaults differ: `encoding/json.Valid` and `bytes.TrimSpace`.

/// encoding/json's `maxNestingDepth`.
const MAX_DEPTH: usize = 10_000;

/// `json.Valid`: RFC 8259 grammar, string bytes unchecked for UTF-8, nesting capped at
/// 10,000. Iterative, so hostile depth cannot exhaust the stack.
pub(crate) fn json_valid(b: &[u8]) -> bool {
    let ws = |mut i: usize| {
        while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\r' | b'\n') {
            i += 1;
        }
        i
    };
    let string = |mut i: usize| -> Option<usize> {
        if b.get(i) != Some(&b'"') {
            return None;
        }
        i += 1;
        loop {
            match *b.get(i)? {
                b'"' => return Some(i + 1),
                b'\\' => match *b.get(i + 1)? {
                    b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => i += 2,
                    b'u' => {
                        let hex = b.get(i + 2..i + 6)?;
                        if !hex.iter().all(u8::is_ascii_hexdigit) {
                            return None;
                        }
                        i += 6;
                    }
                    _ => return None,
                },
                c if c < 0x20 => return None,
                _ => i += 1,
            }
        }
    };
    let number = |mut i: usize| -> Option<usize> {
        if b.get(i) == Some(&b'-') {
            i += 1;
        }
        match *b.get(i)? {
            b'0' => i += 1,
            b'1'..=b'9' => {
                while b.get(i).is_some_and(u8::is_ascii_digit) {
                    i += 1;
                }
            }
            _ => return None,
        }
        if b.get(i) == Some(&b'.') {
            i += 1;
            if !b.get(i).is_some_and(u8::is_ascii_digit) {
                return None;
            }
            while b.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
        }
        if matches!(b.get(i), Some(b'e' | b'E')) {
            i += 1;
            if matches!(b.get(i), Some(b'+' | b'-')) {
                i += 1;
            }
            if !b.get(i).is_some_and(u8::is_ascii_digit) {
                return None;
            }
            while b.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
        }
        Some(i)
    };
    let mut stack: Vec<u8> = Vec::new();
    let mut i = ws(0);
    'value: loop {
        // Parse one value starting at `i`.
        match b.get(i) {
            Some(b'{') | Some(b'[') => {
                let open = b[i];
                stack.push(open);
                if stack.len() > MAX_DEPTH {
                    return false;
                }
                i = ws(i + 1);
                let close = if open == b'{' { b'}' } else { b']' };
                if b.get(i) == Some(&close) {
                    stack.pop();
                    i += 1;
                } else if open == b'{' {
                    let Some(next) = string(i) else { return false };
                    i = ws(next);
                    if b.get(i) != Some(&b':') {
                        return false;
                    }
                    i = ws(i + 1);
                    continue 'value;
                } else {
                    continue 'value;
                }
            }
            Some(b'"') => match string(i) {
                Some(next) => i = next,
                None => return false,
            },
            Some(b't') if b[i..].starts_with(b"true") => i += 4,
            Some(b'f') if b[i..].starts_with(b"false") => i += 5,
            Some(b'n') if b[i..].starts_with(b"null") => i += 4,
            Some(b'-' | b'0'..=b'9') => match number(i) {
                Some(next) => i = next,
                None => return false,
            },
            _ => return false,
        }
        // After a value: close containers or move to the next element.
        loop {
            i = ws(i);
            match stack.last() {
                None => return i == b.len(),
                Some(&open) => {
                    let close = if open == b'{' { b'}' } else { b']' };
                    match b.get(i) {
                        Some(c) if *c == close => {
                            stack.pop();
                            i += 1;
                        }
                        Some(b',') => {
                            i = ws(i + 1);
                            if open == b'{' {
                                let Some(next) = string(i) else { return false };
                                i = ws(next);
                                if b.get(i) != Some(&b':') {
                                    return false;
                                }
                                i = ws(i + 1);
                            }
                            continue 'value;
                        }
                        _ => return false,
                    }
                }
            }
        }
    }
}

/// `bytes.TrimSpace`: Unicode white space at both ends; invalid UTF-8 stops trimming.
pub(crate) fn trim_space(b: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < b.len() {
        match leading_char(&b[start..]) {
            Some((c, n)) if c.is_whitespace() => start += n,
            _ => break,
        }
    }
    let mut end = b.len();
    while end > start {
        match trailing_char(&b[start..end]) {
            Some((c, n)) if c.is_whitespace() => end -= n,
            _ => break,
        }
    }
    &b[start..end]
}

fn leading_char(b: &[u8]) -> Option<(char, usize)> {
    let len = match b.first()? {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => return None,
    };
    let c = std::str::from_utf8(b.get(..len)?).ok()?.chars().next()?;
    Some((c, len))
}

fn trailing_char(b: &[u8]) -> Option<(char, usize)> {
    for len in 1..=4.min(b.len()) {
        if let Ok(s) = std::str::from_utf8(&b[b.len() - len..])
            && let Some(c) = s.chars().next()
            && c.len_utf8() == len
        {
            return Some((c, len));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_valid_matches_go_grammar_and_depth() {
        for ok in ["{}", " [1, -0.5e+3, \"a\\u00e9\", true, null, {\"k\": []}] "] {
            assert!(json_valid(ok.as_bytes()), "{ok}");
        }
        for bad in [
            "",
            "{",
            "[1,]",
            "{\"a\"}",
            "01",
            "1.",
            "\"\u{1}\"",
            "nul",
            "[1] x",
            "{\"a\":1,}",
        ] {
            assert!(!json_valid(bad.as_bytes()), "{bad}");
        }
        assert!(
            json_valid(b"{\"a\":\"\xff\"}"),
            "string bytes are not checked for UTF-8"
        );
        let deep = |n| format!("{}0{}", "[".repeat(n), "]".repeat(n));
        assert!(json_valid(deep(10_000).as_bytes()));
        assert!(!json_valid(deep(10_001).as_bytes()));
        assert!(!json_valid("[".repeat(1_000_000).as_bytes()));
    }

    #[test]
    fn trim_space_is_unicode_aware() {
        assert_eq!(trim_space("\u{a0} x \u{3000}\r".as_bytes()), b"x");
        assert_eq!(trim_space(b"\xff "), b"\xff");
    }
}
