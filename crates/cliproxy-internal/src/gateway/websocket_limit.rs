//! Subscription-limit errors for native Codex clients on the Responses WebSocket.
//!
//! Go closes the downstream socket without a frame for every quota failure
//! (`shouldExposeResponsesUpstreamError`), so Codex cannot tell an exhausted subscription
//! from a dropped connection and retries against it. Current Codex classifies a wrapped
//! `error` event with status 429 (openai/codex e0aa043,
//! `codex-api/src/endpoint/responses_websocket.rs` `map_wrapped_websocket_error_event`
//! and `codex-api/src/api_bridge.rs` `map_api_error`): `error.type`
//! `usage_limit_reached` (with `plan_type`, `resets_at`, `limit_window_minutes` and the
//! rate-limit headers), `usage_not_included`, or an exhausted quota.
//!
//! This module rebuilds that event from an allowlist only: fixed messages, validated
//! scalar fields and Codex's rate-limit headers. Upstream messages, account
//! identifiers, tokens, other headers and nested fields never pass. Credential health and
//! cooldowns are recorded by dispatch before an error reaches this formatting.

use std::time::Duration;

use axum::http::HeaderMap;
use serde_json::{Map, Value, json};

/// Codex `api_bridge.rs`: these codes map to `QuotaExceeded`.
const QUOTA_CODES: [&str; 5] = [
    "insufficient_quota",
    "credit_balance_exhausted",
    "organization_spend_limit_exceeded",
    "project_spend_limit_exceeded",
    "organization_usage_limit_exceeded",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    UsageLimit,
    NotIncluded,
    Quota,
}

/// Where the upstream's response headers come from: `HeaderMap`s (an HTTP response, a
/// WebSocket error frame's `headers` kept as the error's addon) or the frame's JSON
/// object itself.
pub(crate) enum Headers<'a> {
    Maps(&'a [&'a HeaderMap]),
    Json(Option<&'a Map<String, Value>>),
}

impl Headers<'_> {
    fn get(&self, name: &str) -> Option<String> {
        match self {
            Headers::Maps(maps) => maps
                .iter()
                .find_map(|m| m.get(name))
                .and_then(|v| v.to_str().ok())
                .map(|v| v.trim().to_owned()),
            Headers::Json(object) => (*object)?
                .iter()
                .filter(|(k, _)| k.trim().eq_ignore_ascii_case(name))
                .find_map(|(_, v)| match v {
                    Value::String(s) => Some(s.trim().to_owned()),
                    Value::Number(n) => Some(n.to_string()),
                    Value::Bool(b) => Some(b.to_string()),
                    _ => None,
                }),
        }
    }
}

/// The client-safe event for a subscription-limit failure with `status` and the JSON
/// error `body` (`{"error":{..}}`, a wrapped error event, or a bare error object), or
/// `None` for anything else. `now` is the Unix time in seconds.
pub(crate) fn event(status: u16, body: &str, headers: &Headers<'_>, now: i64) -> Option<String> {
    if status != 429 {
        return None;
    }
    build(status, &json(body)?, headers, now)
}

fn build(status: u16, value: &Value, headers: &Headers<'_>, now: i64) -> Option<String> {
    let (kind, error) = classify(status, value)?;
    let mut out = error_object(kind, error);
    if kind == Kind::UsageLimit {
        if let Some(plan) = error.get("plan_type").and_then(Value::as_str).filter(|p| plan_type(p)) {
            out.insert("plan_type".into(), plan.into());
        }
        if let Some(at) = resets_at(error, now) {
            out.insert("resets_at".into(), at.into());
        }
        if let Some(minutes) = error
            .get("limit_window_minutes")
            .and_then(Value::as_u64)
            .filter(|m| u16::try_from(*m).is_ok())
        {
            out.insert("limit_window_minutes".into(), minutes.into());
        }
    }
    Some(wrap(out, safe_headers(headers)))
}

/// A wrapped upstream `error` event (`status` or `status_code`, `error`, `headers`).
pub(crate) fn from_error_event(payload: &str, now: i64) -> Option<String> {
    let value = json(payload)?;
    let status = ["status", "status_code"]
        .iter()
        .find_map(|k| value.get(*k).and_then(Value::as_u64).filter(|s| *s > 0))?;
    let status = u16::try_from(status).ok()?;
    let headers = Headers::Json(value.get("headers").and_then(Value::as_object));
    build(status, &value, &headers, now)
}

/// Every eligible credential is cooling after a limit: `cause` is the last upstream
/// error the scheduler kept, `wait` the earliest retry. The reset and `Retry-After` are
/// the scheduler's, which may be sooner than any one account's reset; the account's own
/// plan and window are left out because the cause names just one pooled account.
pub(crate) fn cooldown(cause: &str, wait: Duration, now: i64) -> Option<String> {
    // Like `upstream_summary`, a short prefix before the JSON body is skipped.
    let value = json(cause).or_else(|| match cause.find('{') {
        Some(i) if i < 50 => json(&cause[i..]),
        _ => None,
    })?;
    let (kind, _) = classify(429, &value)?;
    let mut out = error_object(kind, &Map::new());
    let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    if kind == Kind::UsageLimit {
        out.insert("resets_at".into(), (now.saturating_add(seconds as i64)).into());
    }
    let mut headers = Map::new();
    headers.insert("retry-after".into(), seconds.to_string().into());
    Some(wrap(out, headers))
}

/// One parse of a bounded error body, on the failure path only.
fn json(body: &str) -> Option<Value> {
    serde_json::from_str(body.trim()).ok()
}

fn classify(status: u16, value: &Value) -> Option<(Kind, &Map<String, Value>)> {
    if status != 429 {
        return None;
    }
    let root = value.as_object()?;
    // `{"error":{..}}`, a frame's `body.error` (`buildCodexWebsocketErrorPayload`), or a
    // bare error object.
    let error = match (root.get("error"), root.get("body").and_then(|b| b.get("error"))) {
        (Some(Value::Object(error)), _) | (_, Some(Value::Object(error))) => error,
        _ => root,
    };
    let field = |k: &str| {
        error
            .get(k)
            .and_then(Value::as_str)
            .map(|s| s.trim().to_ascii_lowercase())
            .unwrap_or_default()
    };
    let (kind_field, code) = (field("type"), field("code"));
    let kind = match kind_field.as_str() {
        "usage_limit_reached" => Kind::UsageLimit,
        "usage_not_included" => Kind::NotIncluded,
        "insufficient_quota" => Kind::Quota,
        _ if QUOTA_CODES.contains(&code.as_str()) => Kind::Quota,
        _ => return None,
    };
    Some((kind, error))
}

/// `type`, `code` and a fixed message. Codex renders its own text for each kind.
fn error_object(kind: Kind, error: &Map<String, Value>) -> Map<String, Value> {
    let (kind_name, message) = match kind {
        Kind::UsageLimit => ("usage_limit_reached", "The usage limit has been reached."),
        Kind::NotIncluded => ("usage_not_included", "Usage is not included in this plan."),
        Kind::Quota => ("insufficient_quota", "The quota for this account is exhausted."),
    };
    let mut out = Map::new();
    out.insert("type".into(), kind_name.into());
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .map(|c| c.trim().to_ascii_lowercase())
        .filter(|c| QUOTA_CODES.contains(&c.as_str()));
    if let Some(code) = code {
        out.insert("code".into(), code.into());
    }
    out.insert("message".into(), message.into());
    out
}

fn wrap(error: Map<String, Value>, headers: Map<String, Value>) -> String {
    let mut event = json!({"type": "error", "status": 429, "error": error});
    if !headers.is_empty() {
        event["headers"] = Value::Object(headers);
    }
    event.to_string()
}

/// Codex parses `plan_type` as a known plan or any other string; only a plan-like token
/// passes.
fn plan_type(plan: &str) -> bool {
    !plan.is_empty() && plan.len() <= 64 && plan.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// `resets_at` (Unix seconds), else `resets_in_seconds` from `now` (Codex reads only the
/// absolute form).
fn resets_at(error: &Map<String, Value>, now: i64) -> Option<i64> {
    if let Some(at) = error.get("resets_at").and_then(Value::as_i64).filter(|at| *at > 0) {
        return Some(at);
    }
    let seconds = error
        .get("resets_in_seconds")
        .and_then(Value::as_i64)
        .filter(|s| *s > 0)?;
    Some(now.saturating_add(seconds))
}

#[derive(Clone, Copy)]
enum Shape {
    Seconds,
    Integer,
    Decimal,
    Bool,
    Token,
    Name,
}

fn valid(value: &str, shape: Shape) -> bool {
    let bounded = |max: usize| !value.is_empty() && value.len() <= max;
    match shape {
        // `Retry-After` seconds; Codex also accepts an HTTP date.
        Shape::Seconds => {
            (bounded(10) && value.bytes().all(|b| b.is_ascii_digit()))
                || (bounded(40) && value.bytes().all(|b| b.is_ascii_alphanumeric() || b" ,:".contains(&b)))
        }
        Shape::Integer => bounded(20) && value.trim_start_matches('-').bytes().all(|b| b.is_ascii_digit()),
        Shape::Decimal => bounded(24) && value.parse::<f64>().is_ok_and(f64::is_finite),
        Shape::Bool => value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("false"),
        Shape::Token => bounded(64) && value.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
        Shape::Name => bounded(64) && value.bytes().all(|b| b.is_ascii_alphanumeric() || b" _.-".contains(&b)),
    }
}

/// Codex's rate-limit and retry headers (`codex-api/src/rate_limits.rs`,
/// `http-client/src/retry_after.rs`), each value checked against its shape. The limit
/// families are the default `x-codex-*` and the one `x-codex-active-limit` names.
/// `x-codex-promo-message` is free text and stays out.
fn safe_headers(headers: &Headers<'_>) -> Map<String, Value> {
    let mut out = Map::new();
    let mut keep = |name: &str, shape: Shape| {
        if let Some(value) = headers.get(name).filter(|v| valid(v, shape)) {
            out.insert(name.to_owned(), value.into());
        }
    };
    keep("retry-after", Shape::Seconds);
    keep("x-codex-rate-limit-reached-type", Shape::Token);
    keep("x-codex-credits-has-credits", Shape::Bool);
    keep("x-codex-credits-unlimited", Shape::Bool);
    keep("x-codex-credits-balance", Shape::Decimal);
    let active = headers.get("x-codex-active-limit").filter(|v| valid(v, Shape::Token));
    let mut families = vec!["x-codex".to_owned()];
    if let Some(active) = &active {
        keep("x-codex-active-limit", Shape::Token);
        // Codex: lower-cased, `_` as `-`.
        let family = format!("x-{}", active.to_ascii_lowercase().replace('_', "-"));
        if !families.contains(&family) {
            families.push(family);
        }
    }
    for family in &families {
        for window in ["primary", "secondary"] {
            keep(&format!("{family}-{window}-used-percent"), Shape::Decimal);
            keep(&format!("{family}-{window}-window-minutes"), Shape::Integer);
            keep(&format!("{family}-{window}-reset-at"), Shape::Integer);
        }
        keep(&format!("{family}-limit-name"), Shape::Name);
    }
    out
}

#[cfg(test)]
#[path = "websocket_limit_tests.rs"]
mod tests;
