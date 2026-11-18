//! `String` facade over `crate::protocol::json` (Go's tidwall/gjson, tidwall/sjson v1.2.5 and
//! encoding/json, owned by the translator thread) for the Codex modules and the
//! Responses WebSocket handler. Edits touch only the bytes Go touches: untouched members
//! keep their order, spacing and number spelling. An edit sjson rejects leaves the input
//! unchanged, as Go callers that ignore the error do.

use crate::protocol::json as gj;
use crate::protocol::rawjson::{self, Kind};

fn edited(json: &str, edit: impl FnOnce(&mut Vec<u8>) -> bool) -> String {
    let mut out = json.as_bytes().to_vec();
    if !edit(&mut out) {
        return json.to_owned();
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// `sjson.SetRawBytes`.
pub fn set_raw(json: &str, path: &str, raw: &str) -> String {
    edited(json, |out| gj::set_raw(out, path, raw))
}

/// `sjson.SetBytes` with a string value.
pub fn set_str(json: &str, path: &str, value: &str) -> String {
    edited(json, |out| gj::set_str(out, path, value))
}

/// `sjson.DeleteBytes`.
pub fn delete(json: &str, path: &str) -> String {
    edited(json, |out| gj::delete(out, path))
}

/// `helps.SetStringIfDifferent`.
pub fn set_str_if_different(json: String, path: &str, value: &str) -> String {
    let current = rawjson::get(&json, path);
    if current.kind() == Kind::String && current.str() == value {
        return json;
    }
    set_str(&json, path, value)
}

/// `helps.SetBoolIfDifferent`.
pub fn set_bool_if_different(json: String, path: &str, value: bool) -> String {
    let kind = rawjson::get(&json, path).kind();
    if (value && kind == Kind::True) || (!value && kind == Kind::False) {
        return json;
    }
    set_raw(&json, path, if value { "true" } else { "false" })
}

/// encoding/json string encoding; `escape_html` as `json.Marshal` does.
pub fn go_quote(s: &str, escape_html: bool) -> String {
    let mut out = Vec::with_capacity(s.len() + 2);
    gj::marshal_str(&mut out, s.as_bytes(), escape_html);
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// `json.Unmarshal` into `map[string]json.RawMessage` followed by `json.Marshal`: sorted
/// unescaped keys (last duplicate wins), compacted HTML-escaped values. `None` when Go's
/// unmarshal would fail or yield a nil map.
pub fn go_remarshal_object(
    json: &str,
    edit: impl FnOnce(&mut std::collections::BTreeMap<String, String>) -> bool,
) -> Option<String> {
    if !rawjson::valid(json) {
        return None;
    }
    let root = rawjson::parse(json);
    if root.kind() != Kind::Object {
        return None;
    }
    let mut members = std::collections::BTreeMap::new();
    root.each(|key, value| {
        members.insert(key.str().to_owned(), value.json().to_owned());
        true
    });
    if !edit(&mut members) {
        return None;
    }
    let mut out = String::with_capacity(json.len());
    out.push('{');
    for (i, (key, value)) in members.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&go_quote(key, true));
        out.push(':');
        out.push_str(&String::from_utf8_lossy(&gj::compact(value.as_bytes(), true)));
    }
    out.push('}');
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn value(raw: &str) -> Value {
        serde_json::from_str(raw).expect("edited document is valid JSON")
    }

    #[test]
    fn deleting_fields_preserves_remaining_values_and_missing_paths_are_noops() {
        for input in [r#"{"a":1, "b":2}"#, "{\n  \"a\": 1,\n  \"b\": 2\n}"] {
            assert_eq!(value(&delete(input, "b")), json!({"a": 1}));
            assert_eq!(value(&delete(input, "a")), json!({"b": 2}));
            assert_eq!(delete(input, "missing"), input);
        }
        assert_eq!(value(&delete(r#"{"only":true}"#, "only")), json!({}));
        assert_eq!(value(&delete(r#"{"x":{"a":1,"b":2}}"#, "x.a")), json!({"x":{"b":2}}));
        let input = r#"{ "x": {"a":1}, "opaque": 1.2300e+04 }"#;
        assert_eq!(delete(input, "x.missing"), input);
        let output = delete(input, "x.a");
        assert!(output.contains(r#""opaque": 1.2300e+04"#), "untouched number bytes");
    }

    #[test]
    fn setting_fields_changes_only_the_target_and_supports_nested_paths() {
        let input = r#"{ "a": 1 , "untouched": { "n": 1.2300e+04 } }"#;
        let output = set_raw(input, "a", "true");
        assert_eq!(value(&output), json!({"a":true,"untouched":{"n":12300.0}}));
        assert!(output.contains(r#""untouched": { "n": 1.2300e+04 }"#));
        for (input, path, raw, expected) in [
            (r#"{"a":1}"#, "b", "2", json!({"a":1,"b":2})),
            ("{}", "b", "2", json!({"b":2})),
            ("{\"a\":1}\n", "b", "2", json!({"a":1,"b":2})),
            (r#"{"a":1}"#, "x.y", "2", json!({"a":1,"x":{"y":2}})),
            (r#"{"t":[1]}"#, "t.-1", "2", json!({"t":[1,2]})),
            (r#"{"t":[]}"#, "t.-1", "2", json!({"t":[2]})),
            (r#"{"t":[1]}"#, "t.0", "9", json!({"t":[9]})),
            (r#"{"t":[1]}"#, "t.3", "9", json!({"t":[1,null,null,9]})),
            (r#"{"a":1}"#, "p.0.q", "2", json!({"a":1,"p":[{"q":2}]})),
            (r#"{"a.b":1}"#, r"a\.b", "2", json!({"a.b":2})),
        ] {
            assert_eq!(value(&set_raw(input, path, raw)), expected, "{input} {path}");
        }
    }

    #[test]
    fn inserted_strings_round_trip_without_changing_other_bytes() {
        // RFC 8259 permits equivalent escape spellings. The value and untouched
        // request spans are the contract, including control characters and Unicode.
        let input = r#"{ "untouched": 1.2300e+04 }"#;
        for text in ["a<b", "a<b\n", "é\u{2028}", "\u{1}\u{8}\u{c}", "\"\\"] {
            let output = set_str(input, "v", text);
            assert_eq!(value(&output)["v"], text);
            assert!(output.contains(r#""untouched": 1.2300e+04"#));
            for escape_html in [false, true] {
                assert_eq!(value(&go_quote(text, escape_html)), text);
            }
        }
    }

    #[test]
    fn object_reencoding_preserves_values_and_refuses_invalid_inputs_or_aborted_edits() {
        let input = r#"{"z": [1, 2], "ab": "<x>", "nested": {"k": 1}}"#;
        let output = go_remarshal_object(input, |members| {
            members.insert("added".into(), "true".into());
            true
        })
        .unwrap();
        assert_eq!(
            value(&output),
            json!({"z":[1,2],"ab":"<x>","nested":{"k":1},"added":true})
        );
        for invalid in ["[1]", "null", r#"{"a":1} x"#, "{"] {
            assert!(go_remarshal_object(invalid, |_| true).is_none(), "{invalid}");
        }
        assert!(go_remarshal_object(input, |_| false).is_none());
    }
}
