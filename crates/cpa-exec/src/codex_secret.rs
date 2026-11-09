//! The native Codex routes' outbound credential guard (#25, acceptance criterion 7;
//! `docs/codex/CONTRACT.md` "Credential isolation").
//!
//! An upstream can write the authentication value it was sent into what it answers: an
//! error message that quotes the bearer token, a close reason, a header. Everything a
//! native route delivers is the upstream's own bytes, so such a value would reach a
//! client that never held it. The guard is the last check before delivery. It is bound to
//! the values one upstream request or socket actually carried ([`Secrets::from_request`]):
//! the `Authorization` credential, cookie values, API-key headers and a password in the
//! URL. It is never a global set of configured accounts, saved logins or a credential
//! name: a socket keeps the values of its own handshake, a fresh dial or a refreshed
//! request gets its own.
//!
//! The guard reads the original bytes, whatever a parser makes of them: a frame the
//! pinned client cannot decode, a repeated member a last-key reader drops, an array, a
//! private raw value and a JSON-escaped spelling (`at-FAKE`, a raw value's escaped
//! string inside another string) all count ([`Secrets::exposes`]). It decides delivery
//! only. Processing (eligibility, classification, settlement, cooldown, usage) has
//! already read the frame when the guard runs, and nothing here changes it:
//!
//! - a frame that carries a value is not delivered, and the response goes on as the
//!   processing decided (a skipped frame or notice: the real completion follows; a
//!   terminal event: the stream ends without it, and the client reads a stream that
//!   closed before completion);
//! - a failure that carries one becomes [`Delivery::Confidential`]: its classified status
//!   stays the scheduler's, and the client gets a silent close (WebSocket) or a
//!   status-only error (HTTP), never the frame, its body, a limit event or a close reason
//!   derived from it;
//! - a buffered answer that carries one is a request-scoped 502 (no failover, no
//!   cooldown), because a buffered client has no stream to end.
//!
//! Matching is literal. A value the upstream never received, such as the same text from
//! another account, is no secret here, and a frame without one keeps every byte.

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;

use bytes::Bytes;
use cpa_core::exec::{Delivery, ExecError, ExecResponse, ExecStream, FailureScope, ResponseBody};
use futures_util::StreamExt;
use http::HeaderMap;

/// Header names whose values authenticate a request. `chatgpt-account-id` is an account
/// identity, not an authentication value, and stays out (it is public routing data, and
/// the #6 account readings handle what names an account).
const AUTH_HEADERS: [&str; 6] = [
    "authorization",
    "proxy-authorization",
    "cookie",
    "x-api-key",
    "api-key",
    "x-goog-api-key",
];

/// Unescape rounds after the bytes themselves: a JSON string, a raw value's string inside
/// a JSON string, and so on. Each round halves a run of backslashes, so a value hidden
/// under more rounds than this needs a run of 65,536 backslashes; such bytes count as
/// carrying a value ([`Secrets::exposes`]).
// ponytail: at most 16 unescape rounds bound allocation and work; deeper encodings
// fail closed. Raise this ceiling only with an updated work bound and nested-escape tests.
const ROUNDS: usize = 16;

/// The text a buffered answer that carried an upstream authentication value fails with.
pub(crate) const WITHHELD_RESPONSE: &str = "codex upstream response withheld: it carried the upstream credential";

/// The authentication values one upstream request or socket carried. Empty values are
/// none. Its `Debug` names only how many there are.
#[derive(Clone, Default)]
pub(crate) struct Secrets(Arc<[Box<str>]>);

impl fmt::Debug for Secrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secrets({} redacted)", self.0.len())
    }
}

impl Secrets {
    /// The values `headers` (the request as sent upstream) and `url` carry: the credential
    /// of each `Authorization` (the scheme dropped; the whole value when it has no
    /// scheme), each cookie value, each API-key header, and the URL's password.
    pub(crate) fn from_request(headers: &HeaderMap, url: &str) -> Self {
        let mut out: Vec<Box<str>> = Vec::new();
        let mut add = |value: &str| {
            let value = value.trim();
            if !value.is_empty() && !out.iter().any(|s| &**s == value) {
                out.push(value.into());
            }
        };
        for name in AUTH_HEADERS {
            for value in headers.get_all(name) {
                let value = String::from_utf8_lossy(value.as_bytes());
                match name {
                    "cookie" => {
                        for pair in value.split(';') {
                            let value = pair.split_once('=').map_or(pair, |(_, v)| v).trim();
                            // RFC 6265 cookie-value permits surrounding quotes; they are
                            // syntax, not part of the authentication value (including "").
                            add(value
                                .strip_prefix('"')
                                .and_then(|v| v.strip_suffix('"'))
                                .unwrap_or(value));
                        }
                    }
                    // `Bearer <token>`: the token; `Bearer ` alone carries none.
                    "authorization" | "proxy-authorization" => {
                        let value = value.trim_start();
                        add(value
                            .split_once(char::is_whitespace)
                            .map_or(value, |(_, credential)| credential));
                    }
                    _ => add(&value),
                }
            }
        }
        if let Ok(url) = url::Url::parse(url)
            && let Some(password) = url.password()
        {
            add(&percent_decoded(password));
            add(password);
        }
        Self(out.into())
    }

    /// Whether `bytes` carry one of the values: as written, or under up to [`ROUNDS`]
    /// rounds of JSON unescaping (`\"`, `\\`, `\/`, the control escapes and `\uXXXX`,
    /// surrogate pairs included). Unescaping runs over every byte, inside strings or not,
    /// so a frame no JSON reader accepts is read too. Bytes still unescaping after the
    /// last round count as carrying one. Linear in `bytes` for each round, and a round
    /// runs only while the previous one changed something.
    pub(crate) fn exposes(&self, bytes: &[u8]) -> bool {
        if self.0.is_empty() || bytes.is_empty() {
            return false;
        }
        let mut layer = String::from_utf8_lossy(bytes);
        for _ in 0..=ROUNDS {
            if self.0.iter().any(|secret| layer.contains(&**secret)) {
                return true;
            }
            match unescape(&layer) {
                Some(next) => layer = Cow::Owned(next),
                None => return false,
            }
        }
        true
    }

    fn headers_expose(&self, headers: &HeaderMap) -> bool {
        headers
            .iter()
            .any(|(name, value)| self.exposes(name.as_str().as_bytes()) || self.exposes(value.as_bytes()))
    }

    /// Marks `error` [`Delivery::Confidential`] when anything a client could get of it
    /// carries a value: its body, the frame it would deliver as sent, the headers it
    /// hands the client (`addon`, and a direct response's headers). Its status, scope and
    /// body stay the scheduler's; it is no longer a direct response, and it hands the
    /// client no headers.
    pub(crate) fn guard_error(&self, error: &mut ExecError) {
        if self.0.is_empty() || error.confidential() {
            return;
        }
        let exposed = self.exposes(&error.body)
            || error.delivered_event().is_some_and(|frame| self.exposes(frame))
            || error.addon.as_deref().is_some_and(|h| self.headers_expose(h))
            // Handshake headers can also feed a derived subscription-limit event.
            || self.headers_expose(&error.headers);
        if exposed {
            *error = confidential(std::mem::replace(
                error,
                ExecError::local(0, FailureScope::Transport, ""),
            ));
        }
    }

    /// `error` as [`Self::guard_error`] leaves it.
    pub(crate) fn guarded(&self, mut error: ExecError) -> ExecError {
        self.guard_error(&mut error);
        error
    }

    /// `stream` without the frames that carry a value, and with its error guarded. The
    /// stream goes on after a withheld frame: its processing already decided what the
    /// frame does.
    pub(crate) fn guard_stream(&self, stream: ExecStream) -> ExecStream {
        if self.0.is_empty() {
            return stream;
        }
        let secrets = self.clone();
        stream
            .filter_map(move |item| {
                let out = match item {
                    Ok(frame) if secrets.exposes(&frame) => None,
                    Ok(frame) => Some(Ok(frame)),
                    Err(error) => Some(Err(secrets.guarded(error))),
                };
                std::future::ready(out)
            })
            .boxed()
    }

    /// A native route's answer, guarded: its error ([`Self::guard_error`]), its stream
    /// ([`Self::guard_stream`]), its upstream headers (a header that carries a value is
    /// dropped), and a buffered body, which fails request-scoped when it carries one.
    pub(crate) fn guard_response(&self, result: Result<ExecResponse, ExecError>) -> Result<ExecResponse, ExecError> {
        if self.0.is_empty() {
            return result;
        }
        let mut response = result.map_err(|error| self.guarded(error))?;
        let exposing: Vec<_> = response
            .headers
            .iter()
            .filter(|(name, value)| self.exposes(name.as_str().as_bytes()) || self.exposes(value.as_bytes()))
            .map(|(name, _)| name.clone())
            .collect();
        for name in exposing {
            response.headers.remove(name);
        }
        response.body = match response.body {
            ResponseBody::Stream(stream) => ResponseBody::Stream(self.guard_stream(stream)),
            ResponseBody::Buffered(body) if self.exposes(&body) => return Err(withheld_response()),
            body @ ResponseBody::Buffered(_) => body,
        };
        Ok(response)
    }

    /// `ack`, an interrupt acknowledgement read from the socket, unless it carries a value.
    pub(crate) fn deliverable(&self, frame: Bytes) -> Option<Bytes> {
        (!self.exposes(&frame)).then_some(frame)
    }
}

/// The text a native WebSocket connection ends with when a frame its client's reader
/// would end a response on carried an upstream authentication value.
pub(crate) const WITHHELD_FRAME: &str = "codex upstream frame withheld: it carried the upstream credential";

/// `error` as [`Delivery::Confidential`]: its classification stays.
pub(crate) fn confidential(mut error: ExecError) -> ExecError {
    error.delivery = Delivery::Confidential;
    error.direct = false;
    error.delivered_status = None;
    error.addon = None;
    error
}

/// A buffered answer that carried an upstream authentication value: request-scoped, so
/// the attempt neither fails over (the upstream served it) nor cools the credential.
pub(crate) fn withheld_response() -> ExecError {
    confidential(ExecError::local(502, FailureScope::Request, WITHHELD_RESPONSE))
}

/// `text` with its `%XX` escapes decoded, as a URL's userinfo is sent.
fn percent_decoded(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok());
        match hex
            .filter(|_| b[i] == b'%')
            .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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
#[path = "codex_secret_tests.rs"]
mod tests;
