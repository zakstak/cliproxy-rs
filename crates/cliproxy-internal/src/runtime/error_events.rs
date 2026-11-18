//! Go `errorEvent` (sdk/cliproxy/auth/error_events.go): published on the RESP `errors`
//! channel for every failed attempt, with the credential's state after the failure was
//! recorded.
//!
//! The state comes from the scheduler's live cooldown records, the same view the `.cds`
//! files persist (Go `ModelStates`).
// ponytail: Go keeps a separate quota-recovery clock, so a 429 after a longer transient
// cooldown reports the quota's own `next_recover_at`; here both are the one cooldown
// deadline. Go's error `code` and `retryable` are not carried (always omitted), and a
// credential whose cooldown has lapsed reports `active` where Go keeps `error` until
// the next success.

use std::time::SystemTime;

use crate::credential::Credential;
use crate::execution::ExecError;

use crate::gateway::gojson::Obj;
use crate::persistence::cooldown::Record;

fn time(t: SystemTime) -> String {
    crate::gateway::gojson::string(&crate::gateway::usage_record::go_timestamp(&chrono::DateTime::<
        chrono::Local,
    >::from(t)))
}

/// Go `errorEventQuotaStatus`, omitted when nothing is set.
fn quota(record: &Record, reason: &str) -> Option<String> {
    let q = &record.quota;
    if !q.exceeded && reason.is_empty() && q.next_recover_at.is_none() && q.backoff_level == 0 {
        return None;
    }
    let mut o = Obj::new().raw("exceeded", if q.exceeded { "true" } else { "false" });
    if !reason.is_empty() {
        o = o.str("reason", reason);
    }
    if let Some(at) = q.next_recover_at {
        o = o.raw("next_recover_at", &time(at));
    }
    if q.backoff_level != 0 {
        o = o.raw("backoff_level", &q.backoff_level.to_string());
    }
    Some(o.finish())
}

/// The payload for a failure of `c` on `model`, given its live cooldown `records`.
pub(crate) fn payload(c: &Credential, model: &str, error: &ExecError, records: &[Record]) -> Vec<u8> {
    let key = crate::runtime::scheduler::canonical_model(model);
    // The failing model's state, and the credential-wide quota when one is live (Go keeps
    // that on the auth itself).
    let model_record = records.iter().find(|r| r.model == key);
    let credential_record = records.iter().find(|r| r.model.is_empty());
    let status_code = match crate::runtime::classify::go_status(error) {
        0 => 500,
        s => s,
    };
    let body = crate::runtime::classify::error_text(error);
    let body = body.trim();
    let message = |r: &Record| {
        if r.reason == "cloudflare challenge" {
            r.reason.clone()
        } else {
            r.last_error
                .as_ref()
                .map(|e| e.message.trim().to_owned())
                .unwrap_or_default()
        }
    };
    let mut auth_status = Obj::new();
    match credential_record.or(model_record) {
        Some(auth_record) => {
            let message = message(model_record.unwrap_or(auth_record));
            let auth_reason = match auth_record.quota.reason.as_str() {
                "" => "",
                "credential_quota" => "credential_quota",
                _ => "quota",
            };
            auth_status = auth_status.str("status", "error");
            if !message.is_empty() {
                auth_status = auth_status.str("status_message", &message);
            }
            auth_status = auth_status
                .raw("disabled", if c.disabled { "true" } else { "false" })
                .raw("unavailable", "true");
            if let Some(at) = auth_record.next_retry_after {
                auth_status = auth_status.raw("next_retry_after", &time(at));
            }
            if let Some(q) = quota(auth_record, auth_reason) {
                auth_status = auth_status.raw("quota", &q);
            }
            if let Some(r) = model_record {
                let model_reason = match r.quota.reason.as_str() {
                    "credential_quota" => "quota",
                    reason => reason,
                };
                let mut model_status = Obj::new().str("name", model.trim()).str("status", "error");
                if !message.is_empty() {
                    model_status = model_status.str("status_message", &message);
                }
                model_status = model_status.raw("unavailable", "true");
                if let Some(at) = r.next_retry_after {
                    model_status = model_status.raw("next_retry_after", &time(at));
                }
                if let Some(q) = quota(r, model_reason) {
                    model_status = model_status.raw("quota", &q);
                }
                auth_status = auth_status.raw("model", &model_status.finish());
            }
        }
        None => {
            auth_status = auth_status
                .str("status", if c.disabled { "disabled" } else { "active" })
                .raw("disabled", if c.disabled { "true" } else { "false" })
                .raw("unavailable", "false");
        }
    }
    let mut event = Obj::new().raw(
        "timestamp",
        &crate::gateway::gojson::string(&crate::gateway::usage_record::go_timestamp(&chrono::Local::now())),
    );
    let provider = c.provider.trim();
    if !provider.is_empty() {
        event = event.str("provider", provider);
    }
    if !model.trim().is_empty() {
        event = event.str("model", model.trim());
    }
    if !c.id.trim().is_empty() {
        event = event.str("auth_id", c.id.trim());
    }
    event = event
        .str("auth_index", crate::config::credentials::auth_index(c).trim())
        .raw("status_code", &status_code.to_string())
        .str("body", if body.is_empty() { "request failed" } else { body })
        .raw("auth_status", &auth_status.finish());
    event.finish().into_bytes()
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use crate::execution::FailureScope;
    use serde_json::Value;

    use super::*;
    use crate::runtime::Outcome;
    use crate::runtime::scheduler::{Policy, Scheduler};

    /// Event consumers see the scheduler state after each failure, including the
    /// identity, retry deadline and quota scope. Input traces carry no expected output.
    #[test]
    fn failure_events_expose_current_scheduler_state() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../tests/gateway/fixtures/error_event_inputs.json")).unwrap();
        let cases = fixture.as_array().unwrap();
        assert_eq!(cases.len(), 25);
        let policy = Policy::default();
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let mut metadata = serde_json::Map::new();
            metadata.insert("type".into(), "claude".into());
            let c = Credential::from_file(
                Path::new("/mock"),
                &Path::new("/mock").join(format!("{name}.json")),
                metadata,
            )
            .unwrap();
            let mut s = Scheduler::default();
            let (now, wall) = (Instant::now(), SystemTime::now());
            for step in case["steps"].as_array().unwrap() {
                let status = step["status"].as_u64().unwrap() as u16;
                // The scheduler goldens' mapping of Go's classification to scopes.
                let scope = if step["credential_scope"].as_bool().unwrap() {
                    FailureScope::Credential
                } else {
                    match status {
                        429 => FailureScope::Model,
                        401..=403 | 408 | 500.. => FailureScope::Credential,
                        _ => FailureScope::Request,
                    }
                };
                let mut error = ExecError::local(status, scope, step["message"].as_str().unwrap());
                error
                    .headers
                    .insert("content-type", "application/json".parse().unwrap());
                let hint = step["retry_after_ms"].as_i64().unwrap();
                error.retry_after = (hint >= 0).then(|| Duration::from_millis(hint as u64));
                let model = step["model"].as_str().unwrap();
                s.record(&c, model, &Outcome::Failure(error.clone()), &policy, now);
                let records = s.records(&c, now, wall);
                let got: Value = serde_json::from_slice(&payload(&c, model, &error, &records)).unwrap();
                assert_eq!(got["status_code"], if status == 0 { 500 } else { status });
                assert_eq!(got["auth_id"], c.id);
                assert_eq!(got["provider"], "claude");
                assert_eq!(got["model"], model.trim());
                assert!(chrono::DateTime::parse_from_rfc3339(got["timestamp"].as_str().unwrap()).is_ok());
                assert_eq!(got["auth_status"]["disabled"], false);
                let key = crate::runtime::scheduler::canonical_model(model);
                let model_record = records.iter().find(|record| record.model == key);
                let auth_record = records.iter().find(|record| record.model.is_empty()).or(model_record);
                assert_eq!(got["auth_status"]["unavailable"], auth_record.is_some());
                if let Some(record) = auth_record {
                    assert_eq!(got["auth_status"]["status"], "error");
                    let actual =
                        chrono::DateTime::parse_from_rfc3339(got["auth_status"]["next_retry_after"].as_str().unwrap())
                            .unwrap();
                    assert_eq!(SystemTime::from(actual), record.next_retry_after.unwrap());
                    if record.quota.exceeded {
                        assert_eq!(got["auth_status"]["quota"]["exceeded"], true);
                    }
                } else {
                    assert_eq!(got["auth_status"]["status"], "active");
                    assert!(got["auth_status"].get("next_retry_after").is_none());
                }
                if let Some(record) = model_record {
                    assert_eq!(got["auth_status"]["model"]["name"], model.trim());
                    assert_eq!(got["auth_status"]["model"]["unavailable"], true);
                    if record.quota.exceeded {
                        assert_eq!(got["auth_status"]["model"]["quota"]["exceeded"], true);
                    }
                }
            }
        }
    }
}
