//! First-party Codex terminal request rejections (FP2, #116).
//!
//! The source is openai/codex research main `5c3ff1dabf644169f4bd74b215719f381e4fdb3f`:
//! `codex-api/src/error.rs:92-162` (`parse_terminal_provider_error`),
//! `codex-api/src/api_bridge.rs:130-312` (HTTP and wrapped WebSocket statuses) and
//! `protocol/src/error.rs:470-534` (`retry_policy`). The contract pin `44ccf394573c` does
//! not have these codes. They are adopted here as source-refresh evidence
//! (docs/codex/FP1-SOURCE-REFRESH.md); the pin does not move.
//!
//! The first-party client never retries these rejections. In a pooled proxy each one is
//! the caller's request, or the account workspace's policy for that request, never the
//! account's health: the request stops, the account is not cooled, nothing fails over to
//! another account, and no token is refreshed. A `context_length_exceeded` code is a
//! context-window failure only without a status or with 400. With a 5xx the status keeps
//! its transient meaning.
//!
//! The executor (`codex_response`) and the server's request-fault check
//! (`cpa_server::classify::is_request_fault`) both read these rules, so the scheduler
//! and the executor cannot disagree about one failure.
//!
//! A workspace denial (a 403 with `invalid_workspace_selected` or
//! `codex_workspace_access_denied`, `api_bridge.rs:300-304`) is not here. The proxy, not
//! the client, picks the workspace from the account's own credential, so the denial is
//! about that account. It stays a credential failure.

use crate::rawjson;

/// The error-code paths Go's `clienterror.IsRequestFault` reads, plus first-party
/// `/detail/code`, which `api_bridge.rs:293-311` reads only for a 403's own codes:
/// here `access_program_not_enabled` (`DETAIL`).
const CODE_PATHS: [&str; 5] = ["error.code", "code", "response.error.code", "body.error.code", DETAIL];

/// First-party `/detail/code`.
const DETAIL: &str = "detail.code";

/// The HTTP status first-party Codex pairs with a terminal request rejection `code`, the
/// status an in-stream event, which has no status, is classified with. 403 for the
/// account workspace's own policy (`inference_hook_denied`, `access_program_not_enabled`),
/// so the guard that keeps a frame naming the pooled account from a native client
/// (`codex_ws::account_failure`) still applies. 400 for the other codes.
///
/// `error.rs:103-134`: `inference_hook_denied` with no status or 403,
/// `content_policy_violation` with no status or 400, `invalid_access_program` and
/// `unsupported_access_program` with no status (a 400 already is an invalid request),
/// `access_program_not_enabled` with no status (`api_bridge.rs:305-311` with 403).
pub(crate) fn status_for(code: &str) -> Option<u16> {
    match code {
        "inference_hook_denied" | "access_program_not_enabled" => Some(403),
        "content_policy_violation" | "invalid_access_program" | "unsupported_access_program" => Some(400),
        _ => None,
    }
}

/// What the first-party rules decide about an upstream failure with `status` and error
/// `body`, before Go's request-fault rules run:
///
/// - `Some(true)`: a terminal request rejection (`status_for`) with the status
///   first-party Codex accepts it with.
/// - `Some(false)`: `context_length_exceeded` with a 5xx. First-party Codex reads a
///   context-window failure only without a status or with 400 (`error.rs:110-112`), and
///   it retries a 5xx with backoff (`protocol/src/error.rs` `retry_policy`). Go reads the
///   code as a request fault at any status, which would end the request, keep the account
///   from failing over, and present a server failure as an oversized input.
/// - `None`: Go's rules decide.
///
/// Codes are compared exactly, as first-party Codex compares them. Each path is read once.
pub fn request_fault(status: u16, body: &str) -> Option<bool> {
    let body = body.trim();
    if body.is_empty() || !rawjson::valid(body) {
        return None;
    }
    let mut context = false;
    for path in CODE_PATHS {
        let code = rawjson::get(body, path);
        let code = code.str();
        if status_for(code) == Some(status) && (path != DETAIL || code == "access_program_not_enabled") {
            return Some(true);
        }
        context |= code == "context_length_exceeded";
    }
    (context && status >= 500).then_some(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `error.rs:103-134` and `api_bridge.rs:305-311` at 5c3ff1dabf64: each code is
    /// terminal only with the status first-party Codex pairs it with.
    #[test]
    fn terminal_codes_follow_the_first_party_statuses() {
        for (code, terminal) in [
            ("inference_hook_denied", 403),
            ("access_program_not_enabled", 403),
            ("content_policy_violation", 400),
            ("invalid_access_program", 400),
            ("unsupported_access_program", 400),
        ] {
            assert_eq!(status_for(code), Some(terminal), "{code}");
            let detail = format!(r#"{{"detail":{{"code":"{code}"}}}}"#);
            // `/detail/code` names only the 403 access-program denial (`api_bridge.rs:293-311`).
            let by_detail = code == "access_program_not_enabled";
            assert_eq!(request_fault(terminal, &detail), by_detail.then_some(true), "{code}");
            for body in [
                format!(r#"{{"error":{{"code":"{code}","message":"m","param":null}}}}"#),
                format!(r#"{{"type":"error","status":{terminal},"error":{{"code":"{code}"}}}}"#),
                format!(r#"{{"status":{terminal},"body":{{"error":{{"code":"{code}"}}}}}}"#),
            ] {
                assert_eq!(request_fault(terminal, &body), Some(true), "{code} {body}");
                for other in [401, 404, 429, 500, 502, 503] {
                    assert_eq!(request_fault(other, &body), None, "{code} {other} {body}");
                }
            }
        }
        // Exact codes only, as the first-party match reads them.
        for body in [
            r#"{"error":{"code":"Inference_Hook_Denied"}}"#,
            r#"{"error":{"code":" inference_hook_denied"}}"#,
            r#"{"error":{"message":"inference_hook_denied"}}"#,
            "inference_hook_denied",
            "",
        ] {
            assert_eq!(request_fault(403, body), None, "{body}");
        }
    }

    /// `error.rs:110-112` and `responses_websocket.rs:1416-1439`: a context-labelled 5xx
    /// keeps its transient meaning; Go's rules decide every other status.
    #[test]
    fn a_context_labelled_server_failure_is_not_a_request_fault() {
        for body in [
            r#"{"error":{"code":"context_length_exceeded"}}"#,
            r#"{"type":"error","status":503,"error":{"code":"context_length_exceeded","type":"invalid_request_error"}}"#,
            r#"{"response":{"error":{"code":"context_length_exceeded"}}}"#,
        ] {
            for status in [500, 502, 503, 504] {
                assert_eq!(request_fault(status, body), Some(false), "{status} {body}");
            }
            for status in [400, 403, 413] {
                assert_eq!(request_fault(status, body), None, "{status} {body}");
            }
        }
        assert_eq!(request_fault(503, r#"{"error":{"code":"server_is_overloaded"}}"#), None);
    }

    /// The workspace denial stays the account's failure: no first-party request rule
    /// reads it.
    #[test]
    fn a_workspace_denial_is_left_to_the_credential_rules() {
        for code in ["invalid_workspace_selected", "codex_workspace_access_denied"] {
            for body in [
                format!(r#"{{"error":{{"code":"{code}"}}}}"#),
                format!(r#"{{"detail":{{"code":"{code}"}}}}"#),
            ] {
                assert_eq!(status_for(code), None);
                assert_eq!(request_fault(403, &body), None, "{body}");
            }
        }
    }
}
