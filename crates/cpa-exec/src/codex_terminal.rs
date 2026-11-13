//! First-party Codex terminal request rejections (FP2, #116).
//!
//! The source is openai/codex research main `5c3ff1dabf644169f4bd74b215719f381e4fdb3f`:
//! `codex-api/src/error.rs:92-162` (`parse_terminal_provider_error`),
//! `codex-api/src/api_bridge.rs:130-312` (HTTP and wrapped WebSocket statuses),
//! `codex-api/src/endpoint/responses_websocket.rs:906-925` (the WebSocket read order) and
//! `protocol/src/error.rs:470-534` (`retry_policy`). The contract pin `44ccf394573c` does
//! not have these rules. They are adopted as source-refresh evidence
//! (docs/codex/FP1-SOURCE-REFRESH.md, docs/codex/FP2-TERMINAL-ERRORS.md); the pin does
//! not move.
//!
//! First-party Codex reads a failure in one of two ways, and the proxy keeps them apart:
//!
//! - [`Surface::Stream`]: an `error` or `response.failed` event, which has no outer
//!   status. `parse_terminal_provider_error` reads the event's `error.code` with no
//!   status (`sse/responses.rs:379-383`, `sse/responses_error.rs:32`). Over a WebSocket
//!   this is also the reading of an `error` frame the wrapped mapping does not turn into
//!   an HTTP error: no status, a status that maps to no error (a 2xx, or one past
//!   `http::StatusCode`'s range), or a status the wrapped struct cannot read
//!   (`responses_websocket.rs:747-806`, then `process_responses_event`).
//! - [`Surface::Status`]: an HTTP error, or a wrapped WebSocket frame whose status maps
//!   to an HTTP error. The same parser reads the body's `error.code` with that status, and
//!   a 403 is then matched on its `/detail/code`, `/error/code` or `x-error-json` header
//!   code (`api_bridge.rs:138-143`, `283-312`, `342-354`).
//!
//! Every rejection here is `RetryPolicy::Never` in the first-party client. In a pooled
//! proxy the request stops on the selected account: no failover, no cooldown and no
//! token refresh ([`SourcedFault::RequestTerminal`]). That is the first-party client's
//! own decision, carried to the scheduler on the error; the rejection is not evidence
//! that the account is unhealthy, so no other account is tried and none is penalized.
//! `context_length_exceeded` with an HTTP status other than 400 is not a context-window
//! failure (`error.rs:110-112`): the client retries it ([`SourcedFault::Retryable`]).
//!
//! Only the Codex executor reads these rules. The server's provider-neutral request-fault
//! rules stay Go's for every other provider; they defer to the reading only through
//! `ExecError::sourced`.

use base64::Engine;
use cpa_core::exec::SourcedFault;
use http::HeaderMap;
use serde_json::Value;

/// Which first-party reading a failure gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Surface {
    /// An in-stream event or an unmapped WebSocket `error` frame: no outer status.
    Stream,
    /// An HTTP error, or a wrapped WebSocket frame mapped to one, with its status.
    Status(u16),
}

/// The codes `parse_terminal_provider_error` raises without a status that the contract
/// pin's parser did not have (`error.rs:103-134` at 5c3ff1dabf64).
/// `context_length_exceeded` is not here: without a status it already is the proxy's
/// context-window failure (a 400 request fault).
pub(crate) const STREAM_TERMINAL: [&str; 5] = [
    "inference_hook_denied",
    "invalid_access_program",
    "unsupported_access_program",
    "content_policy_violation",
    "access_program_not_enabled",
];

/// The 403 codes `api_bridge.rs:293-311` matches on the body or `x-error-json` code: a
/// workspace access denial and an access-program denial.
const FORBIDDEN_TERMINAL: [&str; 3] = [
    "invalid_workspace_selected",
    "codex_workspace_access_denied",
    "access_program_not_enabled",
];

/// The HTTP status an in-stream rejection, which has none, is classified with: the
/// status first-party Codex pairs the code with on HTTP. 403 for the account workspace's
/// policy (`inference_hook_denied`, `access_program_not_enabled`), so the guard that keeps
/// a frame that can name the pooled account from a native WebSocket client
/// (`codex_ws::account_failure`, #6) still applies; 400 for the others.
pub(crate) fn stream_status(code: &str) -> Option<u16> {
    match code {
        "inference_hook_denied" | "access_program_not_enabled" => Some(403),
        "content_policy_violation" | "invalid_access_program" | "unsupported_access_program" => Some(400),
        _ => None,
    }
}

/// The status an in-stream failure with error body `body` is classified with when it is
/// a first-party terminal rejection ([`stream_status`] of its code, read as
/// [`reading`] reads it), or `None`.
pub(crate) fn stream_rejection_status(body: &str) -> Option<u16> {
    let value = serde_json::from_str::<Value>(body).ok()?;
    let code = value.pointer("/error/code")?.as_str()?;
    STREAM_TERMINAL.contains(&code).then(|| stream_status(code)).flatten()
}

/// The codes FP2 reads from first-party sources: the stream codes and the two workspace
/// codes. With an HTTP status that pairs with none of their terminal readings, and that is
/// not 400, first-party Codex retries them (`UnexpectedStatus`, `RetryLimit` or
/// `InternalServerError`, `api_bridge.rs:223-312`, `protocol/src/error.rs:470-534`), so
/// Go's request-fault rules (an `invalid_request_error` type, a 413 or 422) must not stop
/// them. On 400 first-party reads them as the terminal `InvalidRequest`
/// (`api_bridge.rs:188-222`), before anything the message says.
const FIRST_PARTY_CODES: [&str; 7] = [
    "inference_hook_denied",
    "invalid_access_program",
    "unsupported_access_program",
    "content_policy_violation",
    "access_program_not_enabled",
    "invalid_workspace_selected",
    "codex_workspace_access_denied",
];

/// The first-party reading of a failure on `surface`, with the JSON `body` (the event's
/// error object as `{"error":…}` for [`Surface::Stream`]; the HTTP body or the whole
/// wrapped frame for [`Surface::Status`]) and the `headers` it came with. `None` leaves
/// the failure to the proxy's Go rules.
///
/// The body is read as first-party Codex reads it, a `serde_json::Value`
/// (`api_bridge.rs:138-140`, `293-298`): the last of a repeated key, and serde_json's
/// private number and raw value tokens resolved. Codes are compared exactly. The body is
/// an error body or one frame, bounded, and parsed once.
pub(crate) fn reading(surface: Surface, body: &str, headers: &HeaderMap) -> Option<SourcedFault> {
    let value = serde_json::from_str::<Value>(body).ok();
    let at = |path: &str| value.as_ref().and_then(|v| v.pointer(path)).and_then(Value::as_str);
    let code = at("/error/code").unwrap_or_default();
    let status = match surface {
        Surface::Stream => {
            return STREAM_TERMINAL.contains(&code).then_some(SourcedFault::RequestTerminal);
        }
        Surface::Status(status) => status,
    };
    // `parse_terminal_provider_error` with the status (`api_bridge.rs:138-143`).
    if matches!(
        (code, status),
        ("inference_hook_denied", 403) | ("content_policy_violation", 400)
    ) {
        return Some(SourcedFault::RequestTerminal);
    }
    // The 403 match (`api_bridge.rs:293-311`), which runs after that parser declined the
    // body, whatever else it names: the body's `/detail/code`, else its `/error/code`,
    // else the `x-error-json` header's code.
    if status == 403 {
        let matched = at("/detail/code")
            .or_else(|| at("/error/code"))
            .map(str::to_owned)
            .or_else(|| identity_code(headers));
        if matched.is_some_and(|c| FORBIDDEN_TERMINAL.contains(&c.as_str())) {
            return Some(SourcedFault::RequestTerminal);
        }
    }
    // `error.rs:110-112`: a context-window failure only with 400; any other status retries.
    if code == "context_length_exceeded" {
        return (status != 400).then_some(SourcedFault::Retryable);
    }
    let named = [code, at("/detail/code").unwrap_or_default()];
    named
        .iter()
        .any(|c| FIRST_PARTY_CODES.contains(c))
        .then_some(if status == 400 {
            SourcedFault::RequestTerminal
        } else {
            SourcedFault::Retryable
        })
}

/// Whether a failure with `status` and error `body` is a first-party context-window
/// failure: `context_length_exceeded` with 400, or in-stream, where the proxy classifies
/// it with 400 (`error.rs:110-112`). First-party Codex reads it from its code before
/// anything else, so Go's usage-limit and model-capacity readings of the message must not
/// turn it into a 429 or a bootstrap overload. The proxy's own context classification
/// (`context_too_large`, #5) still applies.
pub(crate) fn context_window(status: u16, body: &str) -> bool {
    status == 400
        && serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|v| {
                v.pointer("/error/code")
                    .and_then(Value::as_str)
                    .map(|c| c == "context_length_exceeded")
            })
            .unwrap_or(false)
}

/// The headers of a wrapped WebSocket frame `payload` as first-party Codex reads them
/// (`responses_headers.rs:10-31`): the `headers` object of its `serde_json::Value`, a
/// string, number or boolean value each, unchanged (not trimmed), and a name or value
/// `http` refuses skipped.
pub(crate) fn frame_headers(payload: &str) -> HeaderMap {
    let mut mapped = HeaderMap::new();
    let Ok(value) = serde_json::from_str::<Value>(payload) else {
        return mapped;
    };
    let Some(headers) = value.get("headers").and_then(Value::as_object) else {
        return mapped;
    };
    for (name, value) in headers {
        let text = match value {
            Value::String(v) => v.clone(),
            Value::Number(v) => v.to_string(),
            Value::Bool(v) => v.to_string(),
            _ => continue,
        };
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(&text),
        ) {
            mapped.insert(name, value);
        }
    }
    mapped
}

/// The `error.code` of the base64 JSON in `x-error-json` (`api_bridge.rs:342-354`), read
/// as a `serde_json::Value` too.
fn identity_code(headers: &HeaderMap) -> Option<String> {
    let encoded = headers.get("x-error-json")?.to_str().ok()?;
    let decoded = base64::engine::general_purpose::STANDARD.decode(encoded).ok()?;
    let parsed = serde_json::from_slice::<Value>(&decoded).ok()?;
    parsed.pointer("/error/code")?.as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TERMINAL: Option<SourcedFault> = Some(SourcedFault::RequestTerminal);
    const RETRYABLE: Option<SourcedFault> = Some(SourcedFault::Retryable);

    fn none() -> HeaderMap {
        HeaderMap::new()
    }

    fn x_error_json(json: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let value = base64::engine::general_purpose::STANDARD.encode(json);
        headers.insert("x-error-json", value.parse().unwrap());
        headers
    }

    /// `error.rs:103-134` with no status: each new code ends the stream, whatever status
    /// its error object names. Workspace codes have no stream reading
    /// (`parse_terminal_provider_error` does not name them); neither have the pinned
    /// codes nor the context code, which the proxy's own rules keep.
    #[test]
    fn stream_rejections_read_no_status() {
        for code in STREAM_TERMINAL {
            for error in [
                format!(r#"{{"error":{{"code":"{code}"}}}}"#),
                format!(r#"{{"error":{{"code":"{code}","headers":{{"retry-after":"300"}},"plan_type":42}}}}"#),
                format!(r#"{{"error":{{"code":"{code}","status":503}}}}"#),
            ] {
                assert_eq!(reading(Surface::Stream, &error, &none()), TERMINAL, "{error}");
            }
            assert!(stream_status(code).is_some(), "{code}");
        }
        for code in [
            "invalid_workspace_selected",
            "codex_workspace_access_denied",
            "context_length_exceeded",
            "budget_reached",
            "server_is_overloaded",
            "Inference_Hook_Denied",
            " inference_hook_denied",
        ] {
            let error = format!(r#"{{"error":{{"code":"{code}"}}}}"#);
            assert_eq!(reading(Surface::Stream, &error, &none()), None, "{code}");
            assert_eq!(stream_status(code), None, "{code}");
        }
    }

    /// `error.rs:103-134` and `api_bridge.rs:138-143` with a status: a code is terminal
    /// only with the status first-party Codex pairs it with. `invalid_access_program` and
    /// `unsupported_access_program` have no status reading: a 400 already is the ordinary
    /// invalid request (`error.rs:113`). With any other status but 400 first-party retries
    /// the code (`api_bridge.rs:223-312`), so it is `Retryable`, whatever type it names
    /// (review finding 2); on 400 it is the terminal `InvalidRequest` (re-review finding 2).
    #[test]
    fn status_rejections_follow_their_first_party_status() {
        for (code, terminal) in [("inference_hook_denied", 403), ("content_policy_violation", 400)] {
            let body = format!(r#"{{"error":{{"code":"{code}","message":"m"}}}}"#);
            assert_eq!(reading(Surface::Status(terminal), &body, &none()), TERMINAL, "{code}");
            for other in [400, 401, 403, 413, 422, 429, 500, 503] {
                if other != terminal {
                    let want = if other == 400 { TERMINAL } else { RETRYABLE };
                    assert_eq!(reading(Surface::Status(other), &body, &none()), want, "{code} {other}");
                }
            }
        }
        for code in ["invalid_access_program", "unsupported_access_program"] {
            for body in [
                format!(r#"{{"error":{{"code":"{code}"}}}}"#),
                format!(r#"{{"error":{{"code":"{code}","type":"invalid_request_error"}}}}"#),
            ] {
                assert_eq!(reading(Surface::Status(400), &body, &none()), TERMINAL, "{body}");
                for status in [403, 413, 503] {
                    assert_eq!(
                        reading(Surface::Status(status), &body, &none()),
                        RETRYABLE,
                        "{status} {body}"
                    );
                }
            }
        }
        // The review's case: a 503 content-policy body with Go's request-fault type.
        let typed = r#"{"error":{"code":"content_policy_violation","type":"invalid_request_error"}}"#;
        assert_eq!(reading(Surface::Status(503), typed, &none()), RETRYABLE);
        // A code no first-party rule names keeps Go's rules on every status.
        let other = r#"{"error":{"code":"x","type":"invalid_request_error"}}"#;
        for status in [400, 403, 503] {
            assert_eq!(reading(Surface::Status(status), other, &none()), None, "{status}");
        }
    }

    /// Review finding 5: the body and the `x-error-json` JSON are read as a
    /// `serde_json::Value`, which keeps the last of a repeated key, as first-party Codex
    /// does (`api_bridge.rs:138-140`, `342-354`).
    #[test]
    fn a_repeated_key_reads_as_its_last_occurrence() {
        let last = r#"{"error":{"code":"other","code":"inference_hook_denied"}}"#;
        let first = r#"{"error":{"code":"inference_hook_denied","code":"other"}}"#;
        assert_eq!(reading(Surface::Status(403), last, &none()), TERMINAL);
        assert_eq!(reading(Surface::Status(403), first, &none()), None);
        assert_eq!(reading(Surface::Stream, last, &none()), TERMINAL);
        assert_eq!(reading(Surface::Stream, first, &none()), None);
        let detail = r#"{"detail":{"code":"route_rate_limited","code":"invalid_workspace_selected"}}"#;
        assert_eq!(reading(Surface::Status(403), detail, &none()), TERMINAL);
        let header = x_error_json(r#"{"error":{"code":"x","code":"invalid_workspace_selected"}}"#);
        assert_eq!(reading(Surface::Status(403), "", &header), TERMINAL);
        let header = x_error_json(r#"{"error":{"code":"invalid_workspace_selected","code":"x"}}"#);
        assert_eq!(reading(Surface::Status(403), "", &header), None);
    }

    /// Review finding 3: the 403 match runs after the status parser declined the body,
    /// whatever else it names, so a context code does not make a workspace 403 retryable
    /// (`api_bridge.rs:138-143`, then `293-311`).
    #[test]
    fn the_403_match_precedes_the_context_rule() {
        for body in [
            r#"{"detail":{"code":"codex_workspace_access_denied"},"error":{"code":"context_length_exceeded"}}"#,
            r#"{"error":{"code":"context_length_exceeded"},"detail":{"code":"invalid_workspace_selected"}}"#,
        ] {
            assert_eq!(reading(Surface::Status(403), body, &none()), TERMINAL, "{body}");
            assert_eq!(reading(Surface::Status(503), body, &none()), RETRYABLE, "{body}");
        }
        // A context code alone on a 403 stays retryable; with x-error-json naming a
        // workspace it is that denial (the body names no `/detail/code`).
        let context = r#"{"error":{"code":"context_length_exceeded"}}"#;
        assert_eq!(reading(Surface::Status(403), context, &none()), RETRYABLE);
        let header = x_error_json(r#"{"error":{"code":"invalid_workspace_selected"}}"#);
        assert_eq!(
            reading(Surface::Status(403), context, &header),
            RETRYABLE,
            "a body code wins over the header"
        );
    }

    /// `api_bridge_tests.rs` `only_an_explicit_workspace_403_stops_retries` at
    /// 5c3ff1dabf64, case for case: a workspace code in `/detail/code`, `/error/code` or
    /// the `x-error-json` header is terminal on a 403 only. The access-program denial
    /// reads the same way (`api_bridge.rs:305-311`).
    #[test]
    fn forbidden_codes_follow_the_first_party_403_match() {
        let provider = r#"{"error":{"code":"invalid_workspace_selected"}}"#;
        let detail = r#"{"detail":{"code":"invalid_workspace_selected"}}"#;
        let access_denied = r#"{"detail":{"code":"codex_workspace_access_denied"}}"#;
        let other = r#"{"detail":{"code":"route_rate_limited"}}"#;
        // First-party's `terminal` column, with the reading of a non-terminal case: a
        // workspace code on another status is retried there (`Retryable` here).
        for (status, body, with_header, want) in [
            (403, detail, false, TERMINAL),
            (403, access_denied, false, TERMINAL),
            (503, access_denied, false, RETRYABLE),
            (403, provider, false, TERMINAL),
            (403, "", true, TERMINAL),
            (403, "", false, None),
            (403, "invalid_workspace_selected", false, None),
            (403, other, true, None),
            (401, detail, true, RETRYABLE),
        ] {
            let headers = if with_header { x_error_json(provider) } else { none() };
            assert_eq!(
                reading(Surface::Status(status), body, &headers),
                want,
                "{status} {body} header={with_header}"
            );
        }
        for body in [
            r#"{"error":{"code":"access_program_not_enabled","message":"m"}}"#,
            r#"{"detail":{"code":"access_program_not_enabled"}}"#,
        ] {
            assert_eq!(reading(Surface::Status(403), body, &none()), TERMINAL, "{body}");
            assert_eq!(reading(Surface::Status(400), body, &none()), TERMINAL, "{body}");
        }
        let header = x_error_json(r#"{"error":{"code":"access_program_not_enabled"}}"#);
        assert_eq!(reading(Surface::Status(403), "", &header), TERMINAL);
        // An unreadable header is no code.
        let mut bad = HeaderMap::new();
        bad.insert("x-error-json", "not base64!".parse().unwrap());
        assert_eq!(reading(Surface::Status(403), "", &bad), None);
        // First-party runtime authentication denials stay credential failures
        // (`protocol/src/error.rs:703-732`).
        for code in ["runtime_assertion_invalid", "runtime_instance_revoked"] {
            let body = format!(r#"{{"error":{{"code":"{code}"}}}}"#);
            assert_eq!(reading(Surface::Status(403), &body, &none()), None, "{code}");
            assert_eq!(reading(Surface::Status(401), &body, &none()), None, "{code}");
        }
    }

    /// Re-review finding 2: a context code is a context-window failure with 400 or
    /// in-stream (read with 400), and with no other status.
    #[test]
    fn context_window_names_only_400() {
        let body = r#"{"error":{"code":"context_length_exceeded","message":"model is at capacity"}}"#;
        assert!(context_window(400, body));
        for status in [403, 429, 503] {
            assert!(!context_window(status, body), "{status}");
        }
        assert!(!context_window(400, r#"{"error":{"code":"other"}}"#));
        assert!(
            context_window(400, r#"{"error":{"code":"x","code":"context_length_exceeded"}}"#),
            "the last key"
        );
    }

    /// Re-review finding 4: a wrapped frame's headers are read as first-party reads them,
    /// unchanged; a value with a trailing space is not the base64 it would be trimmed.
    #[test]
    fn frame_headers_keep_their_values_as_sent() {
        let json = r#"{"error":{"code":"invalid_workspace_selected"}}"#;
        let encoded = base64::engine::general_purpose::STANDARD.encode(json);
        let frame = |v: &str| {
            format!(
                r#"{{"type":"error","status":403,"headers":{{"x-error-json":"{v}","x-n":7,"x-b":true,"x-o":{{}}}}}}"#
            )
        };
        let headers = frame_headers(&frame(&encoded));
        assert_eq!(headers["x-n"], "7");
        assert_eq!(headers["x-b"], "true");
        assert!(headers.get("x-o").is_none());
        assert_eq!(reading(Surface::Status(403), &frame(&encoded), &headers), TERMINAL);
        let spaced = frame(&format!("{encoded} "));
        assert_eq!(frame_headers(&spaced)["x-error-json"], format!("{encoded} ").as_str());
        assert_eq!(reading(Surface::Status(403), &spaced, &frame_headers(&spaced)), None);
        assert!(frame_headers("not json").is_empty());
    }

    /// `error.rs:110-112` and `responses_websocket.rs:1416-1439`: with a status,
    /// `context_length_exceeded` is a context failure only with 400; with any other
    /// status the client retries it.
    #[test]
    fn a_context_code_is_a_context_failure_only_with_400() {
        let body = r#"{"error":{"code":"context_length_exceeded","type":"invalid_request_error"}}"#;
        assert_eq!(reading(Surface::Status(400), body, &none()), None);
        for status in [401, 403, 404, 413, 500, 502, 503, 504] {
            assert_eq!(reading(Surface::Status(status), body, &none()), RETRYABLE, "{status}");
        }
        let wrapped = r#"{"type":"error","status":503,"error":{"code":"context_length_exceeded"}}"#;
        assert_eq!(reading(Surface::Status(503), wrapped, &none()), RETRYABLE);
        assert_eq!(reading(Surface::Stream, wrapped, &none()), None);
    }
}
