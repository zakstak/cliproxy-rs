//! The native Codex routes' outbound credential guard (#25, acceptance criterion 7;
//! `docs/codex/CONTRACT.md` "Credential isolation").
//!
//! An upstream can write the authentication value it was sent into what it answers: an
//! error message that quotes the bearer token, a close reason, a header. Everything a
//! native route delivers is the upstream's own bytes, so such a value would reach a
//! client that never held it. The guard is the last check before delivery. It is bound to
//! the values one upstream request or socket actually carried ([`from_request`]):
//! the `Authorization` credential, cookie values, API-key headers and a password in the
//! URL. It is never a global set of configured accounts, saved logins or a credential
//! name: a socket keeps the values of its own handshake, a fresh dial or a refreshed
//! request gets its own.
//!
//! The guard reads the original bytes, whatever a parser makes of them: a frame the
//! pinned client cannot decode, a repeated member a last-key reader drops, an array, a
//! private raw value and a JSON-escaped spelling (`at-FAKE`, a raw value's escaped
//! string inside another string) all count ([`SentCredentials::exposes`]). It decides delivery
//! only. Processing (eligibility, classification, settlement, cooldown, usage) has
//! already read the frame when the guard runs, and nothing here changes it:
//!
//! - a frame that carries a value is not delivered, and the response goes on as the
//!   processing decided (a skipped frame or notice: the real completion follows; a
//!   terminal event: the stream ends without it, and the client reads a stream that
//!   closed before completion). A withheld frame was still served: the response records
//!   it ([`ExecResponse::omitted`]), so the stream commits on it as on a delivered frame,
//!   even when nothing else is ever delivered, and the attempt keeps the outcome and
//!   usage its processing gave it instead of ending as an empty stream that runs again;
//! - a failure that carries one becomes [`Delivery::Confidential`]: its classified status
//!   stays the scheduler's, and the client gets a silent close (WebSocket) or a
//!   status-only error (HTTP), never the frame, its body, a limit event or a close reason
//!   derived from it;
//! - a buffered answer that carries one is a request-scoped 502 (no failover, no
//!   cooldown), because a buffered client has no stream to end.
//!
//! Matching is literal. A value the upstream never received, such as the same text from
//! another account, is no secret here, and a frame without one keeps every byte. The same
//! values go to the request log and the usage record, which keep what they record of the
//! exchange scrubbed of them ([`SentCredentials::scrub`]).

use bytes::Bytes;
use cpa_core::exec::{
    Delivery, ExecError, ExecResponse, ExecStream, FailureScope, Omissions, ResponseBody, SentCredentials,
};
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

/// The text a buffered answer that carried an upstream authentication value fails with.
pub(crate) const WITHHELD_RESPONSE: &str = "codex upstream response withheld: it carried the upstream credential";

/// The values `headers` (the request as sent upstream) and `url` carry: the credential of
/// each `Authorization` (the scheme dropped; the whole value when it has no scheme), each
/// cookie value, each API-key header, and the URL's password.
pub(crate) fn from_request(headers: &HeaderMap, url: &str) -> SentCredentials {
    let mut values: Vec<String> = Vec::new();
    for name in AUTH_HEADERS {
        for value in headers.get_all(name) {
            let value = String::from_utf8_lossy(value.as_bytes());
            match name {
                "cookie" => {
                    for pair in value.split(';') {
                        let value = pair.split_once('=').map_or(pair, |(_, v)| v).trim();
                        // RFC 6265 cookie-value permits surrounding quotes; they are
                        // syntax, not part of the authentication value (including "").
                        values.push(
                            value
                                .strip_prefix('"')
                                .and_then(|v| v.strip_suffix('"'))
                                .unwrap_or(value)
                                .to_owned(),
                        );
                    }
                }
                // `Bearer <token>`: the token; `Bearer ` alone carries none.
                "authorization" | "proxy-authorization" => {
                    let value = value.trim_start();
                    values.push(
                        value
                            .split_once(char::is_whitespace)
                            .map_or(value, |(_, credential)| credential)
                            .to_owned(),
                    );
                }
                _ => values.push(value.into_owned()),
            }
        }
    }
    if let Ok(url) = url::Url::parse(url)
        && let Some(password) = url.password()
    {
        values.push(percent_decoded(password));
        values.push(password.to_owned());
    }
    SentCredentials::new(values.iter().map(String::as_str))
}

/// The delivery guard over [`SentCredentials`]: what a native route hands its client,
/// checked against the values one upstream request or socket carried.
pub(crate) trait Guard {
    /// Marks `error` [`Delivery::Confidential`] when anything a client could get of it
    /// carries a value: its body, its original frame (delivered as sent, or withheld
    /// while a limit event may be derived from its body), the headers it hands the client
    /// (`addon`, and a direct response's headers). Its status, scope and body stay the
    /// scheduler's; it is no longer a direct response, and it hands the client no headers.
    fn guard_error(&self, error: &mut ExecError);
    /// `error` as [`Guard::guard_error`] leaves it.
    fn guarded(&self, error: ExecError) -> ExecError;
    /// `stream` without the frames that carry a value, and with its error guarded. The
    /// stream goes on after a withheld frame: its processing already decided what the
    /// frame does. Each withheld frame is recorded in `omitted` before the stream yields
    /// its next item.
    fn guard_stream(&self, stream: ExecStream, omitted: &Omissions) -> ExecStream;
    /// A native route's answer, guarded: its error ([`Guard::guard_error`]), its stream
    /// ([`Guard::guard_stream`], with [`ExecResponse::omitted`] tracking what it leaves
    /// out), its upstream headers (a header that carries a value is dropped), and a
    /// buffered body, which fails request-scoped when it carries one.
    fn guard_response(&self, result: Result<ExecResponse, ExecError>) -> Result<ExecResponse, ExecError>;
    /// `ack`, an interrupt acknowledgement read from the socket, unless it carries a value.
    fn deliverable(&self, frame: Bytes) -> Option<Bytes>;
}

fn headers_expose(sent: &SentCredentials, headers: &HeaderMap) -> bool {
    headers
        .iter()
        .any(|(name, value)| sent.exposes(name.as_str().as_bytes()) || sent.exposes(value.as_bytes()))
}

impl Guard for SentCredentials {
    fn guard_error(&self, error: &mut ExecError) {
        if self.is_empty() || error.confidential() {
            return;
        }
        // The original frame, delivered as sent or withheld: a withheld one still feeds
        // the client a limit event derived from `body`, which can lack the member that
        // carries the value.
        let exposed = self.exposes(&error.body)
            || error.original_frame().is_some_and(|frame| self.exposes(frame))
            || error.addon.as_deref().is_some_and(|h| headers_expose(self, h))
            // Handshake headers can also feed a derived subscription-limit event.
            || headers_expose(self, &error.headers);
        if exposed {
            *error = confidential(
                std::mem::replace(error, ExecError::local(0, FailureScope::Transport, "")),
                self,
            );
        }
    }

    fn guarded(&self, mut error: ExecError) -> ExecError {
        self.guard_error(&mut error);
        error
    }

    fn guard_stream(&self, stream: ExecStream, omitted: &Omissions) -> ExecStream {
        if self.is_empty() {
            return stream;
        }
        let (secrets, omitted) = (self.clone(), omitted.clone());
        stream
            .filter_map(move |item| {
                let out = match item {
                    Ok(frame) if secrets.exposes(&frame) => {
                        omitted.omit();
                        None
                    }
                    Ok(frame) => Some(Ok(frame)),
                    Err(error) => Some(Err(secrets.guarded(error))),
                };
                std::future::ready(out)
            })
            .boxed()
    }

    fn guard_response(&self, result: Result<ExecResponse, ExecError>) -> Result<ExecResponse, ExecError> {
        if self.is_empty() {
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
            ResponseBody::Stream(stream) => {
                response.omitted = Omissions::tracked();
                ResponseBody::Stream(self.guard_stream(stream, &response.omitted))
            }
            ResponseBody::Buffered(body) if self.exposes(&body) => return Err(withheld_response(self)),
            body @ ResponseBody::Buffered(_) => body,
        };
        Ok(response)
    }

    fn deliverable(&self, frame: Bytes) -> Option<Bytes> {
        (!self.exposes(&frame)).then_some(frame)
    }
}

/// The text a native WebSocket connection ends with when a frame its client's reader
/// would end a response on carried an upstream authentication value.
pub(crate) const WITHHELD_FRAME: &str = "codex upstream frame withheld: it carried the upstream credential";

/// `error` as [`Delivery::Confidential`] under `sent`, the values it was checked against:
/// its classification stays.
pub(crate) fn confidential(mut error: ExecError, sent: &SentCredentials) -> ExecError {
    error.delivery = Delivery::Confidential(sent.clone());
    error.direct = false;
    error.delivered_status = None;
    error.addon = None;
    error
}

/// A buffered answer that carried an upstream authentication value: request-scoped, so
/// the attempt neither fails over (the upstream served it) nor cools the credential.
pub(crate) fn withheld_response(sent: &SentCredentials) -> ExecError {
    confidential(ExecError::local(502, FailureScope::Request, WITHHELD_RESPONSE), sent)
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

#[cfg(test)]
#[path = "codex_secret_tests.rs"]
mod tests;
