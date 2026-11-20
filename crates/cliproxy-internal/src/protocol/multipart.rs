//! Multipart delimiter and line scanning shared by uploads and realtime calls.

use memchr::{memchr, memmem, memrchr};

pub(crate) fn read_line<'a>(body: &'a [u8], pos: &mut usize) -> &'a [u8] {
    let rest = &body[*pos..];
    let len = memchr(b'\n', rest).map_or(rest.len(), |i| i + 1);
    *pos += len;
    &rest[..len]
}

pub(crate) fn skip_lwsp(bytes: &[u8]) -> &[u8] {
    &bytes[bytes.iter().take_while(|c| matches!(c, b' ' | b'\t')).count()..]
}

pub(crate) struct Boundary {
    // CRLF followed by the two dashes and the configured boundary.
    marker: Vec<u8>,
    newline: &'static [u8],
    started: bool,
}

pub(crate) enum Scan {
    /// Consume these bytes. Zero means the scanner needs a larger window.
    Data(usize),
    Boundary(usize),
    UnexpectedEof(usize),
}

impl Boundary {
    pub(crate) fn new(value: &str) -> Self {
        Self {
            marker: format!("\r\n--{value}").into_bytes(),
            newline: b"\r\n",
            started: false,
        }
    }

    fn dash(&self) -> &[u8] {
        &self.marker[2..]
    }

    fn line_marker(&self) -> &[u8] {
        if self.newline == b"\n" {
            &self.marker[1..]
        } else {
            &self.marker
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.dash() == b"--"
    }

    pub(crate) fn started(&self) -> bool {
        self.started
    }

    pub(crate) fn newline(&self) -> &[u8] {
        self.newline
    }

    pub(crate) fn is_final(&self, line: &[u8]) -> bool {
        line.strip_prefix(self.dash())
            .and_then(|rest| rest.strip_prefix(b"--"))
            .map(skip_lwsp)
            .is_some_and(|rest| rest.is_empty() || rest == self.newline)
    }

    pub(crate) fn start_part(&mut self, line: &[u8]) -> bool {
        let Some(rest) = line.strip_prefix(self.dash()).map(skip_lwsp) else {
            return false;
        };
        if !self.started && rest == b"\n" {
            self.newline = b"\n";
        }
        if rest != self.newline {
            return false;
        }
        self.started = true;
        true
    }

    /// Offsets and lengths of possible delimiters. The reader checks the suffix
    /// using the bytes its input window makes available.
    pub(crate) fn candidates<'a>(&'a self, buf: &'a [u8], at_start: bool) -> impl Iterator<Item = (usize, usize)> + 'a {
        let first = (at_start && buf.starts_with(self.dash())).then_some((0, self.dash().len()));
        let marker = self.line_marker();
        first
            .into_iter()
            .chain(memmem::find_iter(buf, marker).map(move |i| (i, marker.len())))
    }

    pub(crate) fn scan_window(&self, buf: &[u8], at_start: bool, ended: bool) -> Scan {
        let exhausted = |n| if ended { Scan::UnexpectedEof(n) } else { Scan::Data(n) };
        if at_start && buf.len() < self.dash().len() && self.dash().starts_with(buf) {
            return exhausted(0);
        }
        if let Some((i, len)) = self.candidates(buf, at_start).next() {
            let tail = &buf[i + len..];
            return match tail {
                [] if ended => Scan::Boundary(i),
                [] => Scan::Data(i),
                [b' ' | b'\t' | b'\r' | b'\n', ..] | [b'-', b'-', ..] => Scan::Boundary(i),
                [b'-'] if !ended => Scan::Data(i),
                _ => Scan::Data(i + len),
            };
        }
        let marker = self.line_marker();
        if marker.starts_with(buf) {
            return exhausted(0);
        }
        if let Some(i) = memrchr(marker[0], buf)
            && marker.starts_with(&buf[i..])
        {
            return Scan::Data(i);
        }
        exhausted(buf.len())
    }

    /// Return part data, leaving the cursor on the delimiter. On premature EOF
    /// return the available data so transfer decoders can report their own errors.
    pub(crate) fn read_part<'a>(&self, body: &'a [u8], pos: &mut usize) -> Result<&'a [u8], &'a [u8]> {
        let start = *pos;
        loop {
            match self.scan_window(&body[*pos..], *pos == start, true) {
                Scan::Data(n) => *pos += n,
                Scan::Boundary(n) => {
                    *pos += n;
                    return Ok(&body[start..*pos]);
                }
                Scan::UnexpectedEof(n) => {
                    *pos += n;
                    return Err(&body[start..*pos]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_dash_can_start_a_closing_delimiter_in_the_next_window() {
        let boundary = Boundary::new("part");
        let window = b"text\r\n--part-";
        assert!(matches!(boundary.scan_window(window, true, false), Scan::Data(4)));
        let mut pos = 0;
        let body = b"text\r\n--part-x\r\n--part--";
        assert_eq!(boundary.read_part(body, &mut pos), Ok(&b"text\r\n--part-x"[..]));
        assert_eq!(&body[pos..], b"\r\n--part--");
    }

    #[test]
    fn a_truncated_delimiter_is_excluded_from_the_available_part_data() {
        let mut boundary = Boundary::new("part");
        assert!(boundary.start_part(b"--part\n"));
        let mut pos = 0;
        let body = b"data\n--pa";
        assert_eq!(boundary.read_part(body, &mut pos), Err(&b"data"[..]));
        assert_eq!(&body[pos..], b"\n--pa");
    }
}
