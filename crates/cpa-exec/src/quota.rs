//! Anthropic shared-window vs model/entitlement rejection (helps/claude_ratelimit.go).

use cpa_core::gostr::GoStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cpa_core::exec::{ExecError, FailureScope};
use http::HeaderMap;

fn header(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .trim()
        .to_owned()
}
fn status(headers: &HeaderMap, window: &str) -> String {
    header(headers, &format!("anthropic-ratelimit-unified-{window}status")).to_ascii_lowercase()
}
fn allowed(status: &str) -> bool {
    matches!(status, "allowed" | "allowed_warning")
}
fn healthy(headers: &HeaderMap, window: &str) -> bool {
    header(headers, &format!("anthropic-ratelimit-unified-{window}-utilization"))
        .parse::<f64>()
        .is_ok_and(|n| n.is_finite() && (0.0..1.0).contains(&n))
}
fn overage_claim(headers: &HeaderMap) -> bool {
    header(headers, "anthropic-ratelimit-unified-representative-claim")
        .to_ascii_lowercase()
        .contains("overage")
}
fn overage_only(headers: &HeaderMap) -> bool {
    let five = status(headers, "5h-");
    let seven = status(headers, "7d-");
    if five == "rejected" || seven == "rejected" {
        return false;
    }
    let overage = status(headers, "7d_oi-") == "rejected"
        || status(headers, "overage-") == "rejected"
        || !header(headers, "anthropic-ratelimit-unified-overage-disabled-reason").is_empty()
        || overage_claim(headers);
    if !overage {
        return false;
    }
    if (allowed(&five) && allowed(&seven))
        || (allowed(&seven) && five.is_empty() && healthy(headers, "5h"))
        || (allowed(&five) && seven.is_empty() && healthy(headers, "7d"))
    {
        return true;
    }
    // v8.0.67 (0130d6f3): Anthropic omits the 5h utilization entirely when that window
    // has no usage or does not exist on the tier. A rejected Fable-only or overage claim
    // with an allowed 7d window and no 5h evidence at all is no account-wide quota
    // evidence; a 5h utilization that is present but malformed or exhausted still is.
    if allowed(&seven)
        && five.is_empty()
        && status(headers, "") == "rejected"
        && !headers.contains_key("anthropic-ratelimit-unified-5h-utilization")
    {
        return true;
    }
    // v8.0.27 (8080967): Anthropic omits both shared-window statuses when it did not
    // evaluate them; only a rejected overage claim relies on that absence.
    let absent_or_healthy = |window: &str| {
        header(headers, &format!("anthropic-ratelimit-unified-{window}-utilization")).is_empty()
            || healthy(headers, window)
    };
    status(headers, "") == "rejected"
        && five.is_empty()
        && seven.is_empty()
        && overage_claim(headers)
        && absent_or_healthy("5h")
        && absent_or_healthy("7d")
}
/// `ClaudeHeadersIndicateUnifiedRateLimitRejection`.
pub(crate) fn shared_rejection(headers: &HeaderMap) -> bool {
    status(headers, "5h-") == "rejected"
        || status(headers, "7d-") == "rejected"
        || (status(headers, "") == "rejected" && !overage_only(headers))
}

/// `classifyClaudeUpstreamErrorWithCooling`. With `model_level_cooling` a shared-window
/// rejection cools only the model, like any other 429.
pub(crate) fn classify(mut error: ExecError, model_level_cooling: bool) -> ExecError {
    if (400..600).contains(&error.status) {
        error.retry_after = rate_limit_reset_for_error(&error.headers, &error.body);
    }
    if error.status == 429 {
        let value: serde_json::Value = serde_json::from_slice(&error.body).unwrap_or_default();
        let message = value["error"]["message"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| String::from_utf8_lossy(&error.body).into_owned())
            .go_lower();
        error.scope = if !model_level_cooling && shared_rejection(&error.headers) {
            FailureScope::Credential
        } else if message.contains("fast request rejected")
            || (message.contains("fast")
                && (message.contains("usage credits") || message.contains("credits are required")))
        {
            FailureScope::Request
        } else {
            FailureScope::Model
        };
    }
    error
}

/// `ParseClaudeRateLimitResetForError` now, with Go's 1–30 s fuzz.
pub(crate) fn rate_limit_reset_for_error(headers: &HeaderMap, body: &[u8]) -> Option<Duration> {
    reset_for_error(headers, body, SystemTime::now(), fuzz())
}

/// [`reset`] with one exception: an explicit per-minute rate-limit error carries no
/// quota window, so an unclassified unified reset must not stretch its short
/// `Retry-After` into an hours-long cooldown. It applies only when the body is exactly
/// one per-minute message, no status header declares a quota window, and `Retry-After`
/// is one value inside the deadline filter's range.
pub(crate) fn reset_for_error(headers: &HeaderMap, body: &[u8], now: SystemTime, fuzz: Duration) -> Option<Duration> {
    let declares_window = QUOTA_STATUS.iter().any(|name| {
        headers
            .get_all(*name)
            .iter()
            .any(|v| !v.as_bytes().trim_ascii().is_empty())
    });
    let mut values = headers.get_all("retry-after").iter();
    let single = match (values.next(), values.next()) {
        (Some(value), None) => value.to_str().ok(),
        _ => None,
    };
    let short = single
        .and_then(|raw| retry_after(raw, now))
        .and_then(|t| t.duration_since(now).ok())
        .is_some_and(|d| !d.is_zero() && d <= MAX_WINDOW);
    if declares_window || !short || !per_minute_body(body) {
        return reset(headers, now, fuzz);
    }
    // Only the unclassified unified reset can stretch the cooldown here: each window's
    // own reset counts only when that window is rejected.
    let mut headers = headers.clone();
    headers.remove("anthropic-ratelimit-unified-reset");
    reset(&headers, now, fuzz)
}

/// `claudeQuotaStatusHeaders`: a non-empty value declares a quota window.
const QUOTA_STATUS: [&str; 5] = [
    "anthropic-ratelimit-unified-status",
    "anthropic-ratelimit-unified-5h-status",
    "anthropic-ratelimit-unified-7d-status",
    "anthropic-ratelimit-unified-7d_oi-status",
    "anthropic-ratelimit-unified-overage-status",
];

/// `claudeBodyIsExplicitPerMinuteRateLimit`: the body decodes as Go's
/// `{"error":{"type","message"}}` envelope with type `rate_limit_error` and exactly one
/// of the per-minute messages (case and surrounding whitespace aside).
fn per_minute_body(body: &[u8]) -> bool {
    const MESSAGES: [&str; 3] = [
        "this request would exceed the requests per minute rate limit. please try again later.",
        "this request would exceed the input tokens per minute rate limit. please try again later.",
        "this request would exceed the output tokens per minute rate limit. please try again later.",
    ];
    let Ok(serde_json::Value::Object(root)) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    // encoding/json decodes every key that folds to a field name in order, merging
    // nested objects; null leaves a field unchanged and any other type fails the decode.
    // serde_json keeps only the last of two byte-identical keys, so `"error":{..},
    // "error":null` reads as null where Go keeps the object (DIFFERENCES-FROM-GO.md).
    let (mut kind, mut message) = ("", "");
    for error in go_fields(&root, "error") {
        let serde_json::Value::Object(error) = error else {
            if error.is_null() {
                continue;
            }
            return false;
        };
        for (slot, name) in [(&mut kind, "type"), (&mut message, "message")] {
            for value in go_fields(error, name) {
                match value {
                    serde_json::Value::String(s) => *slot = s,
                    serde_json::Value::Null => {}
                    _ => return false,
                }
            }
        }
    }
    kind == "rate_limit_error" && MESSAGES.contains(&message.trim().go_lower().as_str())
}

/// The values encoding/json decodes into the struct field `name`, in key order: keys
/// equal to it under Go's case folding (ASCII case, plus U+017F for `s` and U+212A for
/// `k`).
fn go_fields<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    name: &'a str,
) -> impl Iterator<Item = &'a serde_json::Value> + 'a {
    let fold = |c: char| match c {
        '\u{17F}' => 's',
        '\u{212A}' => 'k',
        c => c.to_ascii_lowercase(),
    };
    object
        .iter()
        .filter(move |(key, _)| key.chars().map(fold).eq(name.chars()))
        .map(|(_, v)| v)
}

fn fuzz() -> Duration {
    // Rejection sampling avoids modulo bias, matching crypto/rand.Int(30).
    loop {
        let mut byte = [0];
        if getrandom::fill(&mut byte).is_err() {
            return Duration::from_secs(1);
        }
        if byte[0] < 240 {
            return Duration::from_secs(u64::from(byte[0] % 30) + 1);
        }
    }
}

/// `maxClaudeRateLimitWindow` (v8.0.27, 8080967): the longest Anthropic window, 7 days,
/// plus an hour of clock skew. A later reset is a billing boundary, not a rate limit.
const MAX_WINDOW: Duration = Duration::from_secs(7 * 24 * 3600 + 3600);

/// `claudeUnifiedResetIsOverageBillingBoundary`: with an overage claim, a unified reset
/// equal to the overage reset is the billing period's end.
fn overage_billing_boundary(headers: &HeaderMap, unified_reset: &str) -> bool {
    overage_claim(headers)
        && timestamp(unified_reset).is_some_and(|unified| {
            timestamp(&header(headers, "anthropic-ratelimit-unified-overage-reset")) == Some(unified)
        })
}

pub(crate) fn reset(headers: &HeaderMap, now: SystemTime, fuzz: Duration) -> Option<Duration> {
    let overage = overage_only(headers);
    let five = status(headers, "5h-");
    let seven = status(headers, "7d-");
    let oi = status(headers, "7d_oi-");
    let unified = status(headers, "");
    let mut deadlines = Vec::new();
    if !overage && let Some(deadline) = retry_after(&header(headers, "retry-after"), now) {
        deadlines.push(deadline);
    }
    for (window, rejected) in [
        ("5h-", five == "rejected"),
        ("7d-", seven == "rejected"),
        ("7d_oi-", oi == "rejected" && !overage),
        (
            "",
            !overage
                && (unified == "rejected"
                    || five == "rejected"
                    || seven == "rejected"
                    || oi == "rejected"
                    || (unified.is_empty() && !allowed(&five) && !allowed(&seven))),
        ),
    ] {
        let raw = header(headers, &format!("anthropic-ratelimit-unified-{window}reset"));
        if rejected
            && !(window.is_empty() && overage_billing_boundary(headers, &raw))
            && let Some(deadline) = timestamp(&raw)
        {
            deadlines.push(deadline);
        }
    }
    deadlines
        .into_iter()
        .filter_map(|t| t.duration_since(now).ok())
        .filter(|d| !d.is_zero() && *d <= MAX_WINDOW)
        .max()
        .and_then(|d| d.checked_add(fuzz))
}

pub(crate) fn retry_after(raw: &str, now: SystemTime) -> Option<SystemTime> {
    if let Ok(seconds) = raw.trim().parse::<f64>()
        && seconds.is_finite()
        && seconds > 0.0
    {
        return Duration::try_from_secs_f64(seconds)
            .ok()
            .and_then(|d| now.checked_add(d));
    }
    timestamp_date(raw)
}
fn timestamp(raw: &str) -> Option<SystemTime> {
    if let Ok(seconds) = raw.trim().parse::<f64>()
        && seconds.is_finite()
        && seconds > 0.0
    {
        return Duration::try_from_secs_f64(seconds)
            .ok()
            .and_then(|d| UNIX_EPOCH.checked_add(d));
    }
    timestamp_date(raw)
}
fn timestamp_date(raw: &str) -> Option<SystemTime> {
    chrono::DateTime::parse_from_rfc3339(raw.trim())
        .ok()
        .map(SystemTime::from)
        .or_else(|| httpdate::parse_http_date(raw.trim()).ok())
}

/// Go `collectQuotaSignals("claude", headers)`: `Retry-After` and every
/// `Anthropic-Ratelimit-Unified-*` header under its canonical name with its last value,
/// trimmed; empty, oversized (over 512 bytes) and control-character values are dropped.
/// Both kinds rank first, so at most 64 are kept in name order.
pub fn claude_signals(headers: &HeaderMap) -> std::collections::BTreeMap<String, String> {
    const MAX_SIGNALS: usize = 64;
    const MAX_VALUE: usize = 512;
    let mut signals = std::collections::BTreeMap::new();
    for name in headers.keys() {
        let lower = name.as_str();
        if lower != "retry-after" && !lower.starts_with("anthropic-ratelimit-unified-") {
            continue;
        }
        let Some(value) = headers.get_all(name).iter().next_back() else {
            continue;
        };
        let value = String::from_utf8_lossy(value.as_bytes());
        let value = value.trim();
        if value.is_empty() || value.len() > MAX_VALUE || value.chars().any(|c| c < '\u{20}' || c == '\u{7f}') {
            continue;
        }
        signals.insert(crate::proxy::canonical_header(lower), value.to_owned());
    }
    signals.into_iter().take(MAX_SIGNALS).collect()
}

/// The Anthropic unified quota fields a cooldown may keep as evidence: those Go
/// v8.0.67 (8bba7a77) or this repository reads, top-level and per window. Anything else
/// under the `Anthropic-Ratelimit-Unified-` prefix is not quota evidence and is never
/// kept, replayed or saved.
const QUOTA_FIELDS: [&str; 8] = [
    "status",
    "reset",
    "representative-claim",
    "fallback-percentage",
    "overage-status",
    "overage-reset",
    "overage-utilization",
    "overage-disabled-reason",
];
const QUOTA_WINDOWS: [&str; 4] = ["5h", "7d", "7d_oi", "7d_sonnet"];
const QUOTA_WINDOW_FIELDS: [&str; 4] = ["status", "reset", "utilization", "surpassed-threshold"];
/// Bounds on what a cooldown keeps: one short value per allowlisted field, and in all
/// at most every allowlisted name (64 bytes at most) with its longest value, so the
/// aggregate bound never drops a legitimate field.
const MAX_EVIDENCE_VALUE: usize = 128;
const MAX_EVIDENCE_BYTES: usize =
    (QUOTA_FIELDS.len() + QUOTA_WINDOWS.len() * QUOTA_WINDOW_FIELDS.len()) * (64 + MAX_EVIDENCE_VALUE);

/// The canonical name of an allowlisted quota field, if `name` (any case) is one.
fn quota_field(name: &str) -> Option<String> {
    // The longest allowlisted name is 54 bytes; anything longer is no quota field.
    const MAX_NAME: usize = 64;
    let name = name.trim();
    if name.len() > MAX_NAME {
        return None;
    }
    let lower = name.to_ascii_lowercase();
    let field = lower.strip_prefix("anthropic-ratelimit-unified-")?;
    let known = QUOTA_FIELDS.contains(&field)
        || QUOTA_WINDOWS.iter().any(|window| {
            field
                .strip_prefix(window)
                .and_then(|rest| rest.strip_prefix('-'))
                .is_some_and(|rest| QUOTA_WINDOW_FIELDS.contains(&rest))
        });
    known.then(|| crate::proxy::canonical_header(&lower))
}

/// A quota value kept exactly as Anthropic sent it: printable ASCII, bounded, after
/// trimming. Anything else (invalid UTF-8, other bytes, control characters) is dropped,
/// never rewritten.
fn quota_value(raw: &[u8]) -> Option<String> {
    let value = raw.trim_ascii();
    (!value.is_empty() && value.len() <= MAX_EVIDENCE_VALUE && value.iter().all(|b| (0x20..0x7f).contains(b)))
        .then(|| String::from_utf8(value.to_vec()).expect("printable ASCII is UTF-8"))
}

/// The kept fields, if they show a rejection (Go `hasRejectedClaudeUnifiedSignals`: the
/// overall status or a window status `rejected`), within the aggregate bound.
fn rejected_evidence(fields: impl Iterator<Item = (String, String)>) -> std::collections::BTreeMap<String, String> {
    let mut kept = std::collections::BTreeMap::new();
    let mut bytes = 0;
    for (name, value) in fields {
        if kept.contains_key(&name) || bytes + name.len() + value.len() > MAX_EVIDENCE_BYTES {
            continue;
        }
        bytes += name.len() + value.len();
        kept.insert(name, value);
    }
    let rejected = kept.iter().any(|(name, value)| {
        let field = name.to_ascii_lowercase();
        let field = field.trim_start_matches("anthropic-ratelimit-unified-");
        let status = field == "status" || QUOTA_WINDOWS.iter().any(|w| field == format!("{w}-status"));
        status && value.eq_ignore_ascii_case("rejected")
    });
    if rejected { kept } else { Default::default() }
}

/// Go v8.0.67 `rejectedClaudeSignalsForCooldown` (cb70eacf): the Anthropic unified quota
/// fields of a 429 that rejected a quota window, kept with the cooldown it causes so a
/// local cooldown 429 can show the client the provider's own evidence. Only allowlisted
/// fields with their last value as sent; nothing when no window or overall status is
/// `rejected`, or when the body is an ordinary per-minute limit, which is no
/// subscription quota.
pub fn rejected_cooldown_signals(headers: &HeaderMap, body: &[u8]) -> std::collections::BTreeMap<String, String> {
    if per_minute_body(body) {
        return Default::default();
    }
    rejected_evidence(headers.keys().filter_map(|name| {
        let field = quota_field(name.as_str())?;
        let value = quota_value(headers.get_all(name).iter().next_back()?.as_bytes())?;
        Some((field, value))
    }))
}

/// What a saved cooldown file restores, reduced by the same boundary a live 429 gets
/// ([`rejected_cooldown_signals`]), so a file can bring back nothing a 429 could not
/// have recorded.
pub fn sanitize_cooldown_signals(
    signals: std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, String> {
    rejected_evidence(
        signals
            .into_iter()
            .filter_map(|(name, value)| Some((quota_field(&name)?, quota_value(value.as_bytes())?))),
    )
}

/// The latest passive quota snapshots per Claude credential and per model (Go
/// `QuotaState.ObserveResponseHeadersForProvider` for provider `claude` on `Auth.Quota`
/// and the result model's `ModelState.Quota`): each upstream Messages response that
/// carries a signal replaces both snapshots; one without leaves them untouched. The
/// store is Codex's; only the header rules differ.
// ponytail: snapshots stay until the process restarts, one per Claude credential ever
// used; Go drops them with the auth. Add a retain pass if credential churn ever matters.
#[derive(Default)]
pub struct Observations(crate::codex_quota::QuotaSignals);

impl Observations {
    /// The credential's snapshot only.
    pub fn observe(&self, credential_id: &str, headers: &HeaderMap) {
        self.observe_model(credential_id, "", headers);
    }

    /// The credential's and `model`'s snapshots (keyed by the model without its
    /// thinking suffix).
    pub fn observe_model(&self, credential_id: &str, model: &str, headers: &HeaderMap) {
        self.observe_at(credential_id, model, headers, SystemTime::now());
    }

    fn observe_at(&self, credential_id: &str, model: &str, headers: &HeaderMap, observed_at: SystemTime) {
        self.0
            .record(credential_id, model, claude_signals(headers), observed_at);
    }

    pub fn snapshot(&self, credential_id: &str) -> Option<crate::codex_quota::Snapshot> {
        self.0.snapshot(credential_id)
    }

    /// The latest snapshot per model (Go `model_quotas`), empty when none was observed.
    pub fn model_snapshots(
        &self,
        credential_id: &str,
    ) -> std::collections::BTreeMap<String, crate::codex_quota::Snapshot> {
        self.0.model_snapshots(credential_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(values: &[(&str, &str)]) -> HeaderMap {
        values
            .iter()
            .map(|(key, value)| {
                (
                    http::HeaderName::from_bytes(key.as_bytes()).unwrap(),
                    value.parse().unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn model_shared_and_fast_entitlement_scopes() {
        let mut error = ExecError::local(429, FailureScope::Credential, r#"{"error":{"message":"slow down"}}"#);
        assert_eq!(classify(error.clone(), false).scope, FailureScope::Model);
        error
            .headers
            .insert("anthropic-ratelimit-unified-5h-status", " Rejected ".parse().unwrap());
        assert_eq!(classify(error.clone(), false).scope, FailureScope::Credential);
        error.body = bytes::Bytes::from_static(br#"{"error":{"message":"Usage credits are required for fast mode"}}"#);
        assert_eq!(
            classify(error.clone(), false).scope,
            FailureScope::Credential,
            "shared window takes precedence"
        );
        error.headers.clear();
        assert_eq!(classify(error, false).scope, FailureScope::Request);
    }

    #[test]
    fn overage_only_excludes_retry_after_and_unhealthy_missing_windows_do_not() {
        let now = UNIX_EPOCH + Duration::from_secs(1000);
        let mut h = headers(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-7d-status", "allowed_warning"),
            ("anthropic-ratelimit-unified-5h-utilization", "0.0"),
            ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
            ("anthropic-ratelimit-unified-reset", "2000"),
            ("anthropic-ratelimit-unified-7d_oi-reset", "4000"),
            ("retry-after", "5000"),
        ]);
        assert!(!shared_rejection(&h));
        assert_eq!(reset(&h, now, Duration::ZERO), None);
        for utilization in ["NaN", "-0.1", "1", "inf", "bad"] {
            h.insert(
                "anthropic-ratelimit-unified-5h-utilization",
                utilization.parse().unwrap(),
            );
            assert!(shared_rejection(&h), "{utilization}");
            assert_eq!(reset(&h, now, Duration::from_secs(3)), Some(Duration::from_secs(5003)));
        }
    }

    /// #72: a Fable-only weekly (7d_oi) or overage rejection with an allowed 7d window
    /// and the 5h headers omitted stays model-scoped; present 5h evidence that is
    /// malformed or exhausted still cools the credential (Go 0130d6f3).
    #[test]
    fn fable_only_rejection_without_5h_headers_stays_model_scoped() {
        for claim in [
            ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
            ("anthropic-ratelimit-unified-overage-status", "rejected"),
        ] {
            let mut h = headers(&[
                ("anthropic-ratelimit-unified-status", "rejected"),
                ("anthropic-ratelimit-unified-7d-status", "allowed"),
                ("anthropic-ratelimit-unified-7d-utilization", "0.41"),
                claim,
            ]);
            assert!(!shared_rejection(&h), "{claim:?}");
            let mut error = ExecError::local(429, FailureScope::Credential, r#"{"error":{"message":"limit"}}"#);
            *error.headers = h.clone();
            assert_eq!(classify(error.clone(), false).scope, FailureScope::Model, "{claim:?}");
            for utilization in ["1", "1.0", "bad", ""] {
                h.insert(
                    "anthropic-ratelimit-unified-5h-utilization",
                    utilization.parse().unwrap(),
                );
                assert!(shared_rejection(&h), "{claim:?} 5h {utilization:?}");
            }
            h.insert("anthropic-ratelimit-unified-5h-utilization", "0.2".parse().unwrap());
            assert!(!shared_rejection(&h), "{claim:?} healthy 5h");
            // A rejected 7d window is account-wide whatever the 5h headers say.
            h.remove("anthropic-ratelimit-unified-5h-utilization");
            h.insert("anthropic-ratelimit-unified-7d-status", "rejected".parse().unwrap());
            assert!(shared_rejection(&h), "{claim:?} rejected 7d");
        }
    }

    /// #94 (Go v8.0.67 cb70eacf): a cooldown keeps only the rejected unified quota
    /// headers, bounded like passive signals; `Retry-After` and every other header stay
    /// out, and allowed-only, overage-only, per-minute and malformed evidence keeps
    /// nothing.
    #[test]
    fn cooldown_keeps_only_rejected_unified_quota_headers() {
        let h = headers(&[
            ("anthropic-ratelimit-unified-status", " rejected "),
            ("anthropic-ratelimit-unified-5h-status", "rejected"),
            ("anthropic-ratelimit-unified-5h-reset", "1800000000"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.4"),
            ("retry-after", "60"),
            ("request-id", "req_x"),
            ("anthropic-organization-id", "org-x"),
            ("anthropic-ratelimit-unified-empty", " "),
        ]);
        let kept = rejected_cooldown_signals(&h, br#"{"error":{"type":"rate_limit_error","message":"limit"}}"#);
        assert_eq!(
            kept.into_iter().collect::<Vec<_>>(),
            [
                (
                    "Anthropic-Ratelimit-Unified-5h-Reset".to_owned(),
                    "1800000000".to_owned()
                ),
                (
                    "Anthropic-Ratelimit-Unified-5h-Status".to_owned(),
                    "rejected".to_owned()
                ),
                (
                    "Anthropic-Ratelimit-Unified-7d-Utilization".to_owned(),
                    "0.4".to_owned()
                ),
                ("Anthropic-Ratelimit-Unified-Status".to_owned(), "rejected".to_owned()),
            ]
        );
        let per_minute = br#"{"type":"error","error":{"type":"rate_limit_error","message":"This request would exceed the requests per minute rate limit. Please try again later."}}"#;
        assert!(
            rejected_cooldown_signals(&h, per_minute).is_empty(),
            "a per-minute limit"
        );
        for values in [
            vec![("anthropic-ratelimit-unified-status", "allowed")],
            vec![("anthropic-ratelimit-unified-overage-status", "rejected")],
            vec![("retry-after", "60")],
        ] {
            assert!(
                rejected_cooldown_signals(&headers(&values), b"{}").is_empty(),
                "{values:?}"
            );
        }
        let model_scoped = headers(&[("anthropic-ratelimit-unified-7d_oi-status", "rejected")]);
        assert_eq!(rejected_cooldown_signals(&model_scoped, b"{}").len(), 1);
        // What a saved file restores is filtered the same way: no foreign or malformed
        // names, no oversized or control-character values.
        let saved: std::collections::BTreeMap<String, String> = [
            ("anthropic-ratelimit-unified-5h-status", "rejected"),
            ("Set-Cookie", "session=x"),
            ("anthropic-ratelimit-unified-x\r\nset-cookie", "rejected"),
            ("anthropic-ratelimit-unified-7d-reset", "1\n2"),
            ("anthropic-ratelimit-unified-7d-status", &"r".repeat(600)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        assert_eq!(
            sanitize_cooldown_signals(saved).into_iter().collect::<Vec<_>>(),
            [(
                "Anthropic-Ratelimit-Unified-5h-Status".to_owned(),
                "rejected".to_owned()
            )]
        );
    }

    /// Sol review of #128: only allowlisted provider quota fields are evidence. A
    /// prefixed name that is no quota field (credentials, cookies, personal data), an
    /// unbounded name or value, and a value that is not printable ASCII (invalid UTF-8
    /// included) are dropped on capture and on restore alike, never rewritten; the kept
    /// set stays within its aggregate bound.
    #[test]
    fn cooldown_evidence_is_an_allowlisted_bounded_exact_copy() {
        let mut h = headers(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-authorization", "Bearer fake-secret"),
            ("anthropic-ratelimit-unified-cookie", "fake-session=private"),
            ("anthropic-ratelimit-unified-user-email", "private@example.invalid"),
            ("anthropic-ratelimit-unified-7d_sonnet-reset", "1800000000"),
            ("anthropic-ratelimit-unified-7d_oi-surpassed-threshold", "0.75"),
        ]);
        h.insert(
            "anthropic-ratelimit-unified-representative-claim",
            http::HeaderValue::from_bytes(b"\xff").unwrap(),
        );
        h.insert(
            "anthropic-ratelimit-unified-5h-reset",
            http::HeaderValue::from_bytes("1800000000\u{e9}".as_bytes()).unwrap(),
        );
        let kept = rejected_cooldown_signals(&h, b"{}");
        assert_eq!(
            kept.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "Anthropic-Ratelimit-Unified-7d_oi-Surpassed-Threshold",
                "Anthropic-Ratelimit-Unified-7d_sonnet-Reset",
                "Anthropic-Ratelimit-Unified-Status",
            ]
        );
        let long_name = format!("anthropic-ratelimit-unified-{}", "x".repeat(1_000_000));
        let saved: std::collections::BTreeMap<String, String> = [
            ("anthropic-ratelimit-unified-status".to_owned(), "rejected".to_owned()),
            (long_name, "private".to_owned()),
            ("anthropic-ratelimit-unified-cookie".to_owned(), "x".to_owned()),
            (
                "Anthropic-Ratelimit-Unified-5h-Utilization".to_owned(),
                "\u{fffd}".to_owned(),
            ),
            ("anthropic-ratelimit-unified-5h-reset".to_owned(), "9".repeat(129)),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            sanitize_cooldown_signals(saved).into_iter().collect::<Vec<_>>(),
            [("Anthropic-Ratelimit-Unified-Status".to_owned(), "rejected".to_owned())]
        );
        // Every allowlisted field at its longest value fits the aggregate bound.
        let mut all = std::collections::BTreeMap::new();
        for field in QUOTA_FIELDS {
            all.insert(
                format!("anthropic-ratelimit-unified-{field}"),
                "r".repeat(MAX_EVIDENCE_VALUE),
            );
        }
        for window in QUOTA_WINDOWS {
            for field in QUOTA_WINDOW_FIELDS {
                all.insert(
                    format!("anthropic-ratelimit-unified-{window}-{field}"),
                    "r".repeat(MAX_EVIDENCE_VALUE),
                );
            }
        }
        all.insert("anthropic-ratelimit-unified-status".into(), "rejected".into());
        let kept = sanitize_cooldown_signals(all);
        let bytes: usize = kept.iter().map(|(k, v)| k.len() + v.len()).sum();
        assert!(bytes <= MAX_EVIDENCE_BYTES && kept.contains_key("Anthropic-Ratelimit-Unified-Status"));
        assert!(kept.keys().all(|k| k.len() <= 64));
    }

    #[test]
    fn latest_relevant_deadline_fractional_seconds_and_dates() {
        let now = UNIX_EPOCH + Duration::from_secs(1000);
        let h = headers(&[
            ("anthropic-ratelimit-unified-5h-status", "rejected"),
            ("anthropic-ratelimit-unified-5h-reset", "1030.5"),
            ("anthropic-ratelimit-unified-7d-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-reset", "999999"),
            ("retry-after", "20.25"),
        ]);
        assert_eq!(
            reset(&h, now, Duration::from_secs(7)),
            Some(Duration::from_millis(37500))
        );
        let date = "Fri, 02 Oct 2026 12:00:00 GMT";
        let expected = httpdate::parse_http_date(date).unwrap();
        assert_eq!(retry_after(date, now), Some(expected));
        assert_eq!(timestamp("2026-10-02T12:00:00Z"), Some(expected));
        for bad in ["NaN", "inf", "-1", "0", "1e100", "wrong"] {
            assert_eq!(retry_after(bad, now), None);
        }
    }

    /// The Issue6465 cases of v8.0.27's claude_ratelimit_test.go (8080967): whether the
    /// shared windows rejected the credential, and the cooldown without fuzz.
    #[test]
    fn overage_billing_resets_and_far_deadlines_are_ignored_like_go() {
        let now = |rfc3339: &str| SystemTime::from(chrono::DateTime::parse_from_rfc3339(rfc3339).unwrap());
        let oct7 = now("2026-10-07T20:46:04Z");
        let unix = |t: SystemTime, after: u64| (t.duration_since(UNIX_EPOCH).unwrap().as_secs() + after).to_string();
        let five_hours = unix(oct7, 5 * 3600);
        let oct29 = now("2026-10-29T00:00:00Z");
        let billing = unix(oct29, 48 * 3600);
        let h = |pairs: &[(&str, &str)]| headers(pairs);
        let cases = [
            (
                "overage claim without shared windows",
                h(&[
                    ("anthropic-ratelimit-unified-status", "rejected"),
                    ("anthropic-ratelimit-unified-representative-claim", "overage"),
                    ("anthropic-ratelimit-unified-reset", "1793491200"),
                    ("anthropic-ratelimit-unified-overage-status", "allowed"),
                    ("anthropic-ratelimit-unified-overage-reset", "1793491200"),
                    ("anthropic-ratelimit-unified-overage-utilization", "0.11"),
                ]),
                oct7,
                false,
                None,
            ),
            (
                "rejected 5h window ignores the monthly unified reset",
                h(&[
                    ("anthropic-ratelimit-unified-status", "rejected"),
                    ("anthropic-ratelimit-unified-5h-status", "rejected"),
                    ("anthropic-ratelimit-unified-5h-reset", &five_hours),
                    ("anthropic-ratelimit-unified-reset", "1793491200"),
                    ("anthropic-ratelimit-unified-representative-claim", "five_hour"),
                ]),
                oct7,
                true,
                Some(5 * 3600),
            ),
            (
                "unified reset equal to the overage reset leaves Retry-After",
                h(&[
                    ("anthropic-ratelimit-unified-status", "rejected"),
                    ("anthropic-ratelimit-unified-representative-claim", "overage"),
                    ("anthropic-ratelimit-unified-7d-status", "allowed"),
                    ("anthropic-ratelimit-unified-7d-utilization", "0.69"),
                    // v8.0.67 0130d6f3: an omitted 5h utilization is no longer
                    // account-wide evidence, so the fixture names an exhausted one.
                    ("anthropic-ratelimit-unified-5h-utilization", "1.0"),
                    ("anthropic-ratelimit-unified-reset", &billing),
                    ("anthropic-ratelimit-unified-overage-status", "rejected"),
                    ("anthropic-ratelimit-unified-overage-reset", &billing),
                    ("retry-after", "60"),
                ]),
                oct29,
                true,
                Some(60),
            ),
            (
                "five_hour claim with overage disabled stays credential-wide",
                h(&[
                    ("anthropic-ratelimit-unified-status", "rejected"),
                    ("anthropic-ratelimit-unified-representative-claim", "five_hour"),
                    (
                        "anthropic-ratelimit-unified-overage-disabled-reason",
                        "org_level_disabled",
                    ),
                    ("retry-after", "10800"),
                ]),
                oct7,
                true,
                Some(3 * 3600),
            ),
            (
                "overage claim with exhausted 5h utilization stays credential-wide",
                h(&[
                    ("anthropic-ratelimit-unified-status", "rejected"),
                    ("anthropic-ratelimit-unified-representative-claim", "overage"),
                    ("anthropic-ratelimit-unified-5h-utilization", "1.0"),
                    ("retry-after", "10800"),
                ]),
                oct7,
                true,
                Some(3 * 3600),
            ),
            (
                "overage claim with healthy utilization stays model-scoped",
                h(&[
                    ("anthropic-ratelimit-unified-status", "rejected"),
                    ("anthropic-ratelimit-unified-representative-claim", "overage"),
                    ("anthropic-ratelimit-unified-5h-utilization", "0.00"),
                    ("anthropic-ratelimit-unified-reset", "1793491200"),
                    ("anthropic-ratelimit-unified-overage-reset", "1793491200"),
                    ("retry-after", "10800"),
                ]),
                oct7,
                false,
                None,
            ),
            (
                "Retry-After beyond seven days",
                h(&[
                    ("anthropic-ratelimit-unified-status", "rejected"),
                    ("anthropic-ratelimit-unified-5h-status", "rejected"),
                    ("retry-after", "2034192"),
                ]),
                oct7,
                true,
                None,
            ),
            (
                "allowed unified status keeps a short Retry-After",
                h(&[
                    ("anthropic-ratelimit-unified-status", "allowed"),
                    ("anthropic-ratelimit-unified-representative-claim", "overage"),
                    ("anthropic-ratelimit-unified-overage-status", "allowed"),
                    ("retry-after", "30"),
                ]),
                oct7,
                false,
                Some(30),
            ),
            (
                "missing unified status keeps a short Retry-After",
                h(&[
                    ("anthropic-ratelimit-unified-representative-claim", "overage"),
                    ("anthropic-ratelimit-unified-overage-status", "allowed"),
                    ("retry-after", "30"),
                ]),
                oct7,
                false,
                Some(30),
            ),
        ];
        for (name, headers, now, shared, cooldown) in cases {
            assert_eq!(shared_rejection(&headers), shared, "{name}: shared rejection");
            assert_eq!(
                reset(&headers, now, Duration::ZERO),
                cooldown.map(Duration::from_secs),
                "{name}: cooldown"
            );
        }
    }

    /// TestQuotaStateObserveResponseHeadersRetainsMeasuredClaudeAndCodexWatermarks (the
    /// Claude half), quota_signals_test.go.
    #[test]
    fn claude_observation_keeps_go_measured_watermarks() {
        let measured = [
            ("Anthropic-Ratelimit-Unified-5h-Status", "allowed"),
            ("Anthropic-Ratelimit-Unified-5h-Utilization", "0.0"),
            ("Anthropic-Ratelimit-Unified-5h-Reset", "1787296800"),
            ("Anthropic-Ratelimit-Unified-7d-Status", "allowed"),
            ("Anthropic-Ratelimit-Unified-7d-Utilization", "0.53"),
            ("Anthropic-Ratelimit-Unified-7d-Reset", "1787695200"),
            ("Anthropic-Ratelimit-Unified-Fallback-Percentage", "0.5"),
            (
                "Anthropic-Ratelimit-Unified-Overage-Disabled-Reason",
                "member_zero_credit_limit",
            ),
            ("Anthropic-Ratelimit-Unified-Overage-Status", "rejected"),
            ("Anthropic-Ratelimit-Unified-Representative-Claim", "five_hour"),
            ("Anthropic-Ratelimit-Unified-Reset", "1787296800"),
            ("Anthropic-Ratelimit-Unified-Status", "allowed"),
        ];
        let mut all = measured.to_vec();
        all.push(("Anthropic-Workspace-Id", "workspace-must-not-be-quota-signal"));
        // Codex's signals are not Claude's (TestQuotaStateObserveResponseHeadersKeepsProviderScopedSignals).
        all.push(("X-Codex-Primary-Used-Percent", "2"));
        all.push(("Authorization", "Bearer secret"));
        let signals = claude_signals(&headers(&all));
        let want: std::collections::BTreeMap<String, String> = measured
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        assert_eq!(signals, want);
    }

    /// TestObserveResponseHeadersReplacesStaleWatermarks,
    /// ...KeepsSnapshotWhenResponseCarriesNoSignal, ...AdvancesObservedAtOnRepeatedValues
    /// and ...RejectsControlCharacterValues, for Claude's headers; per model as in
    /// TestMarkResultQuotaFailureDoesNotEraseSiblingObservation.
    #[test]
    fn observations_replace_keep_advance_and_reject_like_go() {
        let at = |secs| UNIX_EPOCH + Duration::from_secs(secs);
        let obs = Observations::default();
        obs.observe_at(
            "c",
            "claude-a(high)",
            &headers(&[
                ("Retry-After", "120"),
                ("Anthropic-Ratelimit-Unified-Status", "rejected"),
            ]),
            at(100),
        );
        assert_eq!(obs.snapshot("c").unwrap().signals["Retry-After"], "120");
        obs.observe_at(
            "c",
            "claude-b",
            &headers(&[("Anthropic-Ratelimit-Unified-Status", "allowed")]),
            at(200),
        );
        let snap = obs.snapshot("c").unwrap();
        assert!(
            !snap.signals.contains_key("Retry-After"),
            "a stale Retry-After never survives"
        );
        assert_eq!(snap.signals["Anthropic-Ratelimit-Unified-Status"], "allowed");
        assert_eq!(snap.observed_at, at(200));
        // Each model keeps its own latest snapshot, keyed without the thinking suffix:
        // a response for one model never replaces a sibling's.
        let models = obs.model_snapshots("c");
        assert_eq!(models.keys().collect::<Vec<_>>(), ["claude-a", "claude-b"]);
        assert_eq!(models["claude-a"].signals["Retry-After"], "120");
        assert_eq!(models["claude-a"].observed_at, at(100));
        assert_eq!(
            models["claude-b"].signals["Anthropic-Ratelimit-Unified-Status"],
            "allowed"
        );
        // No signal: the previous snapshots stay.
        obs.observe_at(
            "c",
            "claude-b",
            &headers(&[("Content-Type", "application/json")]),
            at(300),
        );
        assert_eq!(obs.snapshot("c").unwrap().observed_at, at(200));
        assert_eq!(obs.model_snapshots("c")["claude-b"].observed_at, at(200));
        // Repeated values still advance the observation time.
        obs.observe_at(
            "c",
            "claude-b",
            &headers(&[("Anthropic-Ratelimit-Unified-Status", "allowed")]),
            at(400),
        );
        assert_eq!(obs.snapshot("c").unwrap().observed_at, at(400));
        // Control characters, empty and oversized values are never stored.
        let mut bad = headers(&[("Anthropic-Ratelimit-Unified-Status", "")]);
        bad.insert(
            "anthropic-ratelimit-unified-reset",
            // The only control byte an HTTP header value can carry.
            http::HeaderValue::from_bytes(b"1\tX-Injected: 1").unwrap(),
        );
        bad.insert("anthropic-ratelimit-unified-claim", "x".repeat(513).parse().unwrap());
        assert!(claude_signals(&bad).is_empty());
        assert!(obs.snapshot("other").is_none());
    }

    /// TestObserveResponseHeadersTruncatesDeterministically, for Claude: the first 64 names.
    #[test]
    fn claude_signals_cap_at_64_by_name() {
        let many: Vec<(String, String)> = (0..128)
            .map(|i| (format!("Anthropic-Ratelimit-Unified-L{i:03}-Status"), i.to_string()))
            .collect();
        let refs: Vec<(&str, &str)> = many.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let signals = claude_signals(&headers(&refs));
        assert_eq!(signals.len(), 64);
        assert!(signals.contains_key("Anthropic-Ratelimit-Unified-L000-Status"));
        assert!(signals.contains_key("Anthropic-Ratelimit-Unified-L063-Status"));
        assert!(!signals.contains_key("Anthropic-Ratelimit-Unified-L064-Status"));
    }
}
