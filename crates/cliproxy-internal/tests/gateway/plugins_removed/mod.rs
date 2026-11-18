//! Go recordings made before native plugin removal (docs/DIFFERENCES-FROM-GO.md "Native
//! plugins"): what a golden test drops from them so the rest still compares exactly.
// Each test binary uses part of this module.
#![allow(dead_code)]

/// Native plugins are removed (docs/DIFFERENCES-FROM-GO.md "Native plugins"): a
/// recording's top-level `plugins` section is dropped from the YAML it starts from.
pub fn without_plugins_yaml(yaml: &str) -> String {
    let mut out = String::with_capacity(yaml.len());
    let mut skipping = false;
    for line in yaml.split_inclusive('\n') {
        if line.starts_with("plugins:") {
            skipping = true;
            continue;
        }
        if skipping && (line.starts_with([' ', '\t']) || line.trim().is_empty()) {
            continue;
        }
        skipping = false;
        out.push_str(line);
    }
    out
}

/// ... and the top-level `"plugins"` member from Go's JSON answers, every other byte as
/// Go wrote it.
pub fn without_plugins_json(raw: &str) -> String {
    const KEY: &str = "\"plugins\":";
    let bytes = raw.as_bytes();
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    for (i, &b) in bytes.iter().enumerate() {
        if in_string {
            match (escaped, b) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' if depth == 1 && raw[i..].starts_with(KEY) => {
                let end = value_end(bytes, i + KEY.len());
                let (from, to) = if raw[..i].ends_with(',') {
                    (i - 1, end)
                } else if raw[end..].starts_with(',') {
                    (i, end + 1)
                } else {
                    (i, end)
                };
                return format!("{}{}", &raw[..from], &raw[to..]);
            }
            b'"' => in_string = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    raw.to_owned()
}

/// Where the JSON value starting at `start` ends.
fn value_end(bytes: &[u8], start: usize) -> usize {
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            match (escaped, b) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => {
                    in_string = false;
                    if depth == 0 {
                        return i + 1;
                    }
                }
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' if depth == 0 => return i,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            b',' if depth == 0 => return i,
            _ => {}
        }
    }
    bytes.len()
}

#[test]
fn recorded_plugins_sections_are_dropped_byte_for_byte() {
    assert_eq!(
        without_plugins_json(r#"{"a":1,"plugins":{"enabled":false,"dir":"plugins","configs":{}},"b":"plugins"}"#),
        r#"{"a":1,"b":"plugins"}"#
    );
    assert_eq!(without_plugins_json(r#"{"plugins":null,"b":2}"#), r#"{"b":2}"#);
    assert_eq!(without_plugins_json(r#"{"x":{"plugins":1}}"#), r#"{"x":{"plugins":1}}"#);
    assert_eq!(
        without_plugins_yaml("a: 1\nplugins:\n  enabled: false\n  dir: ./p\nb: 2\n"),
        "a: 1\nb: 2\n"
    );
}

/// [`without_plugins_json`] for a decoded value: the top-level `plugins` member removed.
pub fn without_plugins_value(value: &serde_json::Value) -> serde_json::Value {
    let mut value = value.clone();
    if let Some(object) = value.as_object_mut() {
        object.shift_remove("plugins");
    }
    value
}
