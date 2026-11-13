//! Response plumbing shared by the inference routes: JSON bodies, SSE headers and the
//! stream forwarder (sdk/api/handlers/stream_forwarder.go).

use std::collections::VecDeque;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use cpa_core::config::Config;
use cpa_core::exec::{ExecError, ExecStream};
use futures_util::StreamExt;

/// A JSON response with an explicit Content-Type.
pub fn json(status: u16, content_type: &'static str, body: impl Into<Body>) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (
        status,
        [(header::CONTENT_TYPE, HeaderValue::from_static(content_type))],
        body.into(),
    )
        .into_response()
}

/// gin `c.JSON`: compact JSON with the charset parameter.
pub fn gin_json(status: u16, body: String) -> Response {
    json(status, "application/json; charset=utf-8", body)
}

/// gin `c.JSON(status, ErrorResponse{Error: ErrorDetail{Message, Type}})`.
pub fn error_detail(status: u16, message: &str, kind: &str) -> Response {
    let detail = crate::gojson::Obj::new()
        .str("message", message)
        .str("type", kind)
        .finish();
    gin_json(status, crate::gojson::Obj::new().raw("error", &detail).finish())
}

/// The headers every Go SSE handler sets before the first event.
pub fn sse(body: Body) -> Response {
    let mut res = body.into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    res
}

/// `requests.streaming.keepalive-seconds` (0 or absent disables heartbeats).
pub fn keepalive(cfg: &Config) -> Option<Duration> {
    let seconds = cfg
        .document
        .get("requests")
        .and_then(|r| r.get("streaming"))
        .and_then(|s| s.get("keepalive-seconds"))
        .and_then(serde_yaml_ng::Value::as_i64)
        .unwrap_or(0);
    (seconds > 0).then(|| Duration::from_secs(seconds as u64))
}

/// `requests.passthrough-headers` (Go `PassthroughHeadersEnabled`): upstream response
/// headers reach the client. Off by default.
pub fn passthrough_headers(cfg: &Config) -> bool {
    cfg.document
        .get("requests")
        .and_then(|r| r.get("passthrough-headers"))
        .and_then(serde_yaml_ng::Value::as_bool)
        .unwrap_or(false)
}

/// Go `cpaReservedResponseHeaders`: headers CPA manages itself.
pub fn reserved_response_header(name: &str) -> bool {
    [
        "access-control-allow-credentials",
        "access-control-allow-headers",
        "access-control-allow-methods",
        "access-control-allow-origin",
        "access-control-expose-headers",
        "access-control-max-age",
        "x-cpa-trace-id",
    ]
    .iter()
    .any(|r| name.eq_ignore_ascii_case(r))
}

/// Go `hopByHopHeaders`: RFC 7230 hop-by-hop headers, `Set-Cookie`, and the framing
/// headers CPA sets itself.
fn hop_by_hop(name: &str) -> bool {
    [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "set-cookie",
        "content-length",
        "content-encoding",
    ]
    .iter()
    .any(|h| name.eq_ignore_ascii_case(h))
}

/// Go `FilterUpstreamHeaders`: upstream headers without hop-by-hop, reserved,
/// `Connection`-listed and AI-gateway headers (Claude Code reports gateways it sees).
pub fn filter_upstream_headers(src: &axum::http::HeaderMap) -> axum::http::HeaderMap {
    const GATEWAY_PREFIXES: [&str; 6] = ["x-litellm-", "helicone-", "x-portkey-", "cf-aig-", "x-kong-", "x-bt-"];
    let scoped: Vec<String> = src
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    let mut out = axum::http::HeaderMap::new();
    for (name, value) in src {
        let key = name.as_str();
        if hop_by_hop(key)
            || reserved_response_header(key)
            || scoped.iter().any(|s| s == key)
            || GATEWAY_PREFIXES.iter().any(|p| key.starts_with(p))
        {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

/// Go `WriteUpstreamHeaders`: adds each upstream header the response does not set yet.
pub fn write_upstream_headers(dst: &mut axum::http::HeaderMap, src: &axum::http::HeaderMap) {
    for name in src.keys() {
        if dst.contains_key(name) {
            continue;
        }
        for value in src.get_all(name) {
            dst.append(name.clone(), value.clone());
        }
    }
}

/// Go `WriteErrorResponse` with passthrough on: the error's addon headers replace the
/// response's, reserved names excepted. Content-Type stays the response's: Go sets
/// `application/json` after copying the addon.
// ponytail: Go copies these unfiltered, so an upstream Content-Length (or other
// framing header) can declare the wrong length for CPA's error body; framing and
// hop-by-hop headers are dropped here to keep the response valid.
pub fn write_error_headers(dst: &mut axum::http::HeaderMap, src: &axum::http::HeaderMap) {
    for name in src.keys() {
        if reserved_response_header(name.as_str())
            || hop_by_hop(name.as_str())
            || name == axum::http::header::CONTENT_TYPE
        {
            continue;
        }
        dst.remove(name);
        for value in src.get_all(name) {
            dst.append(name.clone(), value.clone());
        }
    }
}

/// How one route frames a stream.
pub trait Writer: Send + 'static {
    /// One upstream event in, the bytes to write out.
    fn chunk(&mut self, event: Bytes) -> Vec<Bytes>;
    /// Set after a chunk carried a terminal error: stop forwarding.
    fn stopped(&self) -> bool {
        false
    }
    /// Bytes for a terminal error after the stream committed.
    fn error(&mut self, error: &ExecError) -> Vec<Bytes>;
    /// Bytes for a clean end of stream.
    fn end(&mut self) -> Vec<Bytes>;
    /// A heartbeat while idle. It is written only between SSE events; see [`resume`].
    fn heartbeat(&mut self) -> Bytes {
        Bytes::from_static(b": keep-alive\n\n")
    }
}

/// Forwards `first` and `rest` through `writer` as a response body. Without `first` (a
/// stream that committed on a served frame its delivery guard left out,
/// `dispatch::Done::Stream`), `rest` alone decides what follows and how it ends.
pub fn stream<W: Writer>(first: Option<Bytes>, rest: ExecStream, mut writer: W, keepalive: Option<Duration>) -> Body {
    let ready = first.map(|event| writer.chunk(event)).unwrap_or_default();
    let finished = writer.stopped();
    resume(ready, finished, rest, writer, keepalive)
}

/// Continues a stream whose first bytes the writer already produced.
///
/// A heartbeat is written only where an SSE event may start ([`SseTail`]): after a
/// blank line, whatever its line ends, or before the first byte. A writer that forwards
/// upstream bytes as they were read can leave the body inside a line or inside a
/// multi-line event, and a comment there would split or corrupt that event (Go v8.0.67
/// 91e0dee8 does this for image streams). A tick that finds the body mid-event writes
/// nothing; the next tick tries again.
pub fn resume<W: Writer>(
    ready: Vec<Bytes>,
    finished: bool,
    rest: ExecStream,
    writer: W,
    keepalive: Option<Duration>,
) -> Body {
    let ready: VecDeque<Bytes> = ready.into();
    struct State<W> {
        rest: ExecStream,
        writer: W,
        ready: VecDeque<Bytes>,
        finished: bool,
        ticker: Option<tokio::time::Interval>,
        /// Where everything written so far ends.
        tail: SseTail,
    }
    let ticker = keepalive.map(|period| {
        let mut t = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        t
    });
    let state = State {
        rest,
        writer,
        ready,
        finished,
        ticker,
        tail: SseTail::START,
    };
    let body = futures_util::stream::unfold(state, |mut s| async move {
        loop {
            if let Some(bytes) = s.ready.pop_front() {
                s.tail = s.tail.after(&bytes);
                return Some((Ok::<_, std::convert::Infallible>(bytes), s));
            }
            if s.finished {
                return None;
            }
            let next = match s.ticker.as_mut() {
                Some(ticker) => tokio::select! {
                    item = s.rest.next() => Some(item),
                    _ = ticker.tick() => None,
                },
                None => Some(s.rest.next().await),
            };
            match next {
                None if !s.tail.at_boundary() => {}
                None => {
                    let beat = s.writer.heartbeat();
                    s.ready.push_back(beat);
                }
                Some(Some(Ok(event))) => {
                    s.ready.extend(s.writer.chunk(event));
                    s.finished = s.writer.stopped();
                }
                Some(Some(Err(error))) => {
                    s.ready.extend(s.writer.error(&error));
                    s.finished = true;
                }
                Some(None) => {
                    s.ready.extend(s.writer.end());
                    s.finished = true;
                }
            }
        }
    });
    Body::from_stream(body)
}

/// Where an event stream's bytes so far end, for SSE framing. In `text/event-stream`
/// (WHATWG HTML) a line ends at CRLF, at LF or at a lone CR, and a blank line ends an
/// event, so the next byte may start a new one. A CR may be the first half of a CRLF
/// that the next chunk completes: an LF right after a CR ends no further line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SseTail {
    /// Line ends after the last other byte, counted up to 2 (a blank line).
    ends: u8,
    /// The last byte was CR.
    cr: bool,
}

impl SseTail {
    /// Before the first byte, where an event may start.
    pub(crate) const START: SseTail = SseTail { ends: 2, cr: false };

    /// The next byte starts an event: nothing is left inside a line or an event.
    pub(crate) fn at_boundary(self) -> bool {
        self.ends >= 2
    }

    /// The tail once `chunk` follows. Only the trailing run of CR and LF bytes is read,
    /// so following a stream costs no more than its length.
    pub(crate) fn after(self, chunk: &[u8]) -> SseTail {
        let run = chunk.iter().rev().take_while(|&&b| b == b'\r' || b == b'\n').count();
        let mut tail = if run == chunk.len() {
            self
        } else {
            SseTail { ends: 0, cr: false }
        };
        for &byte in &chunk[chunk.len() - run..] {
            tail = match (byte, tail.cr) {
                // The LF of a CRLF whose CR already ended the line.
                (b'\n', true) => SseTail {
                    ends: tail.ends,
                    cr: false,
                },
                (b'\n', false) => SseTail {
                    ends: (tail.ends + 1).min(2),
                    cr: false,
                },
                _ => SseTail {
                    ends: (tail.ends + 1).min(2),
                    cr: true,
                },
            };
        }
        tail
    }

    /// The bytes that end whatever line and event the body is inside, so the next byte
    /// starts an event: none at a boundary. After a CR the first LF only completes the
    /// CRLF, so it takes two.
    pub(crate) fn close(self) -> &'static [u8] {
        if self.at_boundary() {
            b""
        } else if self.ends == 1 && !self.cr {
            b"\n"
        } else {
            b"\n\n"
        }
    }
}

/// Normalizes an executor event to SSE payload lines: `data: x` frames pass through,
/// bare JSON becomes `data: x\n\n`.
pub fn ensure_frame(event: Bytes) -> Bytes {
    let trimmed = event.trim_ascii_start();
    if trimmed.starts_with(b"data:") || trimmed.starts_with(b"event:") || trimmed.starts_with(b":") {
        if event.ends_with(b"\n\n") {
            event
        } else {
            let mut out = event.to_vec();
            out.extend_from_slice(if event.ends_with(b"\n") { b"\n" } else { b"\n\n" });
            Bytes::from(out)
        }
    } else {
        Bytes::from([b"data: ".as_slice(), &event, b"\n\n"].concat())
    }
}

/// The `data:` payloads of one SSE frame, joined with newlines.
pub fn data_payload(frame: &[u8]) -> Option<Vec<u8>> {
    let mut out: Option<Vec<u8>> = None;
    for line in frame.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line).trim_ascii();
        let Some(data) = line.strip_prefix(b"data:") else {
            continue;
        };
        let data = data.trim_ascii();
        match &mut out {
            Some(buf) => {
                buf.push(b'\n');
                buf.extend_from_slice(data);
            }
            None => out = Some(data.to_vec()),
        }
    }
    out
}

/// The `event:` name of one SSE frame.
pub fn event_name(frame: &[u8]) -> String {
    frame
        .split(|&b| b == b'\n')
        .find_map(|line| line.trim_ascii().strip_prefix(b"event:"))
        .map(|name| String::from_utf8_lossy(name.trim_ascii()).into_owned())
        .unwrap_or_default()
}

/// An upstream stream that yields `fragments` as reads, the first at once and each
/// later one `pause_ms` after the one before.
#[cfg(test)]
pub(crate) fn paced(fragments: &[&str], pause_ms: u64) -> ExecStream {
    let fragments: Vec<Bytes> = fragments.iter().map(|f| Bytes::copy_from_slice(f.as_bytes())).collect();
    futures_util::stream::iter(fragments.into_iter().enumerate())
        .then(move |(i, fragment)| async move {
            if i > 0 {
                tokio::time::sleep(Duration::from_millis(pause_ms)).await;
            }
            Ok::<_, ExecError>(fragment)
        })
        .boxed()
}

/// An event stream as a client parses it (WHATWG HTML `text/event-stream`): the
/// dispatched events as (`event` name, joined `data`), and the number of comment lines.
/// Lines end at CRLF, LF or CR; an event the body cuts off is not dispatched.
#[cfg(test)]
pub(crate) fn parse_sse(body: &[u8]) -> (Vec<(String, String)>, usize) {
    let text = String::from_utf8_lossy(body);
    let mut rest = &*text;
    let (mut events, mut comments) = (Vec::new(), 0);
    let (mut name, mut data): (String, Option<String>) = (String::new(), None);
    while let Some(end) = rest.find(['\r', '\n']) {
        let line = &rest[..end];
        rest = &rest[end + if rest[end..].starts_with("\r\n") { 2 } else { 1 }..];
        if line.is_empty() {
            if let Some(data) = data.take() {
                events.push((std::mem::take(&mut name), data));
            }
            name.clear();
            continue;
        }
        if line.starts_with(':') {
            comments += 1;
            continue;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => name = value.to_owned(),
            "data" => match &mut data {
                Some(data) => {
                    data.push('\n');
                    data.push_str(value);
                }
                None => data = Some(value.to_owned()),
            },
            _ => {}
        }
    }
    (events, comments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct Plain;
    impl Writer for Plain {
        fn chunk(&mut self, event: Bytes) -> Vec<Bytes> {
            vec![event]
        }
        fn error(&mut self, _: &ExecError) -> Vec<Bytes> {
            vec![Bytes::from_static(b"ERR")]
        }
        fn end(&mut self) -> Vec<Bytes> {
            vec![Bytes::from_static(b"END")]
        }
    }

    async fn collect(body: Body) -> String {
        String::from_utf8(axum::body::to_bytes(body, usize::MAX).await.unwrap().to_vec()).unwrap()
    }

    #[tokio::test]
    async fn forwards_then_ends_or_errors_and_beats_while_idle() {
        let rest = futures_util::stream::iter([Ok(Bytes::from_static(b"b"))]).boxed();
        assert_eq!(
            collect(stream(Some(Bytes::from_static(b"a")), rest, Plain, None)).await,
            "abEND"
        );
        let err = cpa_core::exec::ExecError::local(502, cpa_core::exec::FailureScope::Transport, "x");
        let rest = futures_util::stream::iter([Err(err)])
            .chain(futures_util::stream::pending())
            .boxed();
        assert_eq!(
            collect(stream(Some(Bytes::from_static(b"a")), rest, Plain, None)).await,
            "aERR"
        );
        // Without a first payload, `rest` alone decides the body: its events, then its
        // end or its error. The body never ends before `rest` does.
        assert_eq!(
            collect(stream(None, futures_util::stream::empty().boxed(), Plain, None)).await,
            "END"
        );
        let rest = futures_util::stream::iter([Ok(Bytes::from_static(b"b"))]).boxed();
        assert_eq!(collect(stream(None, rest, Plain, None)).await, "bEND");
        let err = cpa_core::exec::ExecError::local(502, cpa_core::exec::FailureScope::Transport, "x");
        let rest = futures_util::stream::iter([Err(err)])
            .chain(futures_util::stream::pending())
            .boxed();
        assert_eq!(collect(stream(None, rest, Plain, None)).await, "ERR");
        tokio::time::pause();
        let slow = futures_util::stream::once(async {
            tokio::time::sleep(Duration::from_millis(250)).await;
            Ok(Bytes::from_static(b"z"))
        })
        .boxed();
        let body = stream(None, slow, Plain, Some(Duration::from_millis(100)));
        assert_eq!(collect(body).await, ": keep-alive\n\n: keep-alive\n\nzEND");
        let slow = futures_util::stream::once(async {
            tokio::time::sleep(Duration::from_millis(250)).await;
            Ok(Bytes::from_static(b"z"))
        })
        .boxed();
        let body = stream(
            Some(Bytes::from_static(b"data: a\n\n")),
            slow,
            Plain,
            Some(Duration::from_millis(100)),
        );
        assert_eq!(collect(body).await, "data: a\n\n: keep-alive\n\n: keep-alive\n\nzEND");
    }

    /// The tail of a stream after `chunks`.
    fn tail(chunks: &[&[u8]]) -> SseTail {
        chunks.iter().fold(SseTail::START, |tail, chunk| tail.after(chunk))
    }

    /// Whether `body` leaves a client ready for a new event: an event that follows is
    /// parsed on its own, and the events `body` dispatched stay as they were.
    fn client_at_boundary(body: &[u8]) -> bool {
        let mut next = body.to_vec();
        next.extend_from_slice(b"data: z\n\n");
        let (mut events, _) = parse_sse(&next);
        events.pop() == Some((String::new(), "z".to_owned())) && events == parse_sse(body).0
    }

    /// #105: LF, CRLF and lone-CR line ends, every one of them also split across chunk
    /// edges, including a CRLF whose CR and LF arrive apart. The tail agrees with a
    /// client's parse whatever the split, and `close` ends exactly the open event.
    #[test]
    fn sse_tail_follows_lf_crlf_and_lone_cr_across_any_split() {
        let cases: [(&str, bool, &str); 20] = [
            ("", true, ""),
            ("data: x", false, "\n\n"),
            ("data: x\n", false, "\n"),
            ("data: x\n\n", true, ""),
            ("data: x\n\n\n", true, ""),
            ("data: x\r\n", false, "\n"),
            ("data: x\r\n\r\n", true, ""),
            ("data: x\r\n\n", true, ""),
            ("data: x\r", false, "\n\n"),
            ("data: x\r\r", true, ""),
            ("data: x\r\n\r", true, ""),
            ("data: x\n\r", true, ""),
            ("data: a\ndata: b\n", false, "\n"),
            ("data: a\rdata: b\r", false, "\n\n"),
            ("data: a\r\ndata: b\r\n", false, "\n"),
            ("data: a\ndata: b\n\n", true, ""),
            ("event: e\n", false, "\n"),
            (": comment\n\n", true, ""),
            ("data: x\n\ndata: y", false, "\n\n"),
            ("data: x\r\n\r\ndata: y\r", false, "\n\n"),
        ];
        for (body, boundary, close) in cases {
            let bytes = body.as_bytes();
            let whole = tail(&[bytes]);
            assert_eq!(whole.at_boundary(), boundary, "{body:?}");
            assert_eq!(client_at_boundary(bytes), boundary, "{body:?}: the client disagrees");
            assert_eq!(whole.close(), close.as_bytes(), "{body:?}");
            let closed = [bytes, whole.close()].concat();
            assert!(tail(&[closed.as_slice()]).at_boundary(), "{body:?}: closed");
            assert!(client_at_boundary(&closed), "{body:?}: the client is not at a boundary");
            for cut in 0..=bytes.len() {
                assert_eq!(tail(&[&bytes[..cut], &bytes[cut..]]), whole, "{body:?} cut at {cut}");
            }
            let bytewise: Vec<&[u8]> = bytes.chunks(1).collect();
            assert_eq!(tail(&bytewise), whole, "{body:?} byte by byte");
        }
        let chunks = |chunks: &[&str]| chunks.iter().fold(SseTail::START, |tail, c| tail.after(c.as_bytes()));
        // A CRLF split across chunks ends one line, not two.
        assert!(!chunks(&["data: x\r", "\n"]).at_boundary());
        assert!(chunks(&["data: x\r", "\n", "\r", "\n"]).at_boundary());
        // A tail already at a boundary stays there after more line ends.
        assert!(chunks(&["data: x\n\n", "\r\n", "\n"]).at_boundary());
    }

    /// Forwards upstream bytes as they were read, the way the image route does.
    struct Forward;
    impl Writer for Forward {
        fn chunk(&mut self, event: Bytes) -> Vec<Bytes> {
            vec![event]
        }
        fn error(&mut self, _: &ExecError) -> Vec<Bytes> {
            Vec::new()
        }
        fn end(&mut self) -> Vec<Bytes> {
            Vec::new()
        }
    }

    /// #105: upstream reads that stop inside a line, inside a multi-line event, between
    /// the CR and LF of a CRLF, and after lone-CR lines, each followed by a pause of
    /// several heartbeat periods. Every heartbeat starts at an event boundary (before
    /// #105 one landed in the middle of the first `data:` line), a boundary followed by
    /// a pause still gets its heartbeat, the client parses the same events as the
    /// upstream sent, and removing the heartbeats gives back the upstream bytes.
    #[tokio::test]
    async fn heartbeats_wait_for_an_event_boundary() {
        tokio::time::pause();
        let beat = b": keep-alive\n\n";
        let partial = r#"{"b64_json":"AAABBB"}"#.to_owned() + "\n" + r#"{"second":"line"}"#;
        let cases = [
            (
                "LF, cut inside a line and inside a multi-line event",
                vec![
                    "event: image_generation.partial_image\ndata: {\"b64_json\":\"AAA",
                    "BBB\"}\n",
                    "data: {\"second\":\"line\"}\n",
                    "\n",
                    "data: {\"type\":\"done\"}\n\n",
                ],
                vec![
                    ("image_generation.partial_image", partial),
                    ("", r#"{"type":"done"}"#.to_owned()),
                ],
                "data: {\"second\":\"line\"}\n\n: keep-alive\n\n",
            ),
            (
                "CRLF, with a CR and its LF read apart",
                vec!["data: {\"type\":\"a\"}\r", "\n", "\r", "\n", "data: b\r\n\r\n"],
                vec![("", r#"{"type":"a"}"#.to_owned()), ("", "b".to_owned())],
                "data: {\"type\":\"a\"}\r\n\r: keep-alive\n\n",
            ),
            (
                "lone CR",
                vec!["data: a\r", "\r", "data: b\rdata: c\r", "\r"],
                vec![("", "a".to_owned()), ("", "b\nc".to_owned())],
                "data: a\r\r: keep-alive\n\n",
            ),
        ];
        for (case, fragments, want, kept) in cases {
            let body = stream(None, paced(&fragments, 250), Forward, Some(Duration::from_millis(100)));
            let out = axum::body::to_bytes(body, usize::MAX).await.unwrap();
            let shown = String::from_utf8_lossy(&out);
            let mut payload = Vec::new();
            let mut at = 0;
            while let Some(found) = out[at..].windows(beat.len()).position(|w| w == beat) {
                let start = at + found;
                payload.extend_from_slice(&out[at..start]);
                assert!(
                    tail(&[&out[..start]]).at_boundary(),
                    "{case}: heartbeat at byte {start} inside an event: {shown:?}"
                );
                at = start + beat.len();
            }
            payload.extend_from_slice(&out[at..]);
            let upstream = fragments.concat();
            assert_eq!(payload, upstream.as_bytes(), "{case}: the upstream bytes changed");
            let (events, _) = parse_sse(&out);
            assert_eq!(events, parse_sse(upstream.as_bytes()).0, "{case}: {shown:?}");
            let want: Vec<(String, String)> = want.into_iter().map(|(name, data)| (name.to_owned(), data)).collect();
            assert_eq!(events, want, "{case}");
            assert!(
                shown.contains(kept),
                "{case}: no heartbeat after the boundary {kept:?}: {shown:?}"
            );
        }
    }

    /// While the body stays inside an event, whether a line is open or only the blank
    /// line is missing (after LF, CRLF or a lone CR), the ticker writes nothing,
    /// however long the pause; a cancelled body drops the upstream stream at once.
    #[tokio::test]
    async fn a_body_left_inside_an_event_gets_no_heartbeat_and_cancels_its_upstream() {
        tokio::time::pause();
        struct Guard(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        for open in ["data: {\"a\"", "data: a\n", "data: a\r\n", "data: a\r"] {
            let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let guard = Guard(dropped.clone());
            let rest = futures_util::stream::iter([Ok(Bytes::from_static(open.as_bytes()))])
                .chain(
                    futures_util::stream::pending().map(move |item: Result<Bytes, ExecError>| {
                        let _guard = &guard;
                        item
                    }),
                )
                .boxed();
            let mut body = stream(None, rest, Forward, Some(Duration::from_millis(100)));
            let mut frames = Vec::new();
            let _ = tokio::time::timeout(Duration::from_secs(5), async {
                while let Some(frame) =
                    std::future::poll_fn(|cx| axum::body::HttpBody::poll_frame(std::pin::Pin::new(&mut body), cx)).await
                {
                    frames.push(frame.unwrap().into_data().unwrap());
                }
            })
            .await;
            assert_eq!(
                frames,
                [Bytes::from_static(open.as_bytes())],
                "{open:?}: no heartbeat inside the event"
            );
            assert!(!dropped.load(std::sync::atomic::Ordering::SeqCst), "{open:?}");
            drop(body);
            assert!(
                dropped.load(std::sync::atomic::Ordering::SeqCst),
                "{open:?}: cancel drops the upstream"
            );
        }
    }

    #[test]
    fn frames_and_payloads() {
        assert_eq!(ensure_frame(Bytes::from_static(b"{}")), "data: {}\n\n");
        assert_eq!(ensure_frame(Bytes::from_static(b"data: {}\n")), "data: {}\n\n");
        assert_eq!(
            data_payload(b"event: x\r\ndata: {\"a\":1}\r\n\r\n").unwrap(),
            b"{\"a\":1}"
        );
        assert_eq!(
            event_name(b"event: response.completed\ndata: {}\n\n"),
            "response.completed"
        );
        assert!(data_payload(b": ping\n\n").is_none());
    }

    /// Go `handlers.FilterUpstreamHeaders` (tests/reference/server/main.go
    /// `upstreamHeaderFilters`).
    #[test]
    fn upstream_header_filter_matches_go() {
        use std::collections::BTreeMap;
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/server_go.json")).unwrap();
        let as_map = |v: &serde_json::Value| -> BTreeMap<String, Vec<String>> {
            v.as_object()
                .unwrap()
                .iter()
                .map(|(k, vs)| {
                    let values = vs.as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_owned());
                    (k.to_ascii_lowercase(), values.collect())
                })
                .collect()
        };
        let cases = fixture["upstream_headers"].as_array().unwrap();
        assert_eq!(cases.len(), 2);
        for case in cases {
            let mut src = axum::http::HeaderMap::new();
            for (name, values) in as_map(&case["in"]) {
                for value in values {
                    src.append(
                        axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                        HeaderValue::from_str(&value).unwrap(),
                    );
                }
            }
            let got = filter_upstream_headers(&src);
            let mut got_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for (name, value) in &got {
                got_map
                    .entry(name.as_str().to_owned())
                    .or_default()
                    .push(value.to_str().unwrap().to_owned());
            }
            assert_eq!(got_map, as_map(&case["out"]));
        }
    }

    /// Go `WriteUpstreamHeaders` never overwrites what the handler set;
    /// `WriteErrorResponse` replaces with the error's headers but keeps reserved ones.
    #[test]
    fn upstream_and_error_header_writes_follow_go() {
        let headers = |pairs: &[(&'static str, &'static str)]| {
            let mut h = axum::http::HeaderMap::new();
            for (k, v) in pairs {
                h.append(*k, HeaderValue::from_static(v));
            }
            h
        };
        let mut dst = headers(&[("content-type", "application/json")]);
        write_upstream_headers(
            &mut dst,
            &headers(&[("content-type", "text/plain"), ("x-a", "1"), ("x-a", "2")]),
        );
        assert_eq!(dst["content-type"], "application/json");
        assert_eq!(dst.get_all("x-a").iter().collect::<Vec<_>>(), ["1", "2"]);

        let mut dst = headers(&[("content-type", "application/json"), ("x-cpa-trace-id", "t")]);
        write_error_headers(
            &mut dst,
            &headers(&[
                ("content-type", "text/html"),
                ("x-cpa-trace-id", "upstream"),
                ("content-length", "999"),
                ("x-b", "1"),
            ]),
        );
        assert_eq!(dst["content-type"], "application/json");
        assert_eq!(dst["x-cpa-trace-id"], "t");
        assert!(dst.get("content-length").is_none());
        assert_eq!(dst["x-b"], "1");
    }
}
