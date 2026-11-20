//! Adapts line-based stream translators to the [`StreamTranslator`] contract.
//!
//! The translators were ported from CLIProxyAPI, where executors read upstream bodies
//! line by line and call the translator per line. Here every framed upstream event is
//! split back into those lines, each goes through the line-based [`GoStream`], and each
//! chunk is framed as a complete event of the client's streaming protocol, so the bytes
//! returned are the bytes the client receives.

use crate::format::Format;
use crate::translation::{Error, StreamTranslator};
use bytes::Bytes;

/// One Go `ResponseStreamTransform` with its request-local state.
#[doc(hidden)]
pub trait GoStream: Send {
    /// One upstream line (no terminator) in, zero or more Go chunks out.
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error>;
    /// Go's `ToolInputError() != nil` on the translator state.
    fn tool_input_failed(&self) -> bool {
        false
    }
    /// Go's `FinalizeToolInput()`.
    fn finalize_tool_input(&mut self) -> Vec<Vec<u8>> {
        vec![]
    }
    /// Go's `CanFinalizeResponseStream()`.
    fn can_finalize_response_stream(&self) -> bool {
        false
    }
}

/// bufio.ScanLines over one event: split on `\n`, drop one trailing `\r` per line, and
/// no empty token after a final newline.
pub(crate) fn scan_lines(event: &[u8]) -> impl Iterator<Item = &[u8]> {
    let body = event.strip_suffix(b"\n").unwrap_or(event);
    let empty = event.is_empty();
    body.split(|&c| c == b'\n')
        .filter(move |_| !empty)
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
}

/// One translator chunk as client bytes: a Chat Completions chunk becomes a
/// `data: <chunk>` event, and other clients' chunks are already complete events. An
/// empty chunk writes nothing.
pub fn frame(client: Format, chunk: &[u8]) -> Option<Vec<u8>> {
    if chunk.is_empty() {
        return None;
    }
    match client {
        // Chat Completions streaming: `data: <json>` and a blank line.
        Format::OpenAI => {
            let mut out = Vec::with_capacity(chunk.len() + 8);
            out.extend_from_slice(b"data: ");
            out.extend_from_slice(chunk);
            out.extend_from_slice(b"\n\n");
            Some(out)
        }
        // Claude translators emit complete SSE events; Responses chunks are joined per
        // upstream event by `join_responses_frames`.
        _ => Some(chunk.to_vec()),
    }
}

fn responses_client(client: Format) -> bool {
    matches!(client, Format::OpenAIResponse | Format::Codex)
}

/// Go's responsesSSEFramer.WriteChunk/Flush joining and emission rules
/// (openai_responses_handlers.go): line chunks are joined into frames, a valid data-only
/// frame closes before the next `data:` line, and every frame ends with a blank line.
///
/// For executors that pass upstream Responses lines through untranslated: `write` each
/// chunk, then `flush` at the end of the stream and before writing a terminal error (Go
/// flushes in both places; an incomplete or invalid pending frame is dropped).
// ponytail: repairFrame (private-event filtering, completed-output and error repair,
// terminal tracking) is route logic and stays with the Responses route in gateway.
#[derive(Default)]
pub struct ResponsesFramer {
    pending: Vec<u8>,
    repair: PendingRepair,
}

/// responsesSSEDataLinesValid: no data, `[DONE]`, or one valid JSON payload.
fn data_lines_valid(chunk: &[u8]) -> bool {
    let mut payload: Option<Vec<u8>> = None;
    for line in chunk.split(|&c| c == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(data) = crate::translation::common::trim_space(line).strip_prefix(b"data:") {
            match &mut payload {
                Some(p) => {
                    p.push(b'\n');
                    p.extend_from_slice(crate::translation::common::trim_space(data));
                }
                None => payload = Some(crate::translation::common::trim_space(data).to_vec()),
            }
        }
    }
    data_payload_valid(payload)
}

fn data_payload_valid(payload: Option<Vec<u8>>) -> bool {
    let Some(payload) = payload else {
        return true;
    };
    let payload = crate::translation::common::trim_space(&payload);
    payload.is_empty() || payload == b"[DONE]" || crate::protocol::json::std_valid(payload)
}

/// responsesSSENeedsLineBreak: a field line appended to an unterminated line.
fn needs_line_break(pending: &[u8], chunk: &[u8]) -> bool {
    if pending.is_empty() || chunk.is_empty() || pending.ends_with(b"\n") || pending.ends_with(b"\r") {
        return false;
    }
    if chunk[0] == b'\n' || chunk[0] == b'\r' {
        return false;
    }
    let start = chunk
        .iter()
        .position(|c| !matches!(c, b' ' | b'\t'))
        .unwrap_or(chunk.len());
    let trimmed = &chunk[start..];
    !trimmed.is_empty()
        && [&b"data:"[..], b"event:", b"id:", b"retry:", b":"]
            .iter()
            .any(|p| trimmed.starts_with(p))
}

/// Delimited events need no repair. For the unterminated tail, remember which fields
/// arrived and when the joined data could end a JSON value. Only the existing payload
/// validator decides whether to emit it.
#[derive(Default)]
struct PendingRepair {
    scanned: usize,
    nonblank: bool,
    line: Field,
    has_data: bool,
    has_event: bool,
    payload: PayloadEnd,
}

#[derive(Default)]
enum Field {
    #[default]
    Prefix,
    Data,
    Other,
}

impl PendingRepair {
    fn scan(&mut self, pending: &[u8]) {
        while let Some(&byte) = pending.get(self.scanned) {
            let rest = &pending[self.scanned..];
            let (rune, width) = crate::protocol::json::decode_rune(rest);
            // A split UTF-8 whitespace rune may still disappear under trim_space.
            if rune.is_none() && rest.len() < 4 && std::str::from_utf8(rest).is_err_and(|e| e.error_len().is_none()) {
                break;
            }
            if byte == b'\n' {
                self.line = Field::Prefix;
            } else if !rune.is_some_and(char::is_whitespace) {
                self.nonblank = true;
                match self.line {
                    Field::Prefix if rest.starts_with(b"data:") => {
                        self.has_data = true;
                        self.line = Field::Data;
                        self.scanned += b"data:".len();
                        continue;
                    }
                    Field::Prefix if rest.starts_with(b"event:") => {
                        self.has_event = true;
                        self.line = Field::Other;
                    }
                    Field::Prefix if b"data:".starts_with(rest) || b"event:".starts_with(rest) => break,
                    Field::Prefix => self.line = Field::Other,
                    Field::Data => self.payload.push(byte),
                    Field::Other => {}
                }
            }
            self.scanned += width;
        }
    }

    fn valid_data(&mut self, pending: &[u8]) -> bool {
        if !self.has_data || (matches!(self.line, Field::Data) && self.scanned < pending.len()) {
            return false;
        }
        if !self.payload.can_end() {
            return false;
        }
        if data_lines_valid(pending) {
            return true;
        }
        // A closed string/container, a literal at its last letter, or a number ending
        // in a digit cannot repair an earlier syntax error by appending more data.
        self.payload = PayloadEnd::Invalid;
        false
    }
}

#[derive(Default)]
enum PayloadEnd {
    #[default]
    Empty,
    QuotedOrNested {
        depth: usize,
        quoted: bool,
        escaped: bool,
    },
    Literal(&'static [u8]),
    Number {
        ends_in_digit: bool,
    },
    Closed,
    Invalid,
}

impl PayloadEnd {
    // Whitespace cannot open or close a JSON value. data_lines_valid still checks its
    // exact spelling, including the newlines inserted between data fields.
    fn push(&mut self, byte: u8) {
        match self {
            Self::Empty => {
                *self = match byte {
                    b'{' | b'[' | b'"' => Self::QuotedOrNested {
                        depth: usize::from(byte != b'"'),
                        quoted: byte == b'"',
                        escaped: false,
                    },
                    b't' => Self::Literal(b"rue"),
                    b'f' => Self::Literal(b"alse"),
                    b'n' => Self::Literal(b"ull"),
                    b'-' | b'0'..=b'9' => Self::Number {
                        ends_in_digit: byte != b'-',
                    },
                    _ => Self::Invalid,
                }
            }
            Self::QuotedOrNested { depth, quoted, escaped } => {
                if *escaped {
                    *escaped = false;
                } else {
                    match byte {
                        b'\\' if *quoted => *escaped = true,
                        b'"' => *quoted = !*quoted,
                        b'{' | b'[' if !*quoted => *depth += 1,
                        b'}' | b']' if !*quoted => *depth -= 1,
                        _ => {}
                    }
                }
                if *depth == 0 && !*quoted {
                    *self = Self::Closed;
                }
            }
            Self::Literal(remaining) => {
                *self = match remaining.strip_prefix(&[byte]) {
                    Some([]) => Self::Closed,
                    Some(tail) => Self::Literal(tail),
                    None => Self::Invalid,
                };
            }
            Self::Number { ends_in_digit } => *ends_in_digit = byte.is_ascii_digit(),
            Self::Closed => *self = Self::Invalid,
            Self::Invalid => {}
        }
    }

    fn can_end(&self) -> bool {
        matches!(self, Self::Empty | Self::Closed | Self::Number { ends_in_digit: true })
    }
}

impl ResponsesFramer {
    /// WriteChunk: the frames this chunk completes, each ending in a blank line.
    pub fn write(&mut self, chunk: &[u8]) -> Vec<Bytes> {
        let mut out = vec![];
        self.write_into(chunk, &mut out);
        out.into_iter().map(Bytes::from).collect()
    }

    /// Flush: the pending frame when it carries valid data; otherwise nothing.
    pub fn flush(&mut self) -> Vec<Bytes> {
        let mut out = vec![];
        self.flush_into(&mut out);
        out.into_iter().map(Bytes::from).collect()
    }

    fn write_into(&mut self, chunk: &[u8], out: &mut Vec<Vec<u8>>) {
        if chunk.is_empty() {
            return;
        }
        if self.repair.has_data && !self.repair.has_event {
            let start = chunk.iter().position(|c| !matches!(c, b' ' | b'\t' | b'\r' | b'\n'));
            if start.is_some_and(|i| chunk[i..].starts_with(b"data:")) && self.repair.valid_data(&self.pending) {
                out.push(self.take_terminated());
            }
        }

        // Only a newly appended LF can complete a delimiter, including one split over
        // writes. Look behind within the current frame so two delimiters cannot overlap.
        let appended = self.pending.len();
        if needs_line_break(&self.pending, chunk) {
            self.pending.push(b'\n');
        }
        self.pending.extend_from_slice(chunk);
        let mut start = 0;
        for end in appended..self.pending.len() {
            if self.pending[end] == b'\n' {
                let frame = &self.pending[start..=end];
                if frame.ends_with(b"\n\n") || frame.ends_with(b"\r\n\r\n") {
                    out.push(frame.to_vec());
                    start = end + 1;
                }
            }
        }
        if start > 0 {
            self.pending.drain(..start);
            self.repair = PendingRepair::default();
        }
        self.repair.scan(&self.pending);
        if !self.repair.nonblank && self.repair.scanned == self.pending.len() {
            self.pending.clear();
            self.repair = PendingRepair::default();
        } else if self.repair.has_event && self.repair.valid_data(&self.pending) {
            out.push(self.take_terminated());
        }
    }

    /// Complete the blank line of a valid, undelimited frame.
    fn take_terminated(&mut self) -> Vec<u8> {
        let mut frame = std::mem::take(self).pending;
        frame.extend_from_slice(if frame.ends_with(b"\r\n") {
            b"\r\n"
        } else if frame.ends_with(b"\n") {
            b"\n"
        } else {
            b"\n\n"
        });
        frame
    }

    fn flush_into(&mut self, out: &mut Vec<Vec<u8>>) {
        if self.repair.valid_data(&self.pending) {
            out.push(self.take_terminated());
        } else {
            *self = Self::default();
        }
    }
}

struct Framed {
    client: Format,
    upstream: Format,
    inner: Box<dyn GoStream>,
    responses: ResponsesFramer,
    options: StreamOptions,
}

/// A transform applied to each translated chunk ([`StreamOptions::chunk`]).
pub type ChunkHook = fn(&[u8]) -> Vec<u8>;

/// How a Go executor drives a pair's stream translator beyond reading lines; see
/// [`crate::translation::stream_with`].
#[derive(Clone, Copy, Default)]
pub struct StreamOptions {
    /// Each event is one Go translator call, newlines included, as Go's AI Studio
    /// executor passes every relay chunk and its Antigravity executor every payload.
    pub whole_events: bool,
    /// Applied to every chunk the translator returns for an event, before the client's
    /// framing (AI Studio's `ensureColonSpacedJSON`). The tool-input finalization is
    /// written without it, as Go's `EndApplyPatchStream` does.
    pub chunk: Option<ChunkHook>,
}

/// Go's OpenAI-compatible executor joins a frame's `data:` lines with `\n` and passes the
/// result as one translator line, so for OpenAI upstreams a line that does not start an
/// SSE field continues the previous line.
fn openai_lines(event: &[u8]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = vec![];
    for line in scan_lines(event) {
        let field = [&b"data:"[..], b"event:", b"id:", b"retry:", b":"]
            .iter()
            .any(|p| line.trim_ascii_start().starts_with(p));
        match out.last_mut() {
            Some(previous) if !field && !line.is_empty() && previous.starts_with(b"data:") => {
                previous.push(b'\n');
                previous.extend_from_slice(line);
            }
            _ => out.push(line.to_vec()),
        }
    }
    out
}

impl Framed {
    fn framed(&mut self, chunks: Vec<Vec<u8>>) -> Vec<Bytes> {
        if responses_client(self.client) {
            let mut out = vec![];
            for chunk in &chunks {
                self.responses.write_into(chunk, &mut out);
            }
            return out.into_iter().map(Bytes::from).collect();
        }
        chunks
            .iter()
            .filter_map(|c| frame(self.client, c))
            .map(Bytes::from)
            .collect()
    }
}

impl StreamTranslator for Framed {
    fn flush_frames(&mut self) -> Vec<Bytes> {
        self.responses.flush()
    }

    /// Runs on the caller's thread for every event: the translators' walkers keep nested
    /// JSON on the heap (issue #146), whatever the stream has carried so far.
    fn event(&mut self, event: &[u8]) -> Result<Vec<Bytes>, Error> {
        self.translate(event)
    }

    fn finish(&mut self) -> Result<Vec<Bytes>, Error> {
        let mut out = vec![];
        self.responses.flush_into(&mut out);
        Ok(out.into_iter().map(Bytes::from).collect())
    }

    fn tool_input_failed(&self) -> bool {
        self.inner.tool_input_failed()
    }

    fn can_finalize_response_stream(&self) -> bool {
        self.inner.can_finalize_response_stream()
    }

    fn finalize_tool_input(&mut self) -> Vec<Bytes> {
        let chunks = self.inner.finalize_tool_input();
        self.framed(chunks)
    }
}

impl Framed {
    fn translate(&mut self, event: &[u8]) -> Result<Vec<Bytes>, Error> {
        let mut chunks = vec![];
        if self.upstream == Format::OpenAI {
            for line in openai_lines(event) {
                chunks.extend(self.inner.line(&line)?);
            }
        } else if self.options.whole_events {
            chunks.extend(self.inner.line(event)?);
        } else {
            for line in scan_lines(event) {
                chunks.extend(self.inner.line(line)?);
            }
        }
        if let Some(hook) = self.options.chunk {
            chunks = chunks.iter().map(|c| hook(c)).collect();
        }
        Ok(self.framed(chunks))
    }
}

/// Wraps a Go-shaped translator. Executor-side line handling stays with the executor:
/// Go's OpenAI-compatible executor joins a frame's `data:` lines into one line and feeds
/// `data: [DONE]` when a non-Responses stream ends without one. Executors that do so pass
/// those lines as events.
pub(crate) fn framed(client: Format, upstream: Format, inner: Box<dyn GoStream>) -> Box<dyn StreamTranslator> {
    framed_with(client, upstream, inner, StreamOptions::default())
}

/// [`framed`] with a Go executor's [`StreamOptions`].
pub(crate) fn framed_with(
    client: Format,
    upstream: Format,
    inner: Box<dyn GoStream>,
    options: StreamOptions,
) -> Box<dyn StreamTranslator> {
    Box::new(Framed {
        client,
        upstream,
        inner,
        responses: ResponsesFramer::default(),
        options,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_lines_matches_bufio() {
        let lines: Vec<&[u8]> = scan_lines(b"event: a\r\ndata: 1\n\n").collect();
        assert_eq!(lines, [&b"event: a"[..], b"data: 1", b""]);
        let lines: Vec<&[u8]> = scan_lines(b"data: tail").collect();
        assert_eq!(lines, [&b"data: tail"[..]]);
        assert_eq!(scan_lines(b"").count(), 0);
        assert_eq!(scan_lines(b"\n").collect::<Vec<_>>(), [&b""[..]]);
    }

    struct Echo(Vec<Vec<u8>>);
    impl GoStream for Echo {
        fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
            self.0.push(line.to_vec());
            Ok(vec![line.to_vec()])
        }
    }

    fn responses(chunks: &[&[u8]]) -> Vec<Vec<u8>> {
        let mut framer = ResponsesFramer::default();
        let mut out = vec![];
        for c in chunks {
            out.extend(framer.write(c).into_iter().map(|b| b.to_vec()));
        }
        out.extend(framer.flush().into_iter().map(|b| b.to_vec()));
        out
    }

    /// Responses clients receive named SSE events with exactly one terminating blank line.
    /// Values are specified here from the event schema, independently of translator output.
    #[test]
    fn responses_framer_keeps_event_order_payload_and_terminal_boundaries() {
        let clamp: [&[u8]; 9] = [
            b"event: response.created\n",
            b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_2\"}}\n",
            b"\n",
            b"event: response.output_text.delta\n",
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n",
            b"\n",
            b"event: response.completed\n",
            b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_2\"}}\n",
            b"\n",
        ];
        assert_eq!(
            responses(&clamp),
            vec![
                b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_2\"}}\n\n".to_vec(),
                b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n".to_vec(),
                b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_2\"}}\n\n".to_vec(),
            ]
        );
        let complete = clamp.concat();
        assert_eq!(responses(&[&complete]), responses(&clamp));
        let parsed: Vec<serde_json::Value> = responses(&clamp)
            .iter()
            .map(|frame| {
                let data = frame
                    .split(|&b| b == b'\n')
                    .find_map(|line| line.strip_prefix(b"data: "))
                    .unwrap();
                serde_json::from_slice(data).unwrap()
            })
            .collect();
        assert_eq!(
            parsed,
            vec![
                serde_json::json!({"type":"response.created","response":{"id":"resp_2"}}),
                serde_json::json!({"type":"response.output_text.delta","delta":"hi"}),
                serde_json::json!({"type":"response.completed","response":{"id":"resp_2"}}),
            ]
        );
        // Data-only events stay separate frames; the invalid tail is dropped on flush.
        let data_only: [&[u8]; 6] = [
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"a\"}\n",
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"b\"}\n",
            b"\n",
            b"data: [DONE]\n",
            b": keep\n",
            b"data: {\"type\":\n",
        ];
        assert_eq!(
            responses(&data_only),
            vec![
                b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"a\"}\n\n".to_vec(),
                b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"b\"}\n\n".to_vec(),
                b"data: [DONE]\n: keep\n\n".to_vec(),
            ]
        );
        // Flushing before a terminal error emits a valid pending frame exactly once.
        let mut framer = ResponsesFramer::default();
        assert!(framer.write(b"event: response.created\n").is_empty());
        assert_eq!(framer.write(b"data: {\"a\":1}").len(), 1);
        assert!(framer.write(b"data: {\"b\":2}").is_empty());
        assert_eq!(framer.flush(), vec![Bytes::from_static(b"data: {\"b\":2}\n\n")]);
        assert!(framer.flush().is_empty());
    }

    #[test]
    fn responses_framer_buffers_partial_events_and_flushes_once() {
        assert_eq!(
            responses(&[b"event: a", b"data: {}", b""]),
            [b"event: a\ndata: {}\n\n".to_vec()]
        );
        assert_eq!(
            responses(&[b"event: a\ndata: 1\n\nevent: b\ndata: 2\n\n"]),
            [b"event: a\ndata: 1\n\n".to_vec(), b"event: b\ndata: 2\n\n".to_vec()]
        );
        // A valid data-only frame closes before the next data line.
        assert_eq!(
            responses(&[b"data: {}", b"data: []"]),
            [b"data: {}\n\n".to_vec(), b"data: []\n\n".to_vec()]
        );
        // An event line waits for its data, even across upstream events.
        assert_eq!(
            responses(&[b"event: x", b"", b"data: {}"]),
            [b"event: x\ndata: {}\n\n".to_vec()]
        );
        // An event without data is dropped at flush; blank input emits nothing.
        assert!(responses(&[b"event: lonely"]).is_empty());
        assert!(responses(&[b"", b"  "]).is_empty());
        assert_eq!(responses(&[b"data: x\r\n"]), Vec::<Vec<u8>>::new());
        assert_eq!(responses(&[b"data: [DONE]"]), [b"data: [DONE]\n\n".to_vec()]);
    }

    #[test]
    fn responses_framer_preserves_batched_and_split_delimiters() {
        // Split each delimiter at every offset. A delimited frame is opaque, even
        // when its data is invalid. Only the unterminated tail needs validation.
        for delimiter in [&b"\n\n"[..], b"\r\n\r\n"] {
            let first = [b"data: \xff", delimiter].concat();
            for split in 0..delimiter.len() {
                let mut framer = ResponsesFramer::default();
                let cut = first.len() - delimiter.len() + split;
                assert!(framer.write(&first[..cut]).is_empty());
                let rest = [&first[cut..], b": a\r\n\n: b\r\n\r\n\n\ndata: [DONE]\r\n"].concat();
                assert_eq!(
                    framer.write(&rest),
                    [first.as_slice(), b": a\r\n\n", b": b\r\n\r\n", b"\n\n"]
                );
                assert_eq!(framer.flush(), [b"data: [DONE]\r\n\r\n".as_slice()]);
                assert!(framer.flush().is_empty());
            }
        }
    }

    #[test]
    fn responses_framer_repairs_on_the_write_that_completes_the_payload() {
        let cases: &[(&[u8], &[u8])] = &[
            (b"nul", b"l"),
            (b"fal", b"se"),
            (b"tr", b"ue"),
            (b"-1.25e+", b"3"),
            (br#""escaped \" [}"#, b"\""),
            (br#"[[{"x":"}"}]"#, b"]"),
            (b"[DON", b"E]"),
        ];
        for &(prefix, end) in cases {
            let mut framer = ResponsesFramer::default();
            assert!(framer.write(b"event: delta").is_empty());
            // The whole data field prefix arrives with an unfinished value. Subsequent
            // fragments must not close the event until its value is valid.
            let data = [b"data: ", prefix].concat();
            assert!(framer.write(&data).is_empty());
            for byte in &end[..end.len() - 1] {
                assert!(framer.write(&[*byte]).is_empty());
            }
            let expected = [b"event: delta\n", data.as_slice(), end, b"\n\n"].concat();
            assert_eq!(framer.write(&end[end.len() - 1..]), [expected]);
            assert!(framer.flush().is_empty());
        }
    }

    #[test]
    fn responses_framer_tracks_split_fields_and_unicode_space() {
        let mut framer = ResponsesFramer::default();
        for chunk in [
            &b"\xe3"[..],
            b"\x80",
            b"\x80",
            b"ev",
            b"ent: x\n\xc2",
            b"\xa0da",
            b"ta: [1,",
        ] {
            assert!(framer.write(chunk).is_empty());
        }
        assert_eq!(
            framer.write(b"data: 2]"),
            [b"event: x\n\xc2\xa0data: [1,\ndata: 2]\n\n".as_slice()]
        );
        // An incomplete rune after a valid value can become trailing whitespace.
        assert!(framer.write(b"event: x\ndata: {}\xe3").is_empty());
        assert!(framer.write(b"\x80").is_empty());
        assert_eq!(
            framer.write(b"\x80"),
            [b"event: x\ndata: {}\xe3\x80\x80\n\n".as_slice()]
        );
        assert!(framer.write(b"data: {}\xff").is_empty());
        assert!(framer.flush().is_empty());
    }

    #[test]
    fn responses_framer_waits_for_a_delimiter_after_invalid_joined_data() {
        let mut framer = ResponsesFramer::default();
        assert!(framer.write(b"event: x\ndata: [bogus]").is_empty());
        assert!(framer.write(b"data: {}").is_empty());
        assert_eq!(
            framer.write(b"\n\n"),
            [b"event: x\ndata: [bogus]\ndata: {}\n\n".as_slice()]
        );
        assert!(framer.write(b"data: 1e").is_empty());
        assert!(framer.flush().is_empty());
        assert!(framer.write(b"data: {}").is_empty());
        assert_eq!(framer.write(b" \r\n data: []"), [b"data: {}\n\n".as_slice()]);
        assert_eq!(framer.flush(), [b" \r\n data: []\n\n".as_slice()]);
    }

    #[test]
    fn openai_joined_data_payload_stays_one_line() {
        // Go scenario stream_multiline_data: the executor's joined payload keeps its newline.
        let mut s = framed(Format::OpenAI, Format::OpenAI, Box::new(Echo(vec![])));
        let out = s.event(b"data: {\"id\":\"m\",\n\"choices\":[]}\n\n").unwrap();
        assert_eq!(
            out,
            [Bytes::from_static(b"data: data: {\"id\":\"m\",\n\"choices\":[]}\n\n")]
        );
        // Field lines still split; raw single lines (Kimi) pass as they are.
        let out = s.event(b"event: x\ndata: 1\n\n").unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(
            s.event(b": keep-alive").unwrap(),
            [Bytes::from_static(b"data: : keep-alive\n\n")]
        );
    }

    #[test]
    fn events_become_scanner_lines_and_chunks_get_client_framing() {
        let mut s = framed(Format::OpenAI, Format::OpenAI, Box::new(Echo(vec![])));
        let out = s.event(b"event: x\r\ndata: {}\n\n").unwrap();
        assert_eq!(
            out,
            [
                Bytes::from_static(b"data: event: x\n\n"),
                Bytes::from_static(b"data: data: {}\n\n")
            ]
        );
        // A single line (how some executors feed) is one scanner line.
        assert_eq!(
            s.event(b": keep-alive").unwrap(),
            [Bytes::from_static(b"data: : keep-alive\n\n")]
        );
        assert!(s.finish().unwrap().is_empty());
        let mut s = framed(Format::Claude, Format::OpenAI, Box::new(Echo(vec![])));
        assert_eq!(
            s.event(b"event: e\ndata: 1\n\n").unwrap(),
            [Bytes::from_static(b"event: e"), Bytes::from_static(b"data: 1")]
        );
    }
}
