//! Error classification the Go conductor applies on top of an executor's error
//! (sdk/cliproxy/auth/conductor_cooldown.go, internal/clienterror/client_error.go).
//!
//! Executors report a [`FailureScope`]; Go instead classifies the upstream status and
//! the error text (`err.Error()`, which is the upstream body or `status N`). Both feed
//! retry, failover and cooldown decisions here so they cannot drift apart.

use cpa_core::exec::{ExecError, FailureScope};
use cpa_core::gostr::GoStr;
use serde_json::Value;

/// Go `err.Error()` of an executor status error: the body, or `status N` when empty.
pub fn error_text(e: &ExecError) -> String {
    if e.body.is_empty() {
        format!("status {}", e.status)
    } else {
        String::from_utf8_lossy(&e.body).into_owned()
    }
}

/// The text a cooldown keeps for `e`, whose classification text is `text`: `text`,
/// unless the failure carries an upstream authentication value
/// ([`cpa_core::exec::Delivery::Confidential`]); then only its status, since that text
/// is shown later, to other clients too ("last error").
pub fn stored_error_text(e: &ExecError, text: &str) -> String {
    if e.confidential() {
        format!("status {}", e.status)
    } else {
        text.to_owned()
    }
}

/// Go `extractErrorBody`, what request-scoped error rules match: the error's text,
/// except for the Claude executor's own missing-thread error, whose structured body
/// (`claudeThreadNotFoundError.ResponseBody`, v8.0.27 e7b2226) is Anthropic's JSON
/// while its message stays plain. That error is the local 404 with exactly the
/// executor's message; an upstream answer carries headers.
pub fn rule_text(e: &ExecError) -> String {
    let text = error_text(e);
    if e.status == 404 && explicit_request_scoped(e) && text == cpa_exec::claude::THREAD_NOT_FOUND {
        return format!(r#"{{"type":"error","error":{{"type":"not_found_error","message":"{text}"}}}}"#);
    }
    text
}

/// A local executor error (no upstream response) that the executor marked as a
/// request fault: Go's explicit `RequestScopedError`. Upstream responses always carry
/// headers, so an upstream 404 marked `Request` by a provider is still classified here
/// by status and body like Go does.
fn explicit_request_scoped(e: &ExecError) -> bool {
    e.scope == FailureScope::Request && e.headers.is_empty()
}

/// The Go status of an error: transport faults have none.
pub fn go_status(e: &ExecError) -> u16 {
    if e.scope == FailureScope::Transport {
        0
    } else {
        e.status
    }
}

/// Go's `empty_stream` error: a stream that ended before its first payload. Statusless,
/// so it cools the credential like an unclassified failure and renders as 500.
pub fn empty_stream() -> ExecError {
    ExecError::local(
        0,
        FailureScope::Credential,
        "empty_stream: upstream stream closed before first payload",
    )
}

/// Whether the failure came from an upstream attempt (Go `UpstreamAttempted`): an
/// upstream response, a connection fault, or a stream that ended empty.
pub fn upstream_attempted(e: &ExecError) -> bool {
    !e.headers.is_empty() || e.scope == FailureScope::Transport || e.status == 0
}

/// Go `isUnauthorizedError` for an attempt that was not an explicit request fault.
pub fn is_unauthorized(e: &ExecError) -> bool {
    !explicit_request_scoped(e) && names_unauthorized(e)
}

/// Go `isUnauthorizedError` itself: a 401 status or text, whatever the scope. Result
/// recording uses it with the cooldown policy, which a `*-and-cooldown` rule can force
/// on a request-scoped 401.
pub fn names_unauthorized(e: &ExecError) -> bool {
    let raw = error_text(e).go_lower();
    go_status(e) == 401 || raw.contains("status 401") || raw.contains("401 unauthorized")
}

/// Downstream status for a terminal error. Go answers status-less errors with 500.
pub fn response_status(e: &ExecError) -> u16 {
    match go_status(e) {
        0 => 500,
        s => s,
    }
}

const REQUEST_FAULT_CODES: [&str; 9] = [
    "cyber_policy",
    "context_length_exceeded",
    "message_too_big",
    "string_above_max_length",
    "invalid_prompt",
    "invalid_value",
    "unsupported_value",
    "invalid_request_error",
    "previous_response_not_found",
];
const REQUEST_FAULT_TYPES: [&str; 4] = [
    "invalid_request",
    "invalid_request_error",
    "bad_request_error",
    "invalid_prompt",
];

fn json_body(text: &str) -> Option<Value> {
    serde_json::from_str(text.trim()).ok()
}

fn path_str<'a>(v: &'a Value, path: &str) -> Option<&'a str> {
    path.split('.').try_fold(v, |v, k| v.get(k))?.as_str()
}

fn any_path(v: &Value, paths: &[&str], pred: impl Fn(&str) -> bool) -> bool {
    paths
        .iter()
        .filter_map(|p| path_str(v, p))
        .any(|s| pred(&s.trim().go_lower()))
}

const CODE_PATHS: [&str; 4] = ["error.code", "code", "response.error.code", "body.error.code"];
const TYPE_PATHS: [&str; 4] = ["error.type", "type", "response.error.type", "body.error.type"];

/// `clienterror.IsClaudeThreadNotFound` (v8.0.27, 5c959cb, e7b2226): Claude's 404 for
/// a stale `previous_message_id` continuation, as a JSON error or plain text. Only a
/// replay of the whole conversation helps, so no credential is cooled or rotated.
pub fn is_claude_thread_not_found(status: u16, text: &str) -> bool {
    let body = text.trim();
    if status != 404 || body.is_empty() {
        return false;
    }
    let names_thread = |message: &str| {
        let lower = message.go_lower();
        lower.contains("thread state") && lower.contains("previous_message_id")
    };
    match json_body(body) {
        Some(value) => {
            path_str(&value, "error.type").is_some_and(|t| t.trim().eq_ignore_ascii_case("not_found_error"))
                && names_thread(path_str(&value, "error.message").unwrap_or_default())
        }
        None => names_thread(body),
    }
}

/// `clienterror.IsRequestFault`.
pub fn is_request_fault(status: u16, text: &str) -> bool {
    if status == 402 || status == 429 {
        return false;
    }
    let body = json_body(text);
    if let Some(body) = &body
        && status == 401
        && any_path(body, &TYPE_PATHS, |t| t == "authentication_error")
    {
        return false;
    }
    if is_claude_thread_not_found(status, text) {
        return true;
    }
    if let Some(body) = &body {
        if any_path(body, &CODE_PATHS, |c| {
            c == "model_not_found" || c == "model_not_found_error"
        }) {
            return false;
        }
        if any_path(body, &CODE_PATHS, |c| REQUEST_FAULT_CODES.contains(&c))
            || any_path(body, &TYPE_PATHS, |t| REQUEST_FAULT_TYPES.contains(&t))
        {
            return true;
        }
    }
    let lower = text.go_lower();
    if lower.contains("item with id")
        && lower.contains("not found")
        && lower.contains("items are not persisted when `store` is set to false")
    {
        return true;
    }
    matches!(status, 400 | 409 | 413 | 422)
}

fn model_support_message(text: &str) -> bool {
    let lower = text.trim().go_lower();
    !lower.is_empty()
        && [
            "model_not_supported",
            "requested model is not supported",
            "requested model is unsupported",
            "requested model is unavailable",
            "model is not supported",
            "model not supported",
            "unsupported model",
            "model unavailable",
            "not available for your plan",
            "not available for your account",
        ]
        .iter()
        .any(|p| lower.contains(p))
}

/// `isModelSupportError`.
pub fn is_model_support(status: u16, text: &str) -> bool {
    explicit_model_not_found(text, "") || (matches!(status, 400 | 404 | 422) && model_support_message(text))
}

/// `isInvalidGrantError`.
pub fn is_invalid_grant(status: u16, text: &str) -> bool {
    text.go_lower().contains("invalid_grant") && matches!(status, 0 | 400 | 401)
}

/// `isCloudflareChallengeError`.
pub fn is_cloudflare(status: u16, text: &str) -> bool {
    let lower = text.trim().go_lower();
    status < 500
        && (lower.contains("challenge-platform")
            || lower.contains("cf-mitigated")
            || lower.contains("cloudflare challenge")
            || (lower.contains("just a moment") && lower.contains("cloudflare")))
}

/// `isRequestInvalidError`: never failed over, never cooled.
pub fn is_request_invalid(e: &ExecError) -> bool {
    if explicit_request_scoped(e) {
        return true;
    }
    let status = go_status(e);
    let text = error_text(e);
    if is_cloudflare(status, &text) || is_invalid_grant(status, &text) || is_model_support(status, &text) {
        return false;
    }
    is_request_fault(status, &text)
}

/// Faults that stop a `responses/compact` request without failover.
pub fn is_compact_fault(e: &ExecError) -> bool {
    let status = go_status(e);
    let text = error_text(e);
    if credential_scoped(e) || is_cloudflare(status, &text) || is_invalid_grant(status, &text) {
        return false;
    }
    is_request_fault(status, &text) || matches!(status, 400 | 404 | 405 | 409 | 413 | 422 | 501)
}

/// Compact failures that must not change credential availability.
pub fn is_compact_neutral(e: &ExecError) -> bool {
    let status = go_status(e);
    let text = error_text(e);
    !(credential_scoped(e)
        || is_cloudflare(status, &text)
        || is_invalid_grant(status, &text)
        || matches!(status, 401 | 402 | 403 | 429))
}

/// A count_tokens 404 that names no model: the endpoint is missing, not the model.
pub fn is_count_endpoint_missing(e: &ExecError, requested_model: &str) -> bool {
    let base = crate::scheduler::canonical_model(requested_model);
    go_status(e) == 404 && !explicit_model_not_found(&error_text(e), base)
}

/// Executor-reported credential-wide quota (`IsCredentialScoped`).
pub fn credential_scoped(e: &ExecError) -> bool {
    e.scope == FailureScope::Credential && e.status == 429
}

/// A statusless model-scoped error: a built-in executor's Go `fmt.Errorf` that is no
/// connection fault (the
/// Claude and Antigravity compaction summary extraction and seal). Built-in executors
/// report connection faults as transport. Go classifies such an error by its message, in three separate decisions
/// (sdk/cliproxy/auth/conductor_cooldown.go): the result code (`resultErrorFromError`),
/// the cooldown skip on that result (`shouldSkipCredentialCooldown`) and the retry round
/// on the error (`isRequestRetryRoundError`).
fn statusless(e: &ExecError) -> Option<(String, String)> {
    (e.status == 0 && e.scope == FailureScope::Model).then(|| {
        let text = error_text(e);
        let lower = text.trim().go_lower();
        (text, lower)
    })
}

/// Go `shouldSkipCredentialCooldown`: request faults and transport or connection
/// lifecycle faults never cool a credential. For a statusless error, the code
/// `resultErrorFromError` assigns (model-not-found first, then lifecycle, then
/// transient) and the result's message fallbacks decide; a model-not-found code leaves
/// both fallbacks to the message, where the transient one ignores a message naming a
/// TLS handshake timeout.
pub fn skips_cooldown(e: &ExecError) -> bool {
    if is_request_invalid(e) || is_transport(e) {
        return true;
    }
    let Some((text, lower)) = statusless(e) else {
        return false;
    };
    let (lifecycle, transient) = (lifecycle_message(&lower), transient_message(&lower));
    if explicit_model_not_found(&text, "") {
        lifecycle || (transient && !lower.contains("tls handshake timeout"))
    } else {
        lifecycle || transient
    }
}

/// Go `isConnectionLifecycleMessage` on a trimmed, lowercased message.
fn lifecycle_message(lower: &str) -> bool {
    matches!(
        lower,
        "context canceled" | "context deadline exceeded" | "eof" | "unexpected eof"
    ) || [
        "websocket: close 1000",
        "websocket: close 1001",
        "websocket: close 1006",
        "unexpected eof",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
        || http2_closed(lower)
}

/// Go `isTransientTransportMessage` on a trimmed, lowercased message.
fn transient_message(lower: &str) -> bool {
    const NEEDLES: [&str; 16] = [
        "tls: tls handshake",
        "tls handshake timeout",
        "wsarecv",
        "wsasend",
        "a connection attempt failed",
        "connection refused",
        "connection reset",
        "i/o timeout",
        "no such host",
        "server misbehaving",
        "network is unreachable",
        "no route to host",
        "broken pipe",
        "connection aborted",
        "use of closed network connection",
        "unexpected eof",
    ];
    NEEDLES.iter().any(|needle| lower.contains(needle)) || http2_closed(lower)
}

/// Go `isHTTP2ClosedConnectionMessage`.
fn http2_closed(lower: &str) -> bool {
    lower.contains("http2: client connection force closed via clientconn.close")
        || lower.contains("http2: client conn is closed")
}

/// Transport, timeout and connection-lifecycle faults: no cooldown, retry-round eligible.
pub fn is_transport(e: &ExecError) -> bool {
    if e.scope == FailureScope::Transport {
        return true;
    }
    let lower = error_text(e).trim().go_lower();
    // ponytail: compatibility for executors that report transport faults untyped.
    e.headers.is_empty() && lower.starts_with("upstream request failed")
}

/// `isCredentialRetryRoundStatus` or a transport fault (`isRequestRetryRoundError`).
/// A statusless error counts when its message is Go's transient transport text
/// (`isTransientTransportError`), which leaves out the lifecycle-only texts.
pub fn is_retry_round(e: &ExecError) -> bool {
    is_transport(e)
        || matches!(e.status, 403 | 408 | 429 | 500 | 502 | 503 | 504)
        || statusless(e).is_some_and(|(_, lower)| transient_message(&lower))
}

/// `isExplicitModelNotFoundError` on an error text.
pub fn explicit_model_not_found(text: &str, requested: &str) -> bool {
    json_body(text).is_some_and(|v| structured_not_found(&v, requested))
}

fn structured_not_found(v: &Value, requested: &str) -> bool {
    match v {
        Value::Object(map) => {
            let mut not_found_type = false;
            let mut exact_reference = false;
            for (key, item) in map {
                if let Value::String(text) = item {
                    match key.trim().go_lower().as_str() {
                        "code" if model_not_found_identifier(text) => return true,
                        "type" => {
                            if model_not_found_identifier(text) {
                                return true;
                            }
                            not_found_type |= not_found_identifier(text);
                        }
                        "error" | "message" | "detail" | "error_description" | "title" => {
                            if explicit_not_found_message(text, requested) {
                                return true;
                            }
                            exact_reference |= exact_model_reference(text, requested);
                        }
                        _ => {}
                    }
                }
                if matches!(item, Value::Object(_) | Value::Array(_)) && structured_not_found(item, requested) {
                    return true;
                }
            }
            not_found_type && exact_reference
        }
        Value::Array(items) => items.iter().any(|item| {
            item.as_str().is_some_and(|t| explicit_not_found_message(t, requested))
                || structured_not_found(item, requested)
        }),
        _ => false,
    }
}

fn normalize_identifier(s: &str) -> String {
    s.replace(['-', ' '], "_")
}

fn model_not_found_identifier(value: &str) -> bool {
    let mut candidate = value.trim().go_lower();
    match candidate.rfind('#') {
        Some(i) if i + 1 < candidate.len() => candidate = candidate[i + 1..].to_owned(),
        _ => {
            if let Some(q) = candidate.find('?') {
                candidate.truncate(q);
            }
            let trimmed = candidate.trim_end_matches('/');
            candidate = match trimmed.rfind(['/', ':']) {
                Some(i) => trimmed[i + 1..].to_owned(),
                None => trimmed.to_owned(),
            };
        }
    }
    matches!(
        normalize_identifier(&candidate).as_str(),
        "model_not_found" | "model_not_found_error" | "unknown_model" | "model_does_not_exist" | "model_not_exist"
    )
}

fn not_found_identifier(value: &str) -> bool {
    matches!(
        normalize_identifier(&value.trim().go_lower()).as_str(),
        "not_found" | "not_found_error"
    )
}

fn trim_sentence(s: &str) -> String {
    s.trim()
        .go_lower()
        .trim_matches(|c| " .!;\t\r\n".contains(c))
        .to_owned()
}

fn strip_prefix<'a>(lower: &'a str, prefix: &str) -> Option<&'a str> {
    if lower == prefix {
        return Some("");
    }
    let rest = lower
        .strip_prefix(&format!("{prefix} "))
        .or_else(|| lower.strip_prefix(&format!("{prefix}:")))?;
    let rest = rest.trim();
    Some(rest.strip_prefix(':').unwrap_or(rest).trim())
}

fn explicit_not_found_message(message: &str, requested: &str) -> bool {
    let lower = trim_sentence(message);
    if lower.is_empty() || lower.contains("in request") || lower.contains("in body") || lower.contains("request body") {
        return false;
    }
    let normalized = lower.replace('-', "_");
    if normalized.contains("model_not_found") || normalized.contains("unknown_model") {
        return true;
    }
    for prefix in ["no such model", "unknown model"] {
        if let Some(rest) = strip_prefix(&lower, prefix) {
            if rest.is_empty() {
                return true;
            }
            return trim_model_reference(rest, requested).is_some_and(|s| s.is_empty());
        }
    }
    for prefix in ["the requested model", "requested model", "the model", "model"] {
        if let Some(rest) = strip_prefix(&lower, prefix) {
            if missing_phrase(rest) {
                return true;
            }
            return trim_model_reference(rest, requested).is_some_and(missing_phrase);
        }
    }
    false
}

fn exact_model_reference(message: &str, requested: &str) -> bool {
    let lower = trim_sentence(message);
    for prefix in ["the requested model", "requested model", "the model", "model"] {
        if let Some(rest) = strip_prefix(&lower, prefix) {
            return trim_model_reference(rest, requested).is_some_and(|s| s.is_empty());
        }
    }
    false
}

fn trim_model_reference<'a>(value: &'a str, requested: &str) -> Option<&'a str> {
    let model = requested.trim().go_lower();
    if model.is_empty() {
        return None;
    }
    for candidate in [
        model.clone(),
        format!("'{model}'"),
        format!("\"{model}\""),
        format!("`{model}`"),
    ] {
        if value == candidate {
            return Some("");
        }
        if let Some(rest) = value.strip_prefix(&candidate)
            && (rest.is_empty() || rest.starts_with([' ', ':', ',']))
        {
            return Some(rest.trim_start_matches([' ', ':', ',']));
        }
    }
    None
}

fn missing_phrase(value: &str) -> bool {
    matches!(
        value.trim_matches(|c| " .!;\t\r\n".contains(c)),
        "not found"
            | "was not found"
            | "could not be found"
            | "does not exist"
            | "doesn't exist"
            | "not exist"
            | "is unknown"
            | "does not exist or you do not have access to it"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    /// The Claude and Antigravity compaction summary failures (Go `fmt.Errorf`, v8.0.35
    /// 2bc236d2): a normal model failure on the trigger path, availability-neutral on the
    /// compact alt, and never a retry round.
    #[test]
    fn compaction_summary_failures_follow_go() {
        for message in [
            "extract summary: no summary text found in upstream response",
            "seal compaction capsule: generate nonce: x",
        ] {
            let e = ExecError::local(0, FailureScope::Model, message);
            assert!(!skips_cooldown(&e), "{message}");
            assert!(!is_retry_round(&e), "{message}");
            assert!(!is_transport(&e), "{message}");
            assert!(is_compact_neutral(&e) && !is_compact_fault(&e), "{message}");
            assert!(!is_request_invalid(&e), "{message}");
        }
    }

    /// Go 56eee6a4 (TestManager_MarkResult_ConnectionLifecycleDoesNotCooldown and the
    /// shouldRetryAfterError table): a statusless HTTP/2 closed-connection error, bare or
    /// wrapped as a `url.Error`, is a connection drop: no cooldown, and a retry round.
    #[test]
    fn http2_closed_connections_are_transient_like_go() {
        for message in [
            "http2: client connection force closed via ClientConn.Close",
            "http2: client conn is closed",
            "Post \"https://example.com\": http2: client conn is closed",
        ] {
            let e = ExecError::local(0, FailureScope::Model, message);
            assert!(skips_cooldown(&e), "{message}");
            assert!(is_retry_round(&e), "{message}");
        }
        let other = ExecError::local(0, FailureScope::Model, "http2: server sent GOAWAY");
        assert!(!skips_cooldown(&other) && !is_retry_round(&other));
    }

    fn upstream(status: u16, body: &str) -> ExecError {
        let mut e = ExecError::local(status, FailureScope::Request, body.to_owned());
        e.headers.insert("content-type", "application/json".parse().unwrap());
        e
    }

    #[test]
    fn request_faults_follow_go_status_and_body_rules() {
        // Anthropic's invalid_request_error envelope is a request fault even on 500.
        assert!(is_request_invalid(&upstream(
            500,
            r#"{"type":"error","error":{"type":"invalid_request_error"}}"#
        )));
        // 429/402 stay credential failures whatever the body says.
        assert!(!is_request_invalid(&upstream(
            429,
            r#"{"error":{"type":"invalid_request_error"}}"#
        )));
        // 401 authentication_error is a credential failure.
        assert!(!is_request_invalid(&upstream(
            401,
            r#"{"error":{"type":"authentication_error"}}"#
        )));
        // An upstream 404 marked Request by the executor is not a request fault in Go.
        assert!(!is_request_invalid(&upstream(404, "not found")));
        // Except Claude's missing thread state (v8.0.27 TestIsRequestFault cases): only
        // the client can recover, by replaying the conversation.
        const THREAD: &str = "No thread state was found for the requested previous_message_id. Replay the full conversation with thread create to start a new Thread.";
        let json = format!(r#"{{"type":"error","error":{{"type":"not_found_error","message":"{THREAD}"}}}}"#);
        assert!(is_request_invalid(&upstream(404, &json)));
        assert!(is_request_invalid(&upstream(404, THREAD)));
        assert!(!is_request_invalid(&upstream(
            404,
            r#"{"error":{"type":"not_found_error","message":"Not Found"}}"#
        )));
        assert!(!is_request_fault(500, &json));
        assert!(is_request_invalid(&upstream(422, "bad")));
        // Model-not-found is a capability mismatch, even on 400.
        assert!(!is_request_invalid(&upstream(
            400,
            r#"{"error":{"code":"model_not_found"}}"#
        )));
        assert!(!is_request_invalid(&upstream(400, "invalid_grant")));
        // A local Request error (no upstream headers) is explicit.
        assert!(is_request_invalid(&ExecError::local(
            501,
            FailureScope::Request,
            "not supported"
        )));
        let transport = ExecError::local(502, FailureScope::Transport, "reset");
        assert!(!is_request_invalid(&transport) && is_retry_round(&transport));
        assert_eq!(response_status(&transport), 500);
    }

    #[test]
    fn special_classifiers_match_go() {
        assert!(is_model_support(404, "The model is not supported for this account"));
        assert!(!is_model_support(500, "model not supported"));
        assert!(!is_model_support(
            500,
            r#"{"error":{"type":"not_found_error","message":"model: claude-x"}}"#
        ));
        assert!(explicit_model_not_found(
            r#"{"type":"error","error":{"type":"not_found_error","message":"model: claude-x"}}"#,
            "claude-x"
        ));
        assert!(explicit_model_not_found(
            r#"{"error":{"message":"The model `gpt-9` does not exist"}}"#,
            "gpt-9"
        ));
        assert!(!explicit_model_not_found(
            r#"{"error":{"message":"model field missing in request body"}}"#,
            ""
        ));
        assert!(is_invalid_grant(400, r#"{"error":"invalid_grant"}"#));
        assert!(!is_invalid_grant(403, "invalid_grant"));
        assert!(is_cloudflare(403, "<title>Just a moment...</title> cloudflare"));
        assert!(!is_cloudflare(503, "challenge-platform"));
        let count_404 = upstream(404, "Not Found");
        assert!(is_count_endpoint_missing(&count_404, "claude-x(high)"));
        assert!(!is_count_endpoint_missing(
            &upstream(404, r#"{"error":{"code":"model_not_found"}}"#),
            "claude-x"
        ));
        assert!(is_compact_fault(&upstream(404, "nope")) && is_compact_neutral(&upstream(404, "nope")));
        assert!(!is_compact_neutral(&upstream(429, "quota")));
        let mut empty = upstream(500, "");
        empty.body = Bytes::new();
        assert_eq!(error_text(&empty), "status 500");
    }
}
