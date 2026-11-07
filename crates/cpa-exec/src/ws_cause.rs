//! Transport diagnostics for the upstream WebSocket: why a dial, read or write failed,
//! for the request log only (`ExecError::diagnostic`). The client-facing errors keep
//! their fixed texts.
//!
//! Each diagnostic reads `<kind>: <detail>`. The kind comes only from typed evidence
//! (wreq's classification, an `io::ErrorKind`, a BoringSSL error, tungstenite's error
//! variant, the inflating layer's own error type, or the local timer that fired); a
//! failure without such evidence is `unclassified`, with the library's text as the
//! detail. Library texts can embed URLs and header values: the request log redacts the
//! diagnostic before writing it, so nothing here may log it.

use std::error::Error as StdError;
use std::io;
use std::time::Duration;

use cpa_core::exec::Diagnostic;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::error::ProtocolError;

use crate::codex_ws::deflate::LayerError;

/// Source levels followed; library chains are a handful deep.
const MAX_DEPTH: usize = 8;

/// Each error in `error`'s chain, the error inside an `io::Error` included (its
/// `source()` skips that error and returns the inner one's source).
fn chain<'a>(error: &'a (dyn StdError + 'static)) -> impl Iterator<Item = &'a (dyn StdError + 'static)> {
    std::iter::successors(Some(error), |&e| next(e)).take(MAX_DEPTH)
}

fn next<'a>(error: &'a (dyn StdError + 'static)) -> Option<&'a (dyn StdError + 'static)> {
    match error.downcast_ref::<io::Error>().and_then(io::Error::get_ref) {
        Some(inner) => Some(inner),
        None => error.source(),
    }
}

/// The chain's texts joined with `: `, skipping a text an outer error already includes
/// (wreq appends its source to its own text).
fn text(error: &(dyn StdError + 'static)) -> String {
    let mut out = String::new();
    for e in chain(error) {
        let part = e.to_string();
        if part.is_empty() || out.contains(&part) {
            continue;
        }
        if !out.is_empty() {
            out.push_str(": ");
        }
        out.push_str(&part);
    }
    out
}

/// What the chain's types prove, most specific first.
fn evidence(error: &(dyn StdError + 'static)) -> Option<String> {
    let mut io_kind = None;
    for e in chain(error) {
        if e.is::<btls::ssl::Error>() || e.is::<btls::error::ErrorStack>() {
            return Some("tls".into());
        }
        match e.downcast_ref::<LayerError>() {
            Some(LayerError::Compression(_)) => return Some("compression".into()),
            Some(LayerError::Framing(_)) => return Some("protocol".into()),
            None => {}
        }
        if let Some(io) = e.downcast_ref::<io::Error>() {
            io_kind.get_or_insert(io.kind());
        }
    }
    io_kind.map(io_kind_label)
}

fn io_kind_label(kind: io::ErrorKind) -> String {
    match kind {
        io::ErrorKind::UnexpectedEof => "eof (UnexpectedEof)".into(),
        io::ErrorKind::TimedOut => "timeout (TimedOut)".into(),
        kind => format!("io ({kind:?})"),
    }
}

fn labelled(kind: &str, detail: &str) -> Diagnostic {
    Diagnostic::new(format!("{kind}: {detail}"))
}

/// A dial that got no HTTP response.
pub(crate) fn dial(error: &wreq::Error) -> Diagnostic {
    let found = evidence(error);
    let io = found.as_deref().filter(|k| k.starts_with("io (")).map(|k| &k[3..]);
    let kind = if error.is_dns() {
        "dns".to_owned()
    } else if error.is_proxy_connect() {
        "proxy_connect".to_owned()
    } else if found.as_deref() == Some("tls") || error.is_tls() {
        "tls".to_owned()
    } else if error.is_timeout() {
        "timeout".to_owned()
    } else if error.is_connect() {
        // The OS's reason when there is one: `connect (ConnectionRefused)`.
        match io {
            Some(io) => format!("connect {io}"),
            None => "connect".to_owned(),
        }
    } else {
        found.unwrap_or_else(|| "unclassified".to_owned())
    };
    labelled(&kind, &text(error))
}

/// A failed read or write on an established socket.
pub(crate) fn socket(error: &WsError) -> Diagnostic {
    let kind = match error {
        WsError::Io(io) => evidence(io).unwrap_or_else(|| "io".to_owned()),
        WsError::Protocol(ProtocolError::ResetWithoutClosingHandshake) => "eof".to_owned(),
        WsError::Protocol(_) | WsError::Utf8(_) | WsError::AttackAttempt => "protocol".to_owned(),
        WsError::Capacity(_) => "capacity".to_owned(),
        WsError::ConnectionClosed | WsError::AlreadyClosed => "closed".to_owned(),
        WsError::WriteBufferFull(_) => "io".to_owned(),
        _ => "unclassified".to_owned(),
    };
    labelled(&kind, &text(error))
}

/// The upstream ended the stream without a close frame.
pub(crate) fn stream_ended() -> Diagnostic {
    labelled("eof", "the stream ended without a close frame")
}

/// The upstream sent a close frame (`None`: one without a status code).
pub(crate) fn remote_close(code: Option<u16>) -> Diagnostic {
    match code {
        Some(code) => labelled("remote_close", &format!("code {code}")),
        None => labelled("remote_close", "no status code"),
    }
}

/// The proxy's own idle deadline fired.
pub(crate) fn idle_timeout(idle: Duration) -> Diagnostic {
    labelled("local_timeout", &format!("no application message within {idle:?}"))
}

/// The proxy's own write bound fired.
pub(crate) fn write_timeout(deadline: Duration, len: usize) -> Diagnostic {
    labelled(
        "local_timeout",
        &format!("writing a {len}-byte frame took longer than {deadline:?}"),
    )
}

/// The proxy's own handshake deadline fired.
pub(crate) fn handshake_timeout(deadline: Duration) -> Diagnostic {
    labelled("local_timeout", &format!("no upgrade within {deadline:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn io_with(kind: io::ErrorKind, inner: impl Into<Box<dyn StdError + Send + Sync>>) -> io::Error {
        io::Error::new(kind, inner)
    }

    #[test]
    fn socket_errors_are_labelled_from_their_types() {
        let cases: Vec<(WsError, &str)> = vec![
            (
                WsError::Io(io::Error::from(io::ErrorKind::ConnectionReset)),
                "io (ConnectionReset): IO error: connection reset",
            ),
            (
                WsError::Io(io::Error::from(io::ErrorKind::UnexpectedEof)),
                "eof (UnexpectedEof): IO error: unexpected end of file",
            ),
            (
                WsError::Io(io::Error::from(io::ErrorKind::TimedOut)),
                "timeout (TimedOut): IO error: timed out",
            ),
            (
                WsError::Protocol(ProtocolError::ResetWithoutClosingHandshake),
                "eof: WebSocket protocol error: Connection reset without closing handshake",
            ),
            (
                WsError::Protocol(ProtocolError::NonZeroReservedBits),
                "protocol: WebSocket protocol error: Reserved bits are non-zero",
            ),
            (WsError::ConnectionClosed, "closed: Connection closed normally"),
            (
                WsError::Io(io_with(
                    io::ErrorKind::InvalidInput,
                    LayerError::Compression("corrupt deflate stream".into()),
                )),
                "compression: IO error: corrupt deflate stream",
            ),
            (
                WsError::Io(io_with(
                    io::ErrorKind::InvalidData,
                    LayerError::Framing("websocket: data before FIN"),
                )),
                "protocol: IO error: websocket: data before FIN",
            ),
        ];
        for (error, want) in cases {
            assert_eq!(socket(&error).as_str(), want, "{error:?}");
        }
    }

    /// A text the classifier does not recognise is kept, but the failure is not given a
    /// kind it has no evidence for, however much the text sounds like one.
    #[test]
    fn untyped_text_is_not_evidence() {
        let error = WsError::Io(io_with(io::ErrorKind::Other, "tls handshake failure: deflate"));
        assert_eq!(
            socket(&error).as_str(),
            "io (Other): IO error: tls handshake failure: deflate"
        );
    }

    #[test]
    fn local_and_remote_endings_are_distinct() {
        assert_eq!(
            idle_timeout(Duration::from_secs(300)).as_str(),
            "local_timeout: no application message within 300s"
        );
        assert_eq!(
            write_timeout(Duration::from_secs(10), 42).as_str(),
            "local_timeout: writing a 42-byte frame took longer than 10s"
        );
        assert_eq!(
            handshake_timeout(Duration::from_secs(30)).as_str(),
            "local_timeout: no upgrade within 30s"
        );
        assert_eq!(remote_close(Some(1011)).as_str(), "remote_close: code 1011");
        assert_eq!(remote_close(None).as_str(), "remote_close: no status code");
        assert_eq!(stream_ended().as_str(), "eof: the stream ended without a close frame");
    }
}
