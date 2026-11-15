//! Go `encoding/base64` decoding and `google.golang.org/protobuf/encoding/protowire`
//! (v1.34.1) parsing, ported for exact accept/reject behaviour and error text.

/// A base64 alphabet with Go's padding rule, and whether decoding is `Strict` (trailing
/// padding bits must be zero).
#[derive(Clone, Copy)]
pub(crate) struct Encoding {
    url: bool,
    padded: bool,
    strict: bool,
}

pub(crate) const STD: Encoding = Encoding {
    url: false,
    padded: true,
    strict: false,
};
/// `base64.StdEncoding.Strict()`.
pub(crate) const STD_STRICT: Encoding = Encoding { strict: true, ..STD };
pub(crate) const RAW_STD: Encoding = Encoding {
    url: false,
    padded: false,
    strict: false,
};
pub(crate) const URL: Encoding = Encoding {
    url: true,
    padded: true,
    strict: false,
};
pub(crate) const RAW_URL: Encoding = Encoding {
    url: true,
    padded: false,
    strict: false,
};

impl Encoding {
    fn value(self, b: u8) -> Option<u8> {
        match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'+' if !self.url => Some(62),
            b'/' if !self.url => Some(63),
            b'-' if self.url => Some(62),
            b'_' if self.url => Some(63),
            _ => None,
        }
    }

    /// One `decodeQuantum` step: returns the next source index or the
    /// `CorruptInputError` offset.
    fn quantum(self, out: &mut Vec<u8>, src: &[u8], mut si: usize) -> (usize, Result<(), usize>) {
        let mut dbuf = [0u8; 4];
        let mut dlen = 4;
        let mut err = Ok(());
        let mut j = 0;
        while j < 4 {
            if src.len() == si {
                if j == 0 {
                    return (si, Ok(()));
                }
                if j == 1 || self.padded {
                    return (si, Err(si - j));
                }
                dlen = j;
                break;
            }
            let c = src[si];
            si += 1;
            if let Some(v) = self.value(c) {
                dbuf[j] = v;
                j += 1;
                continue;
            }
            if c == b'\n' || c == b'\r' {
                continue;
            }
            if !self.padded || c != b'=' {
                return (si, Err(si - 1));
            }
            match j {
                0 | 1 => return (si, Err(si - 1)),
                2 => {
                    while si < src.len() && (src[si] == b'\n' || src[si] == b'\r') {
                        si += 1;
                    }
                    if si == src.len() {
                        return (si, Err(src.len()));
                    }
                    if src[si] != b'=' {
                        return (si, Err(si - 1));
                    }
                    si += 1;
                }
                _ => {}
            }
            while si < src.len() && (src[si] == b'\n' || src[si] == b'\r') {
                si += 1;
            }
            if si < src.len() {
                err = Err(si);
            }
            dlen = j;
            break;
        }
        let val = u32::from(dbuf[0]) << 18 | u32::from(dbuf[1]) << 12 | u32::from(dbuf[2]) << 6 | u32::from(dbuf[3]);
        let bytes = [(val >> 16) as u8, (val >> 8) as u8, val as u8];
        // Strict: the bits past the last decoded byte must be zero.
        if self.strict {
            match dlen {
                3 if bytes[2] != 0 => return (si, Err(si - 1)),
                2 if bytes[1] != 0 || bytes[2] != 0 => return (si, Err(si - 2)),
                _ => {}
            }
        }
        if dlen >= 2 {
            out.extend_from_slice(&bytes[..dlen - 1]);
        }
        (si, err)
    }

    /// `Encoding.DecodeString`. `Err` carries Go's error text.
    pub(crate) fn decode(self, src: &[u8]) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(src.len() / 4 * 3 + 3);
        let mut si = 0;
        while si < src.len() {
            let (next, result) = self.quantum(&mut out, src, si);
            if let Err(at) = result {
                return Err(format!("illegal base64 data at input byte {at}"));
            }
            si = next;
        }
        Ok(out)
    }
}

/// `base64.StdEncoding.EncodeToString`.
pub(crate) fn std_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub(crate) const VARINT: u8 = 0;
pub(crate) const FIXED64: u8 = 1;
pub(crate) const BYTES: u8 = 2;
pub(crate) const START_GROUP: u8 = 3;
pub(crate) const END_GROUP: u8 = 4;
pub(crate) const FIXED32: u8 = 5;

/// protowire negative error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WireError {
    Truncated,
    FieldNumber,
    Overflow,
    Reserved,
    EndGroup,
    RecursionDepth,
}

impl WireError {
    /// `protowire.ParseError(n).Error()`. The `proto:` prefix uses a regular space;
    /// Go picks a regular or non-breaking space per binary.
    pub(crate) fn text(self) -> &'static str {
        match self {
            Self::Truncated => "unexpected EOF",
            Self::FieldNumber => "proto: invalid field number",
            Self::Overflow => "proto: variable length integer overflow",
            Self::Reserved => "proto: cannot parse reserved wire type",
            Self::EndGroup => "proto: mismatching end group marker",
            Self::RecursionDepth => "proto: parse error",
        }
    }
}

pub(crate) type Wire<T> = Result<(T, usize), WireError>;

pub(crate) fn consume_varint(b: &[u8]) -> Wire<u64> {
    let mut v: u64 = 0;
    for (i, &byte) in b.iter().enumerate().take(10) {
        if i == 9 {
            if byte < 2 {
                return Ok((v.wrapping_add(u64::from(byte) << 63), 10));
            }
            return Err(WireError::Overflow);
        }
        v = v.wrapping_add(u64::from(byte & 0x7f) << (7 * i));
        if byte < 0x80 {
            return Ok((v, i + 1));
        }
    }
    Err(WireError::Truncated)
}

/// `ConsumeTag`: field number and wire type.
pub(crate) fn consume_tag(b: &[u8]) -> Wire<(i64, u8)> {
    let (v, n) = consume_varint(b)?;
    let num = if v >> 3 > i32::MAX as u64 { -1 } else { (v >> 3) as i64 };
    if num < 1 {
        return Err(WireError::FieldNumber);
    }
    Ok(((num, (v & 7) as u8), n))
}

pub(crate) fn consume_bytes(b: &[u8]) -> Wire<&[u8]> {
    let (m, n) = consume_varint(b)?;
    if m > (b.len() - n) as u64 {
        return Err(WireError::Truncated);
    }
    let m = m as usize;
    Ok((&b[n..n + m], n + m))
}

pub(crate) fn consume_fixed32(b: &[u8]) -> Wire<()> {
    if b.len() < 4 {
        Err(WireError::Truncated)
    } else {
        Ok(((), 4))
    }
}

pub(crate) fn consume_fixed64(b: &[u8]) -> Wire<()> {
    if b.len() < 8 {
        Err(WireError::Truncated)
    } else {
        Ok(((), 8))
    }
}

/// `ConsumeFieldValue`: the length of the value after a tag.
pub(crate) fn consume_field_value(num: i64, typ: u8, b: &[u8]) -> Result<usize, WireError> {
    field_value(num, typ, b, 10_000)
}

fn field_value(num: i64, typ: u8, mut b: &[u8], depth: i64) -> Result<usize, WireError> {
    match typ {
        VARINT => consume_varint(b).map(|(_, n)| n),
        FIXED32 => consume_fixed32(b).map(|(_, n)| n),
        FIXED64 => consume_fixed64(b).map(|(_, n)| n),
        BYTES => consume_bytes(b).map(|(_, n)| n),
        START_GROUP => {
            if depth < 0 {
                return Err(WireError::RecursionDepth);
            }
            let start = b.len();
            loop {
                let ((num2, typ2), n) = consume_tag(b)?;
                b = &b[n..];
                if typ2 == END_GROUP {
                    if num != num2 {
                        return Err(WireError::EndGroup);
                    }
                    return Ok(start - b.len());
                }
                let n = field_value(num2, typ2, b, depth - 1)?;
                b = &b[n..];
            }
        }
        END_GROUP => Err(WireError::EndGroup),
        _ => Err(WireError::Reserved),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4648 section 10 examples plus our explicit signature-input policies.
    #[test]
    fn base64_decodes_rfc_examples_and_refuses_malformed_encodings() {
        for (encoded, plain) in [
            ("", ""),
            ("Zg==", "f"),
            ("Zm8=", "fo"),
            ("Zm9v", "foo"),
            ("Zm9vYg==", "foob"),
            ("Zm9vYmE=", "fooba"),
            ("Zm9vYmFy", "foobar"),
        ] {
            assert_eq!(STD.decode(encoded.as_bytes()).unwrap(), plain.as_bytes());
            assert_eq!(STD_STRICT.decode(encoded.as_bytes()).unwrap(), plain.as_bytes());
        }
        // MIME-style line breaks are accepted; strictness applies to padding bits.
        assert_eq!(STD.decode(b"TW\r\nE=").unwrap(), b"Ma");
        assert_eq!(STD.decode(b"TWF=").unwrap(), b"Ma");
        for bad in [b"TWF=".as_slice(), b"TWB=", b"TR==", b"TWF=\n\n"] {
            assert!(STD_STRICT.decode(bad).is_err());
        }
        for bad in [b"TWE".as_slice(), b"TWE=x", b"T===", b"-_8="] {
            assert!(STD.decode(bad).is_err());
        }
        assert_eq!(RAW_STD.decode(b"TWE").unwrap(), b"Ma");
        assert_eq!(RAW_URL.decode(b"-_8").unwrap(), [0xfb, 0xff]);
        assert!(RAW_STD.decode(b"TWE=").is_err());
        assert!(RAW_STD.decode(b"T").is_err());
    }

    #[test]
    fn protobuf_wire_examples_return_typed_parse_errors() {
        assert_eq!(consume_varint(&[0x96, 0x01]), Ok((150, 2)));
        assert_eq!(consume_varint(&[0x80]), Err(WireError::Truncated));
        assert_eq!(consume_varint(&[0xff; 10]), Err(WireError::Overflow));
        assert_eq!(
            consume_varint(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]),
            Ok((u64::MAX, 10))
        );
        assert_eq!(consume_tag(&[0x00]), Err(WireError::FieldNumber));
        assert_eq!(consume_tag(&[0x12]), Ok(((2, BYTES), 1)));
        assert_eq!(consume_bytes(&[0x03, 1, 2]), Err(WireError::Truncated));
        // Group 1 containing varint field 2, then end group 1.
        assert_eq!(consume_field_value(1, START_GROUP, &[0x10, 0x05, 0x0c]), Ok(3));
        assert_eq!(consume_field_value(1, START_GROUP, &[0x14]), Err(WireError::EndGroup));
        assert_eq!(consume_field_value(1, 6, &[]), Err(WireError::Reserved));
    }
}
