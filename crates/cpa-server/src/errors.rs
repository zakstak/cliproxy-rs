//! Route error shapes: Go's `BuildErrorResponseBody` (OpenAI, Responses, Gemini and
//! Interactions routes) and `toClaudeError` (Claude routes).

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use serde_json::Value;

use crate::dispatch::Failure;
use crate::gojson::{self, Obj};

/// Go `WriteErrorResponse`'s text: the error text trimmed, else the status text.
fn text_for(status: u16, text: &str) -> String {
    let v = gojson::trim(text);
    if v.is_empty() {
        gojson::status_text(status).to_owned()
    } else {
        v.to_owned()
    }
}

/// Go `json.Compact` of valid JSON: the white space between tokens dropped, strings
/// untouched (no HTML escaping), so an upstream error fits on one SSE `data:` line.
pub(crate) fn compact_json(json: &str) -> String {
    let mut out = Vec::with_capacity(json.len());
    let (mut in_string, mut escaped) = (false, false);
    for &b in json.as_bytes() {
        if in_string {
            out.push(b);
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
        } else if !matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
            in_string = b == b'"';
            out.push(b);
        }
    }
    // Only ASCII white space was removed, outside strings.
    String::from_utf8(out).expect("compacting valid UTF-8 JSON keeps it UTF-8")
}

/// Go `BuildErrorResponseBody`: valid JSON passes through compacted (v8.0.35
/// cd9e5ace), anything else is wrapped in an OpenAI error envelope typed by status.
pub fn openai_body(status: u16, text: &str) -> String {
    let status = if status == 0 { 500 } else { status };
    let text = if gojson::trim(text).is_empty() {
        gojson::status_text(status)
    } else {
        text
    };
    let trimmed = gojson::trim(text);
    if !trimmed.is_empty() && serde_json::from_str::<Value>(trimmed).is_ok() {
        return compact_json(trimmed);
    }
    let (kind, code) = match status {
        401 => ("authentication_error", "invalid_api_key"),
        403 => ("permission_error", "insufficient_quota"),
        429 => ("rate_limit_error", "rate_limit_exceeded"),
        404 => ("invalid_request_error", "model_not_found"),
        408 => ("server_error", "request_timeout"),
        500.. => ("server_error", "internal_server_error"),
        _ => ("invalid_request_error", ""),
    };
    let mut detail = Obj::new().str("message", text).str("type", kind);
    if !code.is_empty() {
        detail = detail.str("code", code);
    }
    Obj::new().raw("error", &detail.finish()).finish()
}

/// Go `claudeErrorTypeFromStatus`.
fn claude_type(status: u16) -> &'static str {
    match status {
        401 => "authentication_error",
        402 => "billing_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        // v8.0.27 (42d484f) adds 408.
        408 | 504 => "timeout_error",
        529 => "overloaded_error",
        500.. => "api_error",
        _ => "invalid_request_error",
    }
}

/// Go `toClaudeError` + `claudeErrorDetailFromText`. A missing thread state also gets
/// `details.error_code: thread_not_found`, Anthropic's signal to replay the whole
/// conversation (v8.0.27, 5c959cb).
pub fn claude_body(status: u16, text: &str) -> String {
    let mut message = text_for(status, text);
    let thread_not_found = crate::classify::is_claude_thread_not_found(status, &message);
    let mut kind = claude_type(status).to_owned();
    if let Ok(Value::Object(payload)) = serde_json::from_str::<Value>(&message) {
        let field = |obj: &serde_json::Map<String, Value>, key: &str| {
            obj.get(key)
                .and_then(Value::as_str)
                .map(gojson::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        if let Some(Value::Object(err)) = payload.get("error") {
            kind = field(err, "type").unwrap_or(kind);
            if let Some(m) = field(err, "message").or_else(|| field(err, "code")) {
                message = m;
            }
        } else {
            kind = field(&payload, "type").filter(|t| t != "error").unwrap_or(kind);
            if let Some(m) = field(&payload, "message") {
                message = m;
            }
        }
    }
    let mut detail = Obj::new().str("type", &kind).str("message", &message);
    if thread_not_found {
        detail = detail.raw("details", r#"{"error_code":"thread_not_found"}"#);
    }
    Obj::new().str("type", "error").raw("error", &detail.finish()).finish()
}

/// Writes a failure with the route's body shape.
pub fn write(failure: &Failure, body: fn(u16, &str) -> String) -> Response {
    write_with(failure, body, |_, raw| raw.clone())
}

/// [`write`] with the direct response's body rewritten by `direct_body`.
fn write_with(
    failure: &Failure,
    body: fn(u16, &str) -> String,
    direct_body: impl FnOnce(u16, &bytes::Bytes) -> bytes::Bytes,
) -> Response {
    // Go `writeDirectErrorResponse`: the filtered headers as given, JSON by default.
    if let Some(e) = failure.direct() {
        let delivered = e.delivered_status.unwrap_or(e.status);
        let status = StatusCode::from_u16(delivered).unwrap_or(StatusCode::BAD_GATEWAY);
        let mut res = Response::new(axum::body::Body::from(direct_body(delivered, &e.body)));
        *res.status_mut() = status;
        let headers = crate::respond::filter_upstream_headers(&e.headers);
        for name in headers.keys() {
            for value in headers.get_all(name) {
                res.headers_mut().append(name.clone(), value.clone());
            }
        }
        if !res.headers().contains_key(header::CONTENT_TYPE) {
            res.headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        }
        return res;
    }
    let status = failure.status();
    let text = text_for(status, &failure.text());
    let mut res = crate::respond::json(status, "application/json", body(status, &text));
    if let Some(seconds) = failure.retry_after() {
        res.headers_mut().insert(header::RETRY_AFTER, seconds.into());
    }
    res
}

pub fn openai(failure: &Failure) -> Response {
    write(failure, openai_body)
}

pub fn claude(failure: &Failure) -> Response {
    // Go's Claude `WriteErrorResponse` marks a direct missing-thread answer too.
    write_with(failure, claude_body, |status, raw| {
        let text = String::from_utf8_lossy(raw);
        if crate::classify::is_claude_thread_not_found(status, &text) {
            bytes::Bytes::from(claude_body(status, &text))
        } else {
            raw.clone()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_envelope_matches_go() {
        assert_eq!(
            openai_body(500, "status 500"),
            r#"{"error":{"message":"status 500","type":"server_error","code":"internal_server_error"}}"#
        );
        assert_eq!(openai_body(400, " {\"a\":1} "), r#"{"a":1}"#);
        // Recorded from Go v8.0.35 BuildErrorResponseBody, including its
        // TestBuildErrorResponseBody_CompactsPrettyPrintedJSON input.
        for (text, want) in [
            (
                "{\n  \"error\": {\n    \"code\": 500,\n    \"message\": \"Internal error encountered.\",\n    \"status\": \"INTERNAL\"\n  }\n}",
                r#"{"error":{"code":500,"message":"Internal error encountered.","status":"INTERNAL"}}"#,
            ),
            (" [ 1 ,\t2 ]\r\n", "[1,2]"),
            (
                "{\"a b\" : \"x  y\\n <&> \\u2028 \u{2028}\", \"n\" : [ ] }",
                "{\"a b\":\"x  y\\n <&> \\u2028 \u{2028}\",\"n\":[]}",
            ),
            ("{\"q\":\"a\\\" b\" , \"r\" : 1}", r#"{"q":"a\" b","r":1}"#),
            ("\"  spaced  \"", "\"  spaced  \""),
        ] {
            assert_eq!(openai_body(502, text), want, "{text:?}");
        }
        assert_eq!(
            openai_body(422, "bad <x>"),
            r#"{"error":{"message":"bad \u003cx\u003e","type":"invalid_request_error"}}"#
        );
        assert_eq!(
            openai_body(429, ""),
            r#"{"error":{"message":"Too Many Requests","type":"rate_limit_error","code":"rate_limit_exceeded"}}"#
        );
    }

    #[test]
    fn claude_envelope_matches_go_detail_rules() {
        assert_eq!(
            claude_body(
                429,
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#
            ),
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#
        );
        assert_eq!(
            claude_body(400, r#"{"error":{"code":"bad_thing"}}"#),
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad_thing"}}"#
        );
        assert_eq!(
            claude_body(529, r#"{"type":"error","message":"busy"}"#),
            r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#
        );
        // Go: an empty upstream body becomes `status N` before formatting.
        assert_eq!(
            claude_body(500, "status 500"),
            r#"{"type":"error","error":{"type":"api_error","message":"status 500"}}"#
        );
        assert_eq!(
            claude_body(502, "  "),
            r#"{"type":"error","error":{"type":"api_error","message":"Bad Gateway"}}"#
        );
        assert_eq!(
            claude_body(408, "stream closed"),
            r#"{"type":"error","error":{"type":"timeout_error","message":"stream closed"}}"#
        );
    }

    /// v8.0.27 TestClaudeErrorMarks*MissingThreadForClientReplay: the executor's plain
    /// message and Anthropic's own JSON both get the replay marker; nothing else does.
    #[test]
    fn missing_thread_is_marked_for_client_replay() {
        const THREAD: &str = "No thread state was found for the requested previous_message_id. Replay the full conversation with thread create to start a new Thread.";
        let marked = format!(
            r#"{{"type":"error","error":{{"type":"not_found_error","message":"{THREAD}","details":{{"error_code":"thread_not_found"}}}}}}"#
        );
        assert_eq!(claude_body(404, THREAD), marked);
        let upstream = format!(r#"{{"type":"error","error":{{"type":"not_found_error","message":"{THREAD}"}}}}"#);
        assert_eq!(claude_body(404, &upstream), marked);
        assert_eq!(
            claude_body(404, r#"{"error":{"type":"not_found_error","message":"Not Found"}}"#),
            r#"{"type":"error","error":{"type":"not_found_error","message":"Not Found"}}"#
        );
        assert!(!claude_body(500, THREAD).contains("thread_not_found"));
        // A direct (fast-mode) answer is rewritten on Claude routes only
        // (TestWriteClaudeDirectErrorMarksMissingThreadForClientReplay).
        let direct = |body: &str| {
            let mut error =
                cpa_core::exec::ExecError::local(404, cpa_core::exec::FailureScope::Request, body.to_owned());
            error.direct = true;
            Failure::Exec(error)
        };
        let read = |res: Response| {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(axum::body::to_bytes(res.into_body(), usize::MAX))
                .unwrap()
        };
        assert_eq!(read(claude(&direct(&upstream))), marked.as_bytes());
        assert_eq!(read(openai(&direct(&upstream))), upstream.as_bytes());
        assert_eq!(read(claude(&direct("not found"))), "not found".as_bytes());
    }
}
