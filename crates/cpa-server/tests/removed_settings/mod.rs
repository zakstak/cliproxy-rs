//! Go recordings made with settings of the providers this fork removed (issue #49): what
//! a golden test drops from them so the rest still compares exactly. Every path comes
//! from `cpa_core::removed::is_setting_path`, so only a removed setting at its canonical,
//! legacy or provider-keyed OAuth map path goes; the same names anywhere else (payload
//! rule params, unknown `metadata` maps, string values) are user data and stay.
// Each test binary uses part of this module.
#![allow(dead_code)]

use serde_json::Value;

/// The removed settings in a JSON config view, exact paths in document order.
pub fn paths(v: &Value, active_only: bool) -> Vec<Vec<String>> {
    let yaml: serde_yaml_ng::Value = serde_yaml_ng::to_value(v).expect("JSON reads as YAML");
    cpa_core::removed::setting_paths(&yaml, active_only)
}

/// `v` without the removed settings, whatever their value. Values only: for a recorded
/// file or response compared value for value.
pub fn without(v: &Value) -> Value {
    let mut out = v.clone();
    for path in paths(v, false) {
        remove(&mut out, &path);
    }
    out
}

fn remove(v: &mut Value, path: &[String]) {
    let (last, parents) = path.split_last().unwrap();
    if let Some(Value::Object(map)) = parents.iter().try_fold(v, |node, key| node.get_mut(key.as_str())) {
        map.shift_remove(last.as_str());
    }
}

/// The removed settings an answer of this build holds with a value that would do
/// something, unless it is the value `defaults` holds at that path (the v0 view always
/// shows Go's defaults, `ws-auth: true` included). Any such path is a removed setting
/// leaking back, which normalization must not hide.
pub fn leaks(v: &Value, defaults: Option<&Value>) -> Vec<String> {
    paths(v, true)
        .into_iter()
        .filter(|path| {
            let at = |doc: &Value| path.iter().try_fold(doc, |node, key| node.get(key.as_str())).cloned();
            defaults.is_none_or(|d| at(d) != at(v))
        })
        .map(|path| path.join("."))
        .collect()
}

/// A JSON text without the removed settings, every other byte as written (escapes,
/// member order, spacing). Text that is not a JSON object is returned as it is. Panics
/// if the edit changed anything but those members.
pub fn without_json(text: &str) -> String {
    let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(text) else {
        return text.to_owned();
    };
    let removed = paths(&v, false);
    if removed.is_empty() {
        return text.to_owned();
    }
    let mut out = text.as_bytes().to_vec();
    for path in &removed {
        let path: Vec<String> = path.iter().map(|k| escape(k)).collect();
        assert!(
            cpa_common::json::delete(&mut out, &path.join(".")),
            "{path:?}: not deleted"
        );
    }
    let out = String::from_utf8(out).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&out).ok(),
        Some(without(&v)),
        "deleting {removed:?} changed more than those members:\n{text}\n->\n{out}"
    );
    out
}

/// A key as one gjson path component: every byte but ASCII letters, digits, `-` and `_`
/// escaped.
fn escape(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for c in key.chars() {
        if !(c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A Go config YAML (a recorded file or upload) without the removed settings that hold a
/// value, so it loads here; see `cpa_core::removed::strip_settings`.
pub fn without_yaml(yaml: &str) -> String {
    cpa_core::removed::strip_settings(yaml)
}

/// Whether a management answer is this build's refusal of a removed provider's setting
/// or operation: a 4xx with the shared guidance, and for a removed route (410) the
/// stable `removed_provider` code.
pub fn is_refusal(status: u16, body: &str) -> bool {
    let v: Value = serde_json::from_str(body).unwrap_or_default();
    let text = |v: &Value| v.as_str().is_some_and(|s| s.contains(cpa_core::removed::GUIDANCE));
    let guided = text(&v["error"]) || text(&v["message"]);
    match status {
        410 => guided && v["code"] == "removed_provider",
        400..=499 => guided,
        _ => false,
    }
}

/// With `CPA_COVERAGE_INVENTORY` set, one line per replayed case and how it was
/// covered, for the per-case coverage inventory; nothing otherwise.
pub fn inventory(suite: &str, case: &str, class: &str) {
    if std::env::var_os("CPA_COVERAGE_INVENTORY").is_some() {
        eprintln!("coverage\t{suite}\t{case}\t{class}");
    }
}

#[test]
fn normalizers_keep_unrelated_user_data() {
    // Go's recorded file with a removed setting and user data that only looks like one.
    let recorded = r#"{"metadata":{"gemini":"user-value","protocols":["x","_generate-content","_interactions"]},"note":"keep {gemini: user-data} \u003c","requests":{"payload":{"override":[{"params":{"metadata":{"kimi":"keep"}}}]}},"gemini-api-key":[{"api-key":"k"}],"oauth-excluded-models":{"claude":["a"],"kimi.com":["b"]},"api-keys":{"gemini":[],"claude":[]},"ws-auth":true}"#;
    let kept = without_json(recorded);
    assert_eq!(
        kept,
        r#"{"metadata":{"gemini":"user-value","protocols":["x","_generate-content","_interactions"]},"note":"keep {gemini: user-data} \u003c","requests":{"payload":{"override":[{"params":{"metadata":{"kimi":"keep"}}}]}},"oauth-excluded-models":{"claude":["a"]},"api-keys":{"claude":[]}}"#
    );
    // Without a removed setting nothing is rewritten, not even escapes.
    assert_eq!(without_json(&kept), kept);
    let v: Value = serde_json::from_str(recorded).unwrap();
    assert_eq!(without(&v), serde_json::from_str::<Value>(&kept).unwrap());
    assert_eq!(
        leaks(&v, None),
        ["gemini-api-key", "oauth-excluded-models.kimi.com", "ws-auth"]
    );
    let defaults = serde_json::json!({"ws-auth": true});
    assert_eq!(
        leaks(&v, Some(&defaults)),
        ["gemini-api-key", "oauth-excluded-models.kimi.com"]
    );
    assert_eq!(
        without_json("not json {\"ws-auth\":true}"),
        "not json {\"ws-auth\":true}"
    );
}
