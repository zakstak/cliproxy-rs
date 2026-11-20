//! Go standard-library behaviour the executors' byte handling depends on, where Rust's
//! defaults differ: `bytes.TrimSpace`.

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
    fn trim_space_is_unicode_aware() {
        assert_eq!(trim_space("\u{a0} x \u{3000}\r".as_bytes()), b"x");
        assert_eq!(trim_space(b"\xff "), b"\xff");
    }
}
