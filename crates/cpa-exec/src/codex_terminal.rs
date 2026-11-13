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
//! - In-stream ([`event_rejection_status`]): an `error` or `response.failed` event, which
//!   has no outer status. `parse_terminal_provider_error` reads the event's `error.code` with no
//!   status (`sse/responses.rs:379-383`, `sse/responses_error.rs:32`). Over a WebSocket
//!   this is also the reading of an `error` frame the wrapped mapping does not turn into
//!   an HTTP error: no status, a status that maps to no error (a 2xx, or one past
//!   `http::StatusCode`'s range), or a status the wrapped struct cannot read
//!   (`responses_websocket.rs:747-806`, then `process_responses_event`).
//! - With a status ([`reading`]): an HTTP error, or a wrapped WebSocket frame whose status
//!   maps to an HTTP error. The same parser reads the body's `error.code` with that status, and
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

use crate::rawjson::{self, Kind};

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

/// The HTTP status an in-stream rejection, which has none, is classified with: the
/// status first-party Codex pairs the code with on HTTP. 403 for the account workspace's
/// policy (`inference_hook_denied`, `access_program_not_enabled`), so the guard that keeps
/// a frame that can name the pooled account from a native WebSocket client
/// (`codex_ws::account_failure`, #6) still applies; 400 for the others. `None` for every
/// code outside [`STREAM_TERMINAL`].
pub(crate) fn stream_status(code: &str) -> Option<u16> {
    match code {
        "inference_hook_denied" | "access_program_not_enabled" => Some(403),
        "content_policy_violation" | "invalid_access_program" | "unsupported_access_program" => Some(400),
        _ => None,
    }
}

/// The string at `path` in the JSON text `json`, or `None` when it is absent or no string.
/// `json` is a pinned view (`crate::codex_event`), so each key occurs once.
fn string_at(json: &str, path: &str) -> Option<String> {
    let value = rawjson::get(json, path);
    (value.kind() == Kind::String).then(|| value.str().to_owned())
}

/// The status an in-stream failure body `body` (`{"error":…}`, built by
/// `codex_response::terminal_failure_body`) is classified with when its `error.code` is a
/// first-party stream rejection code, or `None`. A number of any size in it is fine.
pub(crate) fn stream_rejection_status(body: &str) -> Option<u16> {
    string_at(body, "error.code").as_deref().and_then(stream_status)
}

/// The status an in-stream failure event is classified with when first-party Codex reads
/// it as a terminal rejection, or `None`. `event` is its logical value
/// (`crate::codex_event::logical_view`: the event's own keys plain, as its struct reads
/// them, member values as the first-party `Value` reads them). Only first-party's
/// locations count: an `error` event's `error` (`sse/responses.rs:379-383`) and a
/// `response.failed`'s `response.error` (`sse/responses_error.rs:26-32`), each an object
/// with a string `code`; Go's fallback locations do not.
pub(crate) fn event_rejection_status(event: &str) -> Option<u16> {
    if !rawjson::valid(crate::codex_event::json_trim(event)) {
        return None;
    }
    let error = match string_at(event, "type")?.as_str() {
        "error" => "error",
        "response.failed" => "response.error",
        _ => return None,
    };
    if rawjson::get(event, error).kind() != Kind::Object {
        return None;
    }
    string_at(event, &format!("{error}.code"))
        .as_deref()
        .and_then(stream_status)
}

/// The first-party reading of a failure with HTTP status `status`: an HTTP error, a
/// wrapped WebSocket frame mapped to one, or a refused upgrade. `body` is the HTTP body or
/// the whole frame as received, `headers` the response's or the decoded frame's headers.
/// `None` leaves the failure to the proxy's Go rules.
///
/// The body is read as first-party reads it, `serde_json::from_str::<Value>`
/// (`api_bridge.rs:138-140`, `293-298`), through `crate::codex_event::value_view`: the
/// last of a repeated key, serde_json's private number and raw value tokens resolved, a
/// number of any size accepted (`arbitrary_precision`). Codes are compared exactly. The
/// body is an error body or one frame, bounded, and read in linear passes.
pub(crate) fn reading(status: u16, body: &str, headers: &HeaderMap) -> Option<SourcedFault> {
    let value = crate::codex_event::value_view(body);
    let at = |path: &str| value.as_deref().and_then(|v| string_at(v, path));
    let code = at("error.code").unwrap_or_default();
    // `parse_terminal_provider_error` with the status (`api_bridge.rs:138-143`).
    if matches!(
        (code.as_str(), status),
        ("inference_hook_denied", 403) | ("content_policy_violation", 400)
    ) {
        return Some(SourcedFault::RequestTerminal);
    }
    let detail = at("detail.code");
    // The 403 match (`api_bridge.rs:293-311`), which runs after that parser declined the
    // body, whatever else it names: the body's `/detail/code`, else its `/error/code`,
    // else the `x-error-json` header's code.
    if status == 403 {
        let matched = detail
            .clone()
            .or_else(|| at("error.code"))
            .or_else(|| identity_code(headers));
        if matched.is_some_and(|c| FORBIDDEN_TERMINAL.contains(&c.as_str())) {
            return Some(SourcedFault::RequestTerminal);
        }
    }
    // `error.rs:110-112`: a context-window failure only with 400; any other status retries.
    if code == "context_length_exceeded" {
        return (status != 400).then_some(SourcedFault::Retryable);
    }
    let named = [code.as_str(), detail.as_deref().unwrap_or_default()];
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
        && crate::codex_event::value_view(body)
            .and_then(|v| string_at(&v, "error.code"))
            .is_some_and(|c| c == "context_length_exceeded")
}

/// The headers of a wrapped WebSocket frame `frame`, as received, as first-party Codex
/// reads them (`responses_websocket.rs:735-745`, `responses_headers.rs:10-31` at
/// 5c3ff1dabf64): the wrapped struct's `headers` member decoded as a
/// `Map<String, Value>`, then inserted into a `HeaderMap` in the map's order.
///
/// - The map's own keys are plain: a private-token key is an ordinary name, which
///   `http` refuses (sixth review finding 2).
/// - The map keeps insertion order (`preserve_order`): a repeated key keeps its first
///   position and takes its last value, so of two names `http` folds together, the one
///   inserted later wins (sixth review finding 1).
/// - Each value is read as the first-party `Value` reads it (a private token resolved,
///   a number's digits as written): a string, number or boolean, unchanged, else skipped.
pub(crate) fn frame_headers(frame: &str) -> HeaderMap {
    let mut mapped = HeaderMap::new();
    let Some(raw) = crate::codex_event::wrapped_headers(frame) else {
        return mapped;
    };
    let headers = rawjson::parse(raw);
    if headers.kind() != Kind::Object {
        return mapped;
    }
    // Insertion order with one lookup per member: linear in the frame (AGENTS.md).
    let mut members: Vec<(String, String)> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    headers.each(|name, value| {
        let (name, value) = (name.str().to_owned(), value.json().to_owned());
        match index.get(&name) {
            Some(&at) => members[at].1 = value,
            None => {
                index.insert(name.clone(), members.len());
                members.push((name, value));
            }
        }
        true
    });
    for (name, value) in members {
        let Some(value) = crate::codex_event::value_view(&value) else {
            continue;
        };
        let value = rawjson::parse(&value);
        let text = match value.kind() {
            Kind::String => value.str().to_owned(),
            Kind::Number | Kind::True | Kind::False => value.json().to_owned(),
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
    let decoded = std::str::from_utf8(&decoded).ok()?;
    string_at(&crate::codex_event::value_view(decoded)?, "error.code")
}

#[cfg(test)]
mod tests {
    use super::*;

    const TERMINAL: Option<SourcedFault> = Some(SourcedFault::RequestTerminal);
    const RETRYABLE: Option<SourcedFault> = Some(SourcedFault::Retryable);

    /// The in-stream reading of a failure body, as `codex_response::terminal_failure`
    /// takes it.
    fn stream(body: &str, _: &HeaderMap) -> Option<SourcedFault> {
        stream_rejection_status(body).map(|_| SourcedFault::RequestTerminal)
    }

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
                assert_eq!(stream(&error, &none()), TERMINAL, "{error}");
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
            assert_eq!(stream(&error, &none()), None, "{code}");
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
            assert_eq!(reading(terminal, &body, &none()), TERMINAL, "{code}");
            for other in [400, 401, 403, 413, 422, 429, 500, 503] {
                if other != terminal {
                    let want = if other == 400 { TERMINAL } else { RETRYABLE };
                    assert_eq!(reading(other, &body, &none()), want, "{code} {other}");
                }
            }
        }
        for code in ["invalid_access_program", "unsupported_access_program"] {
            for body in [
                format!(r#"{{"error":{{"code":"{code}"}}}}"#),
                format!(r#"{{"error":{{"code":"{code}","type":"invalid_request_error"}}}}"#),
            ] {
                assert_eq!(reading(400, &body, &none()), TERMINAL, "{body}");
                for status in [403, 413, 503] {
                    assert_eq!(reading(status, &body, &none()), RETRYABLE, "{status} {body}");
                }
            }
        }
        // The review's case: a 503 content-policy body with Go's request-fault type.
        let typed = r#"{"error":{"code":"content_policy_violation","type":"invalid_request_error"}}"#;
        assert_eq!(reading(503, typed, &none()), RETRYABLE);
        // A code no first-party rule names keeps Go's rules on every status.
        let other = r#"{"error":{"code":"x","type":"invalid_request_error"}}"#;
        for status in [400, 403, 503] {
            assert_eq!(reading(status, other, &none()), None, "{status}");
        }
    }

    /// Review finding 5: the body and the `x-error-json` JSON are read as a
    /// `serde_json::Value`, which keeps the last of a repeated key, as first-party Codex
    /// does (`api_bridge.rs:138-140`, `342-354`).
    #[test]
    fn a_repeated_key_reads_as_its_last_occurrence() {
        let last = r#"{"error":{"code":"other","code":"inference_hook_denied"}}"#;
        let first = r#"{"error":{"code":"inference_hook_denied","code":"other"}}"#;
        assert_eq!(reading(403, last, &none()), TERMINAL);
        assert_eq!(reading(403, first, &none()), None);
        // An in-stream body is read as given; the routes build it from the pinned view,
        // where each key occurs once.
        assert_eq!(stream(&crate::codex_event::pinned_view(last), &none()), TERMINAL);
        assert_eq!(stream(&crate::codex_event::pinned_view(first), &none()), None);
        let detail = r#"{"detail":{"code":"route_rate_limited","code":"invalid_workspace_selected"}}"#;
        assert_eq!(reading(403, detail, &none()), TERMINAL);
        let header = x_error_json(r#"{"error":{"code":"x","code":"invalid_workspace_selected"}}"#);
        assert_eq!(reading(403, "", &header), TERMINAL);
        let header = x_error_json(r#"{"error":{"code":"invalid_workspace_selected","code":"x"}}"#);
        assert_eq!(reading(403, "", &header), None);
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
            assert_eq!(reading(403, body, &none()), TERMINAL, "{body}");
            assert_eq!(reading(503, body, &none()), RETRYABLE, "{body}");
        }
        // A context code alone on a 403 stays retryable; with x-error-json naming a
        // workspace it is that denial (the body names no `/detail/code`).
        let context = r#"{"error":{"code":"context_length_exceeded"}}"#;
        assert_eq!(reading(403, context, &none()), RETRYABLE);
        let header = x_error_json(r#"{"error":{"code":"invalid_workspace_selected"}}"#);
        assert_eq!(
            reading(403, context, &header),
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
                reading(status, body, &headers),
                want,
                "{status} {body} header={with_header}"
            );
        }
        for body in [
            r#"{"error":{"code":"access_program_not_enabled","message":"m"}}"#,
            r#"{"detail":{"code":"access_program_not_enabled"}}"#,
        ] {
            assert_eq!(reading(403, body, &none()), TERMINAL, "{body}");
            assert_eq!(reading(400, body, &none()), TERMINAL, "{body}");
        }
        let header = x_error_json(r#"{"error":{"code":"access_program_not_enabled"}}"#);
        assert_eq!(reading(403, "", &header), TERMINAL);
        // An unreadable header is no code.
        let mut bad = HeaderMap::new();
        bad.insert("x-error-json", "not base64!".parse().unwrap());
        assert_eq!(reading(403, "", &bad), None);
        // First-party runtime authentication denials stay credential failures
        // (`protocol/src/error.rs:703-732`).
        for code in ["runtime_assertion_invalid", "runtime_instance_revoked"] {
            let body = format!(r#"{{"error":{{"code":"{code}"}}}}"#);
            assert_eq!(reading(403, &body, &none()), None, "{code}");
            assert_eq!(reading(401, &body, &none()), None, "{code}");
        }
    }

    /// Fifth review findings 1 and 3: a body or event is read as the first-party build
    /// reads it, with `arbitrary_precision` (a number of any size) and `raw_value` (a
    /// private raw value resolved), and an event's own keys plain, as its struct reads them.
    #[test]
    fn first_party_value_semantics_hold_for_large_numbers_and_private_tokens() {
        let big =
            r#"{"error":{"code":"inference_hook_denied","message":"m","n":1e400,"m":123456789012345678901234567890}}"#;
        assert_eq!(reading(403, big, &none()), TERMINAL);
        assert_eq!(stream(big, &none()), TERMINAL);
        let failed = r#"{"type":"response.failed","response":{"error":{"code":"inference_hook_denied","n":1e400}}}"#;
        assert_eq!(
            event_rejection_status(&crate::codex_event::logical_view(failed)),
            Some(403)
        );
        let raw = r#"{"$serde_json::private::RawValue":"{\"error\":{\"code\":\"inference_hook_denied\"}}"}"#;
        assert_eq!(
            reading(403, raw, &none()),
            TERMINAL,
            "the root raw value is its JSON text"
        );
        let leading = r#"{"$serde_json::private::RawValue":"0","type":"response.failed","response":{"error":{"code":"content_policy_violation"}}}"#;
        assert_eq!(
            event_rejection_status(&crate::codex_event::logical_view(leading)),
            Some(400),
            "an event's leading private key is an unknown member"
        );
        assert_eq!(reading(400, leading, &none()), None, "a Value refuses that root");
        let big_header = format!(
            r#"{{"type":"error","status":403,"headers":{{"x-error-json":"{}","x-n":1e400}}}}"#,
            base64::engine::general_purpose::STANDARD.encode(r#"{"error":{"code":"invalid_workspace_selected"}}"#)
        );
        let headers = frame_headers(&big_header);
        assert_eq!(headers["x-n"], "1e400", "a number's digits as written");
        assert_eq!(reading(403, &big_header, &headers), TERMINAL);
        assert_eq!(event_rejection_status("not json"), None);
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
        assert_eq!(reading(403, &frame(&encoded), &headers), TERMINAL);
        let spaced = frame(&format!("{encoded} "));
        assert_eq!(frame_headers(&spaced)["x-error-json"], format!("{encoded} ").as_str());
        assert_eq!(reading(403, &spaced, &frame_headers(&spaced)), None);
        assert!(frame_headers("not json").is_empty());
    }

    /// Sixth review findings 1 and 2: wrapped headers are the struct's `headers` member as
    /// a `Map<String, Value>`: keys plain, a repeated key in its first position with its
    /// last value, so the later of two names `http` folds together wins.
    #[test]
    fn wrapped_headers_follow_the_first_party_map() {
        let b64 = |json: &str| base64::engine::general_purpose::STANDARD.encode(json);
        let t = b64(r#"{"error":{"code":"invalid_workspace_selected"}}"#);
        let r = b64(r#"{"error":{"code":"route_rate_limited"}}"#);
        let frame = |headers: &str| format!(r#"{{"type":"error","status":403,"headers":{headers}}}"#);
        let repeated = frame(&format!(
            r#"{{"x-error-json":"{r}","X-Error-Json":"{t}","x-error-json":"{r}"}}"#
        ));
        let headers = frame_headers(&repeated);
        assert_eq!(
            headers["x-error-json"],
            t.as_str(),
            "the uppercase name is inserted last"
        );
        assert_eq!(reading(403, &repeated, &headers), TERMINAL);
        let reversed = frame(&format!(r#"{{"X-Error-Json":"{t}","x-error-json":"{r}"}}"#));
        assert_eq!(reading(403, &reversed, &frame_headers(&reversed)), None);
        // A private token as the map's own key is a plain name `http` refuses.
        let token = frame(&format!(
            r#"{{"$serde_json::private::RawValue":"{{\"x-error-json\":\"{t}\"}}"}}"#
        ));
        let headers = frame_headers(&token);
        assert!(headers.is_empty(), "{headers:?}");
        assert_eq!(reading(403, &token, &headers), None);
        // A private token as a value is what the first-party `Value` reads it as.
        let valued = frame(&format!(
            r#"{{"x-error-json":{{"$serde_json::private::RawValue":"\"{t}\""}}}}"#
        ));
        assert_eq!(frame_headers(&valued)["x-error-json"], t.as_str());
        // A positional frame's headers are its fourth member.
        let positional = format!(r#"["error",403,null,{{"x-error-json":"{t}"}}]"#);
        assert_eq!(frame_headers(&positional)["x-error-json"], t.as_str());
    }

    /// Seventh review finding 1: header de-duplication is one lookup per member. Many
    /// distinct headers, then a repeated one, keep the first position with the last value.
    #[test]
    fn many_wrapped_headers_stay_linear() {
        let b64 = |json: &str| base64::engine::general_purpose::STANDARD.encode(json);
        let t = b64(r#"{"error":{"code":"invalid_workspace_selected"}}"#);
        let mut members: Vec<String> = (0..50_000).map(|i| format!(r#""h{i}":null"#)).collect();
        members.insert(0, r#""x-error-json":"first""#.to_owned());
        members.push(format!(r#""x-error-json":"{t}""#));
        let frame = format!(r#"{{"type":"error","status":403,"headers":{{{}}}}}"#, members.join(","));
        let headers = frame_headers(&frame);
        assert_eq!(headers.len(), 1, "null values are skipped");
        assert_eq!(headers["x-error-json"], t.as_str());
        assert_eq!(reading(403, &frame, &headers), TERMINAL);
    }

    /// Sixth review finding 3: only JSON whitespace surrounds a body the first-party
    /// parser accepts; a body led by U+00A0 is no JSON there.
    #[test]
    fn only_json_whitespace_is_trimmed() {
        let body = r#"{"error":{"code":"inference_hook_denied"}}"#;
        assert_eq!(reading(403, &format!("\u{a0}{body}"), &none()), None);
        assert_eq!(reading(403, &format!("{body}\u{2003}"), &none()), None);
        assert_eq!(reading(403, &format!(" \t\r\n{body}\n"), &none()), TERMINAL);
        let event = r#"{"type":"error","error":{"code":"inference_hook_denied"}}"#;
        assert_eq!(event_rejection_status(&format!("\u{a0}{event}")), None);
        assert_eq!(event_rejection_status(&format!("\n{event} ")), Some(403));
    }

    /// `error.rs:110-112` and `responses_websocket.rs:1416-1439`: with a status,
    /// `context_length_exceeded` is a context failure only with 400; with any other
    /// status the client retries it.
    #[test]
    fn a_context_code_is_a_context_failure_only_with_400() {
        let body = r#"{"error":{"code":"context_length_exceeded","type":"invalid_request_error"}}"#;
        assert_eq!(reading(400, body, &none()), None);
        for status in [401, 403, 404, 413, 500, 502, 503, 504] {
            assert_eq!(reading(status, body, &none()), RETRYABLE, "{status}");
        }
        let wrapped = r#"{"type":"error","status":503,"error":{"code":"context_length_exceeded"}}"#;
        assert_eq!(reading(503, wrapped, &none()), RETRYABLE);
        assert_eq!(stream(wrapped, &none()), None);
    }
}
