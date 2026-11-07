use super::*;

const NOW: i64 = 1_800_000_000;

fn parsed(event: &str) -> Value {
    serde_json::from_str(event).unwrap()
}

fn maps(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (k, v) in pairs {
        map.append(
            axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            v.parse().unwrap(),
        );
    }
    map
}

#[test]
fn usage_limit_keeps_only_codex_fields() {
    let body = r#"{"error":{"type":"usage_limit_reached","message":"limit for acct_1","plan_type":"pro","resets_at":1800003600,"limit_window_minutes":10080,"account_id":"acct_1","nested":{"token":"sk-x"},"resets_in_seconds":5}}"#;
    let event = event(429, body, &Headers::Maps(&[]), NOW).unwrap();
    assert_eq!(
        parsed(&event),
        json!({
            "type": "error",
            "status": 429,
            "error": {
                "type": "usage_limit_reached",
                "message": "The usage limit has been reached.",
                "plan_type": "pro",
                "resets_at": 1_800_003_600_i64,
                "limit_window_minutes": 10080,
            }
        })
    );
}

#[test]
fn relative_reset_and_invalid_fields() {
    let body = r#"{"error":{"type":"usage_limit_reached","plan_type":"pro plan <script>","resets_at":-1,"resets_in_seconds":90,"limit_window_minutes":70000}}"#;
    let event = parsed(&event(429, body, &Headers::Maps(&[]), NOW).unwrap());
    assert_eq!(
        event["error"],
        json!({"type": "usage_limit_reached", "message": "The usage limit has been reached.", "resets_at": NOW + 90})
    );
}

#[test]
fn only_limits_at_429_are_formatted() {
    let limit = r#"{"error":{"type":"usage_limit_reached"}}"#;
    assert!(event(400, limit, &Headers::Maps(&[]), NOW).is_none());
    assert!(event(401, limit, &Headers::Maps(&[]), NOW).is_none());
    for body in [
        r#"{"error":{"type":"rate_limit_error","code":"rate_limit_exceeded"}}"#,
        r#"{"error":{"type":"server_error"}}"#,
        "usage_limit_reached",
        "",
    ] {
        assert!(event(429, body, &Headers::Maps(&[]), NOW).is_none(), "{body}");
    }
}

#[test]
fn quota_and_not_included_kinds() {
    let cases = [
        (
            r#"{"error":{"type":"usage_not_included","message":"x"}}"#,
            "usage_not_included",
            None,
        ),
        (r#"{"error":{"type":"insufficient_quota"}}"#, "insufficient_quota", None),
        (
            r#"{"error":{"code":"credit_balance_exhausted","type":"x"}}"#,
            "insufficient_quota",
            Some("credit_balance_exhausted"),
        ),
        // A top-level error object, as `isCodexUsageLimitError` also accepts.
        (r#"{"type":"usage_limit_reached"}"#, "usage_limit_reached", None),
    ];
    for (body, kind, code) in cases {
        let event = parsed(&event(429, body, &Headers::Maps(&[]), NOW).unwrap());
        assert_eq!(event["error"]["type"], kind, "{body}");
        assert_eq!(event["error"]["code"].as_str(), code, "{body}");
        assert!(event["error"].get("plan_type").is_none());
    }
}

#[test]
fn headers_are_allowlisted_and_validated() {
    let response = maps(&[
        ("retry-after", "30"),
        ("x-codex-active-limit", "codex_other"),
        ("x-codex-primary-used-percent", "99.5"),
        ("x-codex-primary-window-minutes", "300"),
        ("x-codex-primary-reset-at", "1800003600"),
        ("x-codex-secondary-used-percent", "not-a-number"),
        ("x-codex-other-primary-used-percent", "12"),
        ("x-codex-other-limit-name", "GPT-5 Codex"),
        ("x-codex-credits-has-credits", "false"),
        ("x-codex-credits-balance", "1.25"),
        ("x-codex-promo-message", "free text"),
        ("x-unrelated-primary-used-percent", "1"),
        ("x-openai-account-id", "acct_1"),
        ("set-cookie", "a=b"),
        ("authorization", "Bearer sk-x"),
    ]);
    let body = r#"{"error":{"type":"usage_limit_reached"}}"#;
    let event = parsed(&event(429, body, &Headers::Maps(&[&response]), NOW).unwrap());
    assert_eq!(
        event["headers"],
        json!({
            "retry-after": "30",
            "x-codex-active-limit": "codex_other",
            "x-codex-primary-used-percent": "99.5",
            "x-codex-primary-window-minutes": "300",
            "x-codex-primary-reset-at": "1800003600",
            "x-codex-other-primary-used-percent": "12",
            "x-codex-other-limit-name": "GPT-5 Codex",
            "x-codex-credits-has-credits": "false",
            "x-codex-credits-balance": "1.25",
        })
    );
}

#[test]
fn frame_headers_win_over_response_headers() {
    let frame = maps(&[("retry-after", "5")]);
    let response = maps(&[("retry-after", "50"), ("x-codex-primary-used-percent", "100")]);
    let body = r#"{"error":{"type":"usage_limit_reached"}}"#;
    let event = parsed(&event(429, body, &Headers::Maps(&[&frame, &response]), NOW).unwrap());
    assert_eq!(
        event["headers"],
        json!({"retry-after": "5", "x-codex-primary-used-percent": "100"})
    );
}

#[test]
fn wrapped_error_event_with_json_headers() {
    let payload = r#"{"type":"error","status_code":429,"error":{"type":"usage_limit_reached","plan_type":"plus","resets_at":1800000600},"headers":{"Retry-After":7,"x-codex-primary-used-percent":100.0,"cookie":"a=b","x-codex-primary-window-minutes":{"nested":1}}}"#;
    let event = parsed(&from_error_event(payload, NOW).unwrap());
    assert_eq!(
        event,
        json!({
            "type": "error",
            "status": 429,
            "error": {
                "type": "usage_limit_reached",
                "message": "The usage limit has been reached.",
                "plan_type": "plus",
                "resets_at": 1_800_000_600_i64,
            },
            "headers": {"retry-after": "7", "x-codex-primary-used-percent": "100.0"},
        })
    );
    // A frame's `body.error` (Go `buildCodexWebsocketErrorPayload`).
    let body = r#"{"type":"error","status":429,"body":{"error":{"type":"usage_limit_reached"}}}"#;
    assert!(from_error_event(body, NOW).is_some());
    assert!(from_error_event(r#"{"type":"error","error":{"type":"usage_limit_reached"}}"#, NOW).is_none());
}

#[test]
fn cooldown_reports_the_scheduler_wait() {
    let cause = r#"{"error":{"type":"usage_limit_reached","plan_type":"pro","resets_at":1900000000,"limit_window_minutes":300}}"#;
    let event = parsed(&cooldown(cause, Duration::from_millis(59_100), NOW).unwrap());
    assert_eq!(
        event,
        json!({
            "type": "error",
            "status": 429,
            "error": {
                "type": "usage_limit_reached",
                "message": "The usage limit has been reached.",
                "resets_at": NOW + 60,
            },
            "headers": {"retry-after": "60"},
        })
    );
    let prefixed = format!("codex: {cause}");
    assert!(cooldown(&prefixed, Duration::from_secs(1), NOW).is_some());
    assert!(cooldown(r#"{"error":{"type":"rate_limit_error"}}"#, Duration::from_secs(1), NOW).is_none());
    assert!(cooldown("status 429", Duration::from_secs(1), NOW).is_none());
}

/// Codex's own parse, copied field for field from openai/codex e0aa043: the wrapped event
/// (`codex-api/src/endpoint/responses_websocket.rs` `WrappedWebsocketErrorEvent`) and the
/// usage body read from it (`codex-api/src/api_bridge.rs` `UsageErrorResponse`). Codex
/// maps 429 with `error.type` `usage_limit_reached` to `UsageLimitReached`, reading
/// `plan_type`, `resets_at` and `limit_window_minutes` (as `u16`); `Retry-After` comes
/// from `headers`. A copy, not Codex itself: Codex's crates are not dependencies here.
mod codex_mirror {
    use serde::Deserialize;
    use serde_json::{Map, Value};

    #[derive(Debug, Deserialize)]
    #[allow(dead_code)]
    pub struct WrappedWebsocketError {
        pub code: Option<String>,
        pub message: Option<String>,
        #[serde(default)]
        pub headers: Option<Value>,
    }

    #[derive(Debug, Deserialize)]
    pub struct WrappedWebsocketErrorEvent {
        #[serde(rename = "type")]
        pub kind: String,
        #[serde(alias = "status_code")]
        pub status: Option<u16>,
        #[serde(default)]
        pub error: Option<WrappedWebsocketError>,
        #[serde(default)]
        pub headers: Option<Map<String, Value>>,
    }

    #[derive(Debug, Deserialize)]
    pub struct UsageErrorResponse {
        pub error: UsageErrorBody,
    }

    #[derive(Debug, Deserialize)]
    #[allow(dead_code)]
    pub struct UsageErrorBody {
        pub code: Option<String>,
        #[serde(rename = "type")]
        pub error_type: Option<String>,
        // Codex: `codex_protocol::auth::PlanType`, a known plan or any other string.
        pub plan_type: Option<String>,
        pub resets_at: Option<i64>,
        pub limit_window_minutes: Option<Value>,
    }
}

#[test]
fn current_codex_classifies_the_event_as_a_usage_limit() {
    use codex_mirror::{UsageErrorResponse, WrappedWebsocketErrorEvent};
    let body = r#"{"error":{"type":"usage_limit_reached","plan_type":"pro","resets_at":1800003600,"limit_window_minutes":300,"account_id":"acct_1"}}"#;
    let response = maps(&[("retry-after", "120"), ("x-codex-primary-used-percent", "100")]);
    let payload = event(429, body, &Headers::Maps(&[&response]), NOW).unwrap();

    let wrapped: WrappedWebsocketErrorEvent = serde_json::from_str(&payload).unwrap();
    assert_eq!(wrapped.kind, "error");
    assert_eq!(wrapped.status, Some(429));
    // No `code` that Codex would turn into a retryable connection error.
    assert!(wrapped.error.as_ref().unwrap().code.is_none());
    let headers = wrapped.headers.unwrap();
    assert_eq!(headers["retry-after"], "120");
    assert_eq!(headers["x-codex-primary-used-percent"], "100");

    // `TransportError::Http` keeps the original payload as the body `map_api_error` reads.
    let usage: UsageErrorResponse = serde_json::from_str(&payload).unwrap();
    assert_eq!(usage.error.error_type.as_deref(), Some("usage_limit_reached"));
    assert_eq!(usage.error.plan_type.as_deref(), Some("pro"));
    assert_eq!(usage.error.resets_at, Some(1_800_003_600));
    let window = usage.error.limit_window_minutes.as_ref().and_then(Value::as_u64);
    assert_eq!(window.and_then(|m| u16::try_from(m).ok()), Some(300));

    for body in [
        r#"{"error":{"type":"usage_not_included"}}"#,
        r#"{"error":{"code":"insufficient_quota"}}"#,
    ] {
        let payload = event(429, body, &Headers::Maps(&[]), NOW).unwrap();
        let usage: UsageErrorResponse = serde_json::from_str(&payload).unwrap();
        let quota = usage.error.error_type.as_deref() == Some("insufficient_quota")
            || usage.error.code.as_deref() == Some("insufficient_quota");
        assert!(
            usage.error.error_type.as_deref() == Some("usage_not_included") || quota,
            "{payload}"
        );
    }
}
