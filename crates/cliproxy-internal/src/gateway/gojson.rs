//! Server-side helpers for the bodies CLIProxyAPI builds itself (error envelopes, model
//! lists): Go struct-ordered objects ([`Obj`]), status texts and durations. Go's JSON
//! encoding itself comes from `crate::protocol::json`.

use serde_json::Value;

/// A Go JSON string literal (`json.Marshal(string)`).
pub fn string(s: &str) -> String {
    String::from_utf8(crate::protocol::json::quote(s)).expect("valid UTF-8 in, valid UTF-8 out")
}

/// sjson's string encoding: plain quoting unless a byte needs escaping, then
/// `json.Marshal` (which also HTML-escapes).
pub fn sjson_string(s: &str) -> String {
    if s.bytes()
        .any(|b| !(b' '..=0x7f).contains(&b) || b == b'"' || b == b'\\')
    {
        string(s)
    } else {
        format!("\"{s}\"")
    }
}

/// `json.Marshal` of a value decoded into `map[string]any`: object keys sorted, and a
/// float written as Go writes a float64 (`1`, `-0`, `100000000000000000000`, `1e+21`).
pub fn sorted(v: &Value) -> String {
    use crate::protocol::json::GoValue;
    fn go(v: &Value) -> GoValue {
        match v {
            Value::Number(n) if n.is_f64() => GoValue::Number(
                n.as_f64()
                    .and_then(crate::protocol::json::json_float)
                    .unwrap_or_else(|| n.to_string()),
            ),
            Value::Array(items) => GoValue::Array(items.iter().map(go).collect()),
            Value::Object(map) => GoValue::Object(map.iter().map(|(k, v)| (k.clone(), go(v))).collect()),
            other => GoValue::from_json(other),
        }
    }
    String::from_utf8(go(v).marshal()).expect("valid UTF-8 in, valid UTF-8 out")
}

/// An ordered object, for Go structs: fields render in insertion order.
#[derive(Default)]
pub struct Obj(String);

impl Obj {
    pub fn new() -> Self {
        Self::default()
    }

    /// A field whose value is already JSON.
    pub fn raw(mut self, key: &str, json: &str) -> Self {
        self.0.push(if self.0.is_empty() { '{' } else { ',' });
        self.0.push_str(&string(key));
        self.0.push(':');
        self.0.push_str(json);
        self
    }

    pub fn str(self, key: &str, value: &str) -> Self {
        let value = string(value);
        self.raw(key, &value)
    }

    pub fn finish(mut self) -> String {
        if self.0.is_empty() {
            self.0.push('{');
        }
        self.0.push('}');
        self.0
    }
}

/// gjson `Result.String()` for a decoded value: strings verbatim, other scalars as
/// their JSON text, objects and arrays as compact JSON, null or missing as empty.
pub fn gjson_string(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// Go `strings.TrimSpace`.
pub fn trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace())
}

/// Go `http.StatusText`.
pub fn status_text(status: u16) -> &'static str {
    match status {
        499 => "",
        _ => axum::http::StatusCode::from_u16(status)
            .ok()
            .and_then(|s| s.canonical_reason())
            .unwrap_or_default(),
    }
}

/// Go `time.Duration.String()` for non-negative durations.
pub fn duration(d: std::time::Duration) -> String {
    let nanos = d.as_nanos();
    if nanos == 0 {
        return "0s".into();
    }
    if nanos < 1_000_000_000 {
        let (unit, div) = if nanos < 1_000 {
            ("ns", 1u128)
        } else if nanos < 1_000_000 {
            ("µs", 1_000)
        } else {
            ("ms", 1_000_000)
        };
        return format!("{}{unit}", fraction(nanos, div));
    }
    let hours = nanos / 3_600_000_000_000;
    let minutes = nanos / 60_000_000_000 % 60;
    let seconds = nanos % 60_000_000_000;
    let mut out = String::new();
    if hours > 0 {
        out.push_str(&format!("{hours}h"));
    }
    if hours > 0 || minutes > 0 {
        out.push_str(&format!("{minutes}m"));
    }
    out.push_str(&format!("{}s", fraction(seconds, 1_000_000_000)));
    out
}

fn fraction(value: u128, div: u128) -> String {
    let whole = value / div;
    let mut rest = value % div;
    if rest == 0 {
        return whole.to_string();
    }
    let mut digits = String::new();
    let mut scale = div / 10;
    while rest > 0 && scale > 0 {
        digits.push(char::from(b'0' + (rest / scale) as u8));
        rest %= scale;
        scale /= 10;
    }
    format!("{whole}.{digits}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn generated_json_preserves_values_and_escapes_controls() {
        for value in ["a<b>&\"\u{2028}\u{1}", "q\"é", "plain <b>", "\r\n\t\\"] {
            for encoded in [string(value), sjson_string(value)] {
                assert_eq!(serde_json::from_str::<String>(&encoded).unwrap(), value);
                assert!(!encoded.contains('\n'), "a string cannot create a new JSON line");
                assert!(!encoded.contains('\r'));
            }
        }
        let value = serde_json::json!({"z": 1, "a": {"y": [true, null], "b": "x"}});
        assert_eq!(serde_json::from_str::<Value>(&sorted(&value)).unwrap(), value);
        let encoded = Obj::new().str("message", "m<&>").str("type", "t").finish();
        assert_eq!(
            serde_json::from_str::<Value>(&encoded).unwrap(),
            serde_json::json!({"message":"m<&>","type":"t"})
        );
    }

    #[test]
    fn displayed_durations_round_trip_without_losing_precision() {
        for value in [
            Duration::ZERO,
            Duration::from_nanos(1),
            Duration::from_micros(3),
            Duration::from_millis(250),
            Duration::from_millis(1500),
            Duration::from_secs(60),
            Duration::from_secs(5400),
            Duration::from_secs(43200),
        ] {
            let rendered = duration(value);
            assert_eq!(
                crate::config::parse_duration(&rendered),
                Some(i64::try_from(value.as_nanos()).unwrap()),
                "{rendered}"
            );
        }
    }

    #[test]
    fn diagnostic_values_are_readable_and_missing_values_are_empty() {
        let value = serde_json::json!({"text": "x", "number": 5, "flag": true, "none": null, "object": {"k": 1}});
        for (field, expected) in [
            ("text", "x"),
            ("number", "5"),
            ("flag", "true"),
            ("none", ""),
            ("missing", ""),
        ] {
            assert_eq!(gjson_string(value.get(field)), expected);
        }
        assert_eq!(
            serde_json::from_str::<Value>(&gjson_string(value.get("object"))).unwrap(),
            value["object"]
        );
    }
}
