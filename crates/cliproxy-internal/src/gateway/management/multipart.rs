//! Credential uploads as gin's `c.MultipartForm()` reads them: Go's
//! `mime.ParseMediaType`, `mime/multipart.Reader` and the `textproto` header rules,
//! applied to a body that is already fully in memory.
//!
//! ponytail: no quoted-printable part decoding and no header count or size limits.

use crate::protocol::multipart::{Boundary, Scan, read_line};
use crate::text::GoStr;
use crate::text::quote;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;

use crate::gateway::body_limit::Read;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};

/// A file field of the form.
pub(super) struct FilePart {
    /// `filepath.Base` of the part's `filename` parameter; never empty.
    pub filename: String,
    pub data: Vec<u8>,
}

pub(super) type Files = BTreeMap<String, Vec<FilePart>>;

const NOT_MULTIPART: &str = "request Content-Type isn't multipart/form-data";
const MISSING_BOUNDARY: &str = "no multipart boundary param in Content-Type";
const TOO_LARGE: &str = "multipart: message too large";
const MAX_PARTS: usize = 1000;
/// Go `peekBufferSize`, the multipart reader's `bufio` size.
const PEEK_BUFFER: usize = 4096;

/// gin `Context.ContentType`: the header up to the first space or `;`, unchanged.
pub(super) fn is_form(content_type: &str) -> bool {
    content_type.split([' ', ';']).next() == Some("multipart/form-data")
}

/// File parts grouped by sorted field name (gin's handler sorts `form.File` keys),
/// in body order within a field. Errors carry Go's text.
pub(super) fn form_files(content_type: &str, body: &[u8]) -> Result<Vec<FilePart>, String> {
    Ok(form(content_type, body)?.0.into_values().flatten().collect())
}

/// `Request.ParseMultipartForm`: file parts and value parts by field name, each in body
/// order.
pub(super) fn form(content_type: &str, body: &[u8]) -> Result<(Files, HashMap<String, Vec<String>>), String> {
    let mut hit_limit = false;
    read_form(content_type, body, &mut hit_limit, false)
}

/// A multipart body that could not be used: the read failed before the limit, or Go's
/// `ParseMultipartForm` error with its status (`413` for the limit error).
pub(crate) enum Failed {
    Read(BytesRejection),
    Form(u16, String),
}

/// The body for `ParseMultipartForm`. When `server.max-request-body-bytes` stopped the
/// read, Go has parsed the bytes before the limit: the form is those bytes when it ends
/// within them, else the limit error inside file data and, inside a part's headers,
/// usually the parse error of the truncated line.
pub(crate) fn body(read: Read, content_type: &str) -> Result<Bytes, Failed> {
    match read.body {
        Err(rejection) if crate::gateway::body_limit::is_too_large(&rejection) => {
            let mut hit_limit = false;
            match read_form(content_type, &read.prefix, &mut hit_limit, true) {
                Ok(_) => Ok(read.prefix),
                Err(e) => Err(Failed::Form(if hit_limit { 413 } else { 400 }, e)),
            }
        }
        body => body.map_err(Failed::Read),
    }
}

/// `limited`: the body ends in `http.MaxBytesError` rather than EOF; `hit_limit` is set
/// when the reader runs into it.
fn read_form(
    content_type: &str,
    body: &[u8],
    hit_limit: &mut bool,
    limited: bool,
) -> Result<(Files, HashMap<String, Vec<String>>), String> {
    let (media, params) = parse_media_type(content_type).map_err(|_| NOT_MULTIPART.to_owned())?;
    if media != "multipart/form-data" {
        return Err(NOT_MULTIPART.into());
    }
    let boundary = params.get("boundary").ok_or(MISSING_BOUNDARY)?;
    let mut reader = Reader {
        buf: body,
        pos: 0,
        boundary: Boundary::new(boundary),
        limited,
        hit_limit,
        window: 0,
        over: false,
    };
    let mut files: Files = BTreeMap::new();
    let mut values: HashMap<String, Vec<String>> = HashMap::new();
    let mut parts = 0;
    while let Some(headers) = reader.next_part()? {
        parts += 1;
        if parts > MAX_PARTS {
            return Err(TOO_LARGE.into());
        }
        let (disposition, params) = headers
            .get("content-disposition")
            .and_then(|v| parse_media_type(v).ok())
            .unwrap_or_default();
        let name = if disposition == "form-data" {
            params.get("name").cloned().unwrap_or_default()
        } else {
            String::new()
        };
        if name.is_empty() {
            // Go skips the part; a truncated body surfaces at the next boundary scan.
            if reader.body().is_err() {
                reader.pos = reader.buf.len();
            }
            continue;
        }
        let data = reader.body()?;
        let filename = params.get("filename").map(String::as_str).unwrap_or_default();
        if !filename.is_empty() {
            files.entry(name).or_default().push(FilePart {
                filename: go_base(filename).to_owned(),
                data,
            });
        } else {
            values
                .entry(name)
                .or_default()
                .push(String::from_utf8_lossy(&data).into_owned());
        }
    }
    Ok((files, values))
}

/// Go `filepath.Base` on Unix.
pub(super) fn go_base(path: &str) -> &str {
    if path.is_empty() {
        return ".";
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/";
    }
    trimmed.rsplit('/').next().unwrap_or(trimmed)
}

struct Reader<'a, 'h> {
    buf: &'a [u8],
    pos: usize,
    boundary: Boundary,
    limited: bool,
    hit_limit: &'h mut bool,
    /// Limited parse: the end of what Go's 4096-byte `bufio.Reader` has pulled from
    /// the `MaxBytesReader`, and whether that reader has returned the limit error.
    /// Modelled with an underlying reader that fills every read it is given.
    window: usize,
    over: bool,
}

/// `bufio.Reader.ReadSlice('\n')` in the limited parse.
enum Slice<'a> {
    Line(&'a [u8]),
    /// `bufio.ErrBufferFull`: 4096 bytes without a newline.
    Full(&'a [u8]),
    /// The read error, with what was buffered before it.
    Over(&'a [u8]),
}

impl<'a> Reader<'a, '_> {
    /// Go `Reader.nextPart`; `None` is a clean end of the form.
    fn next_part(&mut self) -> Result<Option<HashMap<String, String>>, String> {
        if self.boundary.is_empty() {
            return Err("multipart: boundary is empty".into());
        }
        let mut expect_new_part = false;
        loop {
            let line = if self.limited {
                match self.slice_go() {
                    Slice::Line(line) => line,
                    Slice::Full(_) => return Err("multipart: NextPart: bufio: buffer full".into()),
                    Slice::Over(_) => return Err(format!("multipart: NextPart: {}", self.end("EOF"))),
                }
            } else {
                let line = read_line(self.buf, &mut self.pos);
                let eof = !line.ends_with(b"\n");
                if eof && self.boundary.is_final(line) {
                    return Ok(None);
                }
                if eof {
                    return Err("multipart: NextPart: EOF".into());
                }
                line
            };
            if self.boundary.start_part(line) {
                return self.headers();
            }
            if self.boundary.is_final(line) {
                return Ok(None);
            }
            if expect_new_part {
                return Err(format!("multipart: expecting a new Part; got line {}", quote(line)));
            }
            if !self.boundary.started() {
                continue;
            }
            if line == self.boundary.newline() {
                expect_new_part = true;
                continue;
            }
            return Err(format!("multipart: unexpected line in Next(): {}", quote(line)));
        }
    }

    /// bufio `fill` over the `MaxBytesReader`: a read of the free buffer space gets
    /// all of it, or the bytes left before the limit and the limit error.
    fn fill(&mut self) {
        let want = PEEK_BUFFER - (self.window - self.pos);
        let left = self.buf.len() - self.window;
        if want <= left {
            self.window += want;
        } else {
            self.window = self.buf.len();
            self.over = true;
        }
    }

    /// `bufio.Reader.ReadSlice('\n')` (the order of its checks matters: a newline,
    /// then the read error, then a full buffer).
    fn slice_go(&mut self) -> Slice<'a> {
        loop {
            let buffered = &self.buf[self.pos..self.window];
            if let Some(i) = buffered.iter().position(|&b| b == b'\n') {
                self.pos += i + 1;
                return Slice::Line(&buffered[..=i]);
            }
            if self.over {
                self.pos = self.window;
                return Slice::Over(buffered);
            }
            if buffered.len() >= PEEK_BUFFER {
                self.pos += PEEK_BUFFER;
                return Slice::Full(buffered);
            }
            self.fill();
        }
    }

    /// `bufio.Reader.ReadLine`: a line without its newline and whether it continues
    /// (a full buffer; a "\r" at its end is put back), or `None` for the read error.
    /// Bytes before the error come back as a line, the error with the next call.
    fn read_line_go(&mut self) -> Option<(&'a [u8], bool)> {
        match self.slice_go() {
            Slice::Full(line) => match line.strip_suffix(b"\r") {
                Some(line) => {
                    self.pos -= 1;
                    Some((line, true))
                }
                None => Some((line, true)),
            },
            Slice::Over([]) => None,
            Slice::Over(line) => Some((line, false)),
            Slice::Line(line) => {
                let line = &line[..line.len() - 1];
                Some((line.strip_suffix(b"\r").unwrap_or(line), false))
            }
        }
    }

    /// `textproto.Reader.readLineSlice`: the fragments of one line joined; the read
    /// error drops them. `Err` is `message too large` past `lim`.
    fn read_line_slice_go(&mut self, lim: usize) -> Result<Option<Cow<'a, [u8]>>, String> {
        let mut line: Option<Vec<u8>> = None;
        loop {
            let Some((part, more)) = self.read_line_go() else {
                return Ok(None);
            };
            if line.as_ref().map_or(0, Vec::len) + part.len() > lim {
                return Err(TOO_LARGE.into());
            }
            match (&mut line, more) {
                (None, false) => return Ok(Some(Cow::Borrowed(part))),
                (None, true) => line = Some(part.to_vec()),
                (Some(line), _) => line.extend_from_slice(part),
            }
            if !more {
                return Ok(line.map(Cow::Owned));
            }
        }
    }

    /// `textproto.Reader.skipSpace` (`ReadByte` fills an empty buffer).
    fn skip_space_go(&mut self) -> usize {
        let mut n = 0;
        loop {
            if self.pos == self.window {
                if self.over {
                    return n;
                }
                self.fill();
                continue;
            }
            if !matches!(self.buf[self.pos], b' ' | b'\t') {
                return n;
            }
            self.pos += 1;
            n += 1;
        }
    }

    /// One header line: the limited parse reads it as Go does, a whole body as one
    /// line up to its newline. `None` is the end of the body.
    fn header_line(&mut self, lim: usize) -> Result<Option<Cow<'a, [u8]>>, String> {
        if self.limited {
            return self.read_line_slice_go(lim);
        }
        Ok(self.read_line().map(Cow::Borrowed))
    }

    /// The read error at the end of the body: `eof`, or the limit error.
    fn end(&mut self, eof: &str) -> String {
        if !self.limited {
            return eof.to_owned();
        }
        *self.hit_limit = true;
        crate::gateway::body_limit::MESSAGE.to_owned()
    }

    /// `textproto.Reader.readLineSlice`: one line without its "\n" or "\r\n";
    /// `None` at the end of the body.
    fn read_line(&mut self) -> Option<&'a [u8]> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let line = read_line(self.buf, &mut self.pos);
        Some(match line.strip_suffix(b"\n") {
            Some(line) => line.strip_suffix(b"\r").unwrap_or(line),
            None => line,
        })
    }

    /// Go `readMIMEHeader`, keyed by lowercased name; the first value wins for
    /// `Header.Get`. EOF inside the headers ends the form cleanly, as in Go; the limit
    /// error fails it.
    fn headers(&mut self) -> Result<Option<HashMap<String, String>>, String> {
        let mut headers = HashMap::new();
        // `Peek(1)`.
        if self.limited && self.pos == self.window && !self.over {
            self.fill();
        }
        let limited = self.limited;
        let visible = if limited { self.window } else { self.buf.len() };
        if matches!(self.buf[..visible].get(self.pos), Some(b' ' | b'\t')) {
            let Some(line) = self.header_line(80)? else {
                return self.ended();
            };
            if line.len() > 80 {
                return Err(TOO_LARGE.into());
            }
            return Err(format!("malformed MIME header initial line: {}", quote(&line)));
        }
        loop {
            let Some(first) = self.header_line(usize::MAX)? else {
                return self.ended();
            };
            let first = &*first;
            if first.is_empty() {
                return Ok(Some(headers));
            }
            if !first.contains(&b':') {
                return Err(format!("malformed MIME header: missing colon: {}", quote(first)));
            }
            // textproto trims spaces and tabs; a whole body keeps the wider trim.
            // ponytail: the limited parse only, where a cut line can end in "\r".
            let trim = if self.limited { go_trim } else { trim_ascii_space };
            let mut kv = trim(first).to_vec();
            loop {
                let spaces = if limited {
                    self.skip_space_go()
                } else {
                    let spaces = self.buf[self.pos..]
                        .iter()
                        .take_while(|&&b| b == b' ' || b == b'\t')
                        .count();
                    self.pos += spaces;
                    spaces
                };
                if spaces == 0 {
                    break;
                }
                kv.push(b' ');
                // Go stops at a continuation line it cannot read; the next line's read
                // reports the error.
                match self.header_line(usize::MAX)? {
                    Some(line) => kv.extend_from_slice(trim(&line)),
                    None => break,
                }
            }
            let malformed = || format!("malformed MIME header line: {}", quote(&kv));
            let colon = kv.iter().position(|&b| b == b':').unwrap_or(kv.len());
            let (key, value) = (&kv[..colon], &kv[(colon + 1).min(kv.len())..]);
            if !key.iter().all(|&b| is_header_field_byte(b) || b == b' ') {
                return Err(malformed());
            }
            if value.iter().any(|&b| (b < 0x20 && b != b'\t') || b == 0x7f) {
                return Err(malformed());
            }
            // Keys with a space are kept but never canonicalized, so `Get` misses them.
            if !key.contains(&b' ') {
                let value = value.iter().skip_while(|&&b| b == b' ' || b == b'\t');
                headers
                    .entry(String::from_utf8_lossy(key).to_ascii_lowercase())
                    .or_insert_with(|| String::from_utf8_lossy(&value.copied().collect::<Vec<_>>()).into_owned());
            }
        }
    }

    /// The headers ended with the body: a clean end of the form at EOF.
    fn ended(&mut self) -> Result<Option<HashMap<String, String>>, String> {
        match self.limited {
            false => Ok(None),
            true => Err(self.end("EOF")),
        }
    }

    /// The current part's body, up to its closing boundary (Go `partReader`).
    fn body(&mut self) -> Result<Vec<u8>, String> {
        if self.limited {
            return self.body_go();
        }
        self.boundary
            .read_part(self.buf, &mut self.pos)
            .map(<[u8]>::to_vec)
            .map_err(|_| self.end("unexpected EOF"))
    }
}

impl Reader<'_, '_> {
    /// `partReader.Read` until the part ends, over the bufio window: it scans what is
    /// buffered and fills only when the scan needs more (`Peek(len+1)`).
    fn body_go(&mut self) -> Result<Vec<u8>, String> {
        let mut data = Vec::new();
        let (mut total, mut read_err) = (0, false);
        loop {
            let buffered = &self.buf[self.pos..self.window];
            match self.boundary.scan_window(buffered, total == 0, read_err) {
                Scan::Boundary(n) => {
                    data.extend_from_slice(&buffered[..n]);
                    self.pos += n;
                    return Ok(data);
                }
                Scan::UnexpectedEof(n) => {
                    self.pos += n;
                    return Err(self.end("unexpected EOF"));
                }
                Scan::Data(0) => {
                    let want = buffered.len() + 1;
                    while self.window - self.pos < want.min(PEEK_BUFFER) && !self.over {
                        self.fill();
                    }
                    read_err = self.window - self.pos < want;
                }
                Scan::Data(n) => {
                    data.extend_from_slice(&buffered[..n]);
                    self.pos += n;
                    total += n;
                }
            }
        }
    }
}

/// textproto `trim`: spaces and tabs at both ends.
fn go_trim(b: &[u8]) -> &[u8] {
    let is_space = |c: &u8| matches!(c, b' ' | b'\t');
    let start = b.iter().position(|c| !is_space(c)).unwrap_or(b.len());
    let end = b.iter().rposition(|c| !is_space(c)).map_or(start, |i| i + 1);
    &b[start..end]
}

fn trim_ascii_space(b: &[u8]) -> &[u8] {
    let is_space = |c: &u8| matches!(c, b' ' | b'\t' | b'\n' | b'\r');
    let start = b.iter().position(|c| !is_space(c)).unwrap_or(b.len());
    let end = b.iter().rposition(|c| !is_space(c)).map_or(start, |i| i + 1);
    &b[start..end]
}

fn is_header_field_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

fn is_tspecial(c: u8) -> bool {
    b"()<>@,;:\\\"/[]?=".contains(&c)
}

fn is_token_char(c: u8) -> bool {
    c > 0x20 && c < 0x7f && !is_tspecial(c)
}

fn consume_token(v: &str) -> (&str, &str) {
    let i = v.bytes().position(|c| !is_token_char(c)).unwrap_or(v.len());
    v.split_at(i)
}

fn consume_value(v: &str) -> Option<(String, &str)> {
    if !v.starts_with('"') {
        let (token, rest) = consume_token(v);
        return Some((token.to_owned(), rest));
    }
    let bytes = v.as_bytes();
    let mut out = Vec::new();
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return Some((String::from_utf8_lossy(&out).into_owned(), &v[i + 1..])),
            // Unnecessary escapes stay literal backslashes (MSIE paths).
            b'\\' if i + 1 < bytes.len() && is_tspecial(bytes[i + 1]) => {
                out.push(bytes[i + 1]);
                i += 1;
            }
            b'\r' | b'\n' => return None,
            b => out.push(b),
        }
        i += 1;
    }
    None
}

/// Go `consumeMediaParam`: `None` when `v` does not start with a valid parameter.
fn consume_media_param(v: &str) -> Option<(String, String, &str)> {
    let rest = v.trim_start().strip_prefix(';')?.trim_start();
    let (param, rest) = consume_token(rest);
    if param.is_empty() {
        return None;
    }
    let rest = rest.trim_start().strip_prefix('=')?.trim_start();
    let (value, after) = consume_value(rest)?;
    if value.is_empty() && after.len() == rest.len() {
        return None;
    }
    Some((param.go_lower(), value, after))
}

/// Go `mime.ParseMediaType`, including RFC 2231 continuations and charsets.
pub(super) fn parse_media_type(v: &str) -> Result<(String, HashMap<String, String>), ()> {
    let base = v.split(';').next().unwrap_or_default();
    let media = base.go_lower().trim().to_owned();
    let (kind, rest) = consume_token(&media);
    if kind.is_empty() {
        return Err(());
    }
    if !rest.is_empty() {
        let (sub, rest) = consume_token(rest.strip_prefix('/').ok_or(())?);
        if sub.is_empty() || !rest.is_empty() {
            return Err(());
        }
    }
    let mut params = HashMap::new();
    let mut continuation: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut v = &v[base.len()..];
    loop {
        v = v.trim_start();
        if v.is_empty() {
            break;
        }
        let Some((key, value, rest)) = consume_media_param(v) else {
            if v.trim() == ";" {
                break;
            }
            return Err(());
        };
        let map = match key.split_once('*') {
            Some((base_name, _)) => continuation.entry(base_name.to_owned()).or_default(),
            None => &mut params,
        };
        if map.get(&key).is_some_and(|existing| *existing != value) {
            return Err(());
        }
        map.insert(key, value);
        v = rest;
    }
    for (key, pieces) in continuation {
        if let Some(v) = pieces.get(&format!("{key}*")) {
            if let Some(decoded) = decode_2231(v) {
                params.insert(key, decoded);
            }
            continue;
        }
        let mut buf = String::new();
        let mut valid = false;
        for n in 0.. {
            if let Some(v) = pieces.get(&format!("{key}*{n}")) {
                valid = true;
                buf.push_str(v);
                continue;
            }
            let Some(v) = pieces.get(&format!("{key}*{n}*")) else {
                break;
            };
            valid = true;
            if n == 0 {
                buf.push_str(&decode_2231(v).unwrap_or_default());
            } else {
                buf.push_str(&percent_unescape(v).unwrap_or_default());
            }
        }
        if valid {
            params.insert(key, buf);
        }
    }
    Ok((media, params))
}

fn decode_2231(v: &str) -> Option<String> {
    let (charset, rest) = v.split_once('\'')?;
    let (_, value) = rest.split_once('\'')?;
    match charset.go_lower().as_str() {
        "us-ascii" | "utf-8" => percent_unescape(value),
        _ => None,
    }
}

/// Go `percentHexUnescape`.
fn percent_unescape(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(
                u8::from_str_radix(hex, 16)
                    .ok()
                    .filter(|_| hex.bytes().all(|c| c.is_ascii_hexdigit()))?,
            );
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CT: &str = "multipart/form-data; boundary=b";

    #[test]
    fn reads_file_fields_sorted_by_field_and_skips_values() {
        let body = b"--b\r\nContent-Disposition: form-data; name=\"z\"; filename=\"dir/a.json\"\r\n\r\n{}\r\n--b\r\nContent-Disposition: form-data; name=\"f\"\r\n\r\nv\r\n--b\r\nContent-Disposition: form-data; name=\"c\"; filename=\"b.json\"\r\n\r\n[1]\r\n--b--\r\n";
        let files = form_files(CT, body).unwrap();
        let got: Vec<_> = files.iter().map(|f| (f.filename.as_str(), f.data.as_slice())).collect();
        // Field "c" sorts before field "z".
        assert_eq!(got, [("b.json", &b"[1]"[..]), ("a.json", &b"{}"[..])]);
    }

    #[test]
    // Product policy: RFC 7578 form-data and RFC 2231/5987 extended parameters carry filenames safely; contradictory parameters and malformed media types are refused.
    fn multipart_parameters_decode_quoted_and_extended_filenames() {
        let (t, p) = parse_media_type("Form-Data; Name=\"a\\\"b\"; filename*=UTF-8''x%2Ey; k*0=p; k*1*=%41").unwrap();
        assert_eq!(t, "form-data");
        assert_eq!(p["name"], "a\"b");
        assert_eq!(p["filename"], "x.y");
        assert_eq!(p["k"], "pA");
        assert!(parse_media_type("a/b; x=1; x=2").is_err());
        assert!(parse_media_type("a/b; x=1; x=1").is_ok());
        assert!(parse_media_type("a/b/c").is_err());
        assert!(parse_media_type("a/b; x=\"open").is_err());
    }

    /// Deterministic fuzz over boundary-shaped fragments: never panics, and any
    /// file returned has a non-empty base name.
    #[test]
    fn hostile_bodies_never_panic() {
        const PIECES: &[&[u8]] = &[
            b"--b",
            b"--",
            b"\r\n",
            b"\n",
            b"\r",
            b" ",
            b"\t",
            b"-",
            b"b",
            b"{}",
            b"Content-Disposition: form-data; name=\"f\"; filename=\"x.json\"",
            b"Content-Disposition: form-data; name=f",
            b"X:",
            b":",
            b"\x7f",
            b"\xff",
        ];
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..20_000 {
            let mut body = Vec::new();
            for _ in 0..(next() % 24) {
                body.extend_from_slice(PIECES[(next() % PIECES.len() as u64) as usize]);
            }
            if let Ok(files) = form_files(CT, &body) {
                assert!(
                    files
                        .iter()
                        .all(|f| !f.filename.is_empty() && !f.filename.contains('/'))
                );
            }
        }
    }
}
