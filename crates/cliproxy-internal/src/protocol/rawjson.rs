//! The [`crate::protocol::json`] reader and editor behind `&str` signatures.
//!
//! Claude request rewriting edits through it. CCH signs the final body and prompt
//! caching keys on its prefix, so for a given request the rewritten bytes must stay
//! the same from one release to the next: a change to what an edit writes is a cache
//! semantics change and needs the live cache-read and cache-write check in AGENTS.md.
//! Matching CLIProxyAPI's bytes is not the requirement; keeping the current, signed and
//! cached bytes stable is. The Codex observers (failure classification, quota headers,
//! WebSocket bookkeeping, the Images adapter and Alpha Search) and the shared Responses
//! WebSocket handler read through it too. Reading never changes the document: callers
//! forward the bytes they inspected.
//!
//! Lookup rules that callers depend on: a path that is missing under the first of
//! duplicate keys moves on to a later member, a lookup into text that does not start
//! with JSON finds the first `{` or `[`, [`Value::str`] renders an integer with its
//! literal digits and another number as its shortest decimal without an exponent, and
//! `bool()`, `int()` and `float()` coerce as [`crate::protocol::json::Res`] does.
//! [`Value::array`] treats a non-array as a one-element list and null as empty;
//! [`Value::items`] is the elements of an array and nothing otherwise.

use std::borrow::Cow;

use crate::protocol::json::{self, Res};
/// JSON kinds exposed by the reader. This adapter does not use the third-party gjson
/// crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Null,
    False,
    True,
    Number,
    String,
    Array,
    Object,
}

/// A gjson `Result` with `&str` accessors.
#[derive(Clone, Debug, Default)]
pub struct Value<'a> {
    res: Res<'a>,
    raw: Cow<'a, str>,
    /// `Result.String()` when it is not the unescaped string `res.s` already holds
    /// owned; `str` reads that one in place rather than copying it.
    s: Option<Cow<'a, str>>,
}

/// `bytes` as text, borrowed from `src` when it is a slice of it.
fn as_str<'a>(src: &'a str, bytes: Cow<'a, [u8]>) -> Cow<'a, str> {
    match bytes {
        Cow::Borrowed(b) => {
            let start = (b.as_ptr() as usize).wrapping_sub(src.as_ptr() as usize);
            match src.get(start..start.wrapping_add(b.len())) {
                Some(slice) if slice.as_ptr() == b.as_ptr() => Cow::Borrowed(slice),
                _ => String::from_utf8_lossy(b),
            }
        }
        Cow::Owned(v) => {
            Cow::Owned(String::from_utf8(v).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
        }
    }
}

impl<'a> Value<'a> {
    fn new(src: &'a str, res: Res<'a>) -> Self {
        let raw = as_str(src, res.raw.clone());
        let s = match (&res.kind, &res.s) {
            (json::Kind::String, Cow::Owned(_)) => None,
            _ => Some(as_str(src, res.bytes())),
        };
        Value { res, raw, s }
    }

    fn src(&self) -> &'a str {
        match &self.raw {
            Cow::Borrowed(raw) => raw,
            Cow::Owned(_) => "",
        }
    }

    /// `Result.Get`.
    pub fn get(&self, path: &str) -> Value<'a> {
        Value::new(self.src(), self.res.get(path))
    }

    pub fn exists(&self) -> bool {
        self.res.exists()
    }

    pub fn kind(&self) -> Kind {
        match self.res.kind {
            json::Kind::Null => Kind::Null,
            json::Kind::False => Kind::False,
            json::Kind::True => Kind::True,
            json::Kind::Number => Kind::Number,
            json::Kind::String => Kind::String,
            json::Kind::Json if self.res.raw().first() == Some(&b'[') => Kind::Array,
            json::Kind::Json => Kind::Object,
        }
    }

    /// The raw JSON.
    pub fn json(&self) -> &str {
        &self.raw
    }

    /// `Result.String()`.
    pub fn str(&self) -> &str {
        match &self.s {
            Some(s) => s,
            // Unescaped from valid UTF-8 (lone surrogates become U+FFFD), so this holds.
            None => std::str::from_utf8(&self.res.s).unwrap_or_default(),
        }
    }

    /// `Result.Int()`.
    pub fn i64(&self) -> i64 {
        self.res.int()
    }

    /// `Result.Float()`.
    pub fn f64(&self) -> f64 {
        self.res.float()
    }

    /// `Result.Bool()`.
    pub fn bool(&self) -> bool {
        self.res.bool()
    }

    /// `Result.ForEach`.
    pub fn each(&self, mut f: impl FnMut(Value<'a>, Value<'a>) -> bool) {
        let src = self.src();
        self.res.each(|k, v| f(Value::new(src, k), Value::new(src, v)));
    }

    /// `Result.Array()`: one allocation, as the gjson crate made, rather than one for
    /// the port's results and another for these.
    pub fn array(&self) -> Vec<Value<'a>> {
        let src = self.src();
        if self.kind() != Kind::Array {
            // Nothing for null, else the value itself.
            return self.res.array().into_iter().map(|v| Value::new(src, v)).collect();
        }
        let mut n = 0;
        self.res.each(|_, _| {
            n += 1;
            true
        });
        let mut out = Vec::with_capacity(n);
        self.res.each(|_, v| {
            out.push(Value::new(src, v));
            true
        });
        out
    }

    /// `if v.IsArray() { range v.Array() }`: the elements of an array, else nothing.
    pub fn items(&self) -> Vec<Value<'a>> {
        if self.kind() == Kind::Array {
            self.array()
        } else {
            Vec::new()
        }
    }

    /// The values `Result.ForEach` visits: array elements, an object's member values
    /// (duplicates included), a scalar itself, nothing when missing.
    pub fn values(&self) -> Vec<Value<'a>> {
        if self.kind() == Kind::Array {
            return self.array();
        }
        let mut out = Vec::new();
        self.each(|_, v| {
            out.push(v);
            true
        });
        out
    }

    /// `len(Result.Map())`: distinct member keys of an object, else 0.
    pub fn map_len(&self) -> usize {
        self.res.map().len()
    }
}

/// `gjson.Get`.
pub fn get<'a>(json: &'a str, path: &str) -> Value<'a> {
    Value::new(json, json::get(json.as_bytes(), path))
}

/// `gjson.Parse`.
pub fn parse(json: &str) -> Value<'_> {
    Value::new(json, json::parse(json.as_bytes()))
}

/// `gjson.Valid`.
pub fn valid(json: &str) -> bool {
    json::valid(json.as_bytes())
}

/// Byte offset of a value that `get` borrowed from `json`.
pub fn offset(json: &str, value: &Value<'_>) -> Option<usize> {
    let raw = value.json();
    let start = (raw.as_ptr() as usize).checked_sub(json.as_ptr() as usize)?;
    (!raw.is_empty() && start + raw.len() <= json.len() && &json[start..start + raw.len()] == raw).then_some(start)
}

/// A JSON string with `<`, `>`, `&`, U+2028 and U+2029 escaped, as the existing Claude
/// rewrites write set strings.
pub fn go_string(s: &str) -> String {
    marshal(s, true)
}

/// `json.Encoder` with `SetEscapeHTML(false)`, i.e. `JSON.stringify` for these inputs.
pub fn js_string(s: &str) -> String {
    marshal(s, false)
}

fn marshal(s: &str, html: bool) -> String {
    let mut out = Vec::with_capacity(s.len() + 2);
    json::marshal_str(&mut out, s.as_bytes(), html);
    text(out)
}

/// The edits below never break UTF-8 in a UTF-8 document; the fallback is unreachable.
fn text(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// `sjson.SetRawBytes`; an sjson error leaves the document unchanged.
pub fn set_raw(json: &str, path: &str, raw: &str) -> String {
    json::try_set_raw(json.as_bytes(), path, raw).map_or_else(|_| json.to_owned(), text)
}

/// `sjson.SetBytes` with a Go string value.
pub fn set_str(json: &str, path: &str, value: &str) -> String {
    json::try_set_str(json.as_bytes(), path, value).map_or_else(|_| json.to_owned(), text)
}

/// `sjson.DeleteBytes`.
pub fn delete(json: &str, path: &str) -> String {
    json::try_delete(json.as_bytes(), path).map_or_else(|_| json.to_owned(), text)
}

/// [`delete`] of an owned document, which comes back as it was, not copied, when
/// nothing changes.
pub fn delete_owned(json: String, path: &str) -> String {
    json::delete_changed(json.as_bytes(), path).map_or(json, text)
}

/// `gjson.Result.String()` on a lookup, owned.
pub fn string(json: &str, path: &str) -> String {
    get(json, path).str().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value as Json, json};
    #[test]
    fn reads_typed_fields_without_changing_numeric_or_string_wire_spelling() {
        let body = r#"{"type":"response.completed","n":9007199254740993,"fraction":1.2300e+04,"name":"u\u0031","items":[1,{"id":"two"}],"nil":null}"#;
        assert_eq!(get(body, "type").str(), "response.completed");
        assert_eq!(get(body, "n").json(), "9007199254740993");
        assert_eq!(get(body, "fraction").json(), "1.2300e+04");
        // Text rendering: integer digits as written, other numbers in plain decimal.
        assert_eq!(get(body, "n").str(), "9007199254740993");
        assert_eq!(get(body, "fraction").str(), "12300");
        assert_eq!(get(r#"{"f":1.5e-7}"#, "f").str(), "0.00000015");
        assert_eq!(get(body, "name").str(), "u1");
        assert_eq!(get(body, "name").json(), r#""u\u0031""#);
        assert_eq!(get(body, "items.1.id").str(), "two");
        assert_eq!(get(body, "items").items().len(), 2);
        assert!(get(body, "nil").exists());
        assert!(!get(body, "missing").exists());
        assert!(get(body, "type").items().is_empty());
    }
    #[test]
    fn edits_target_fields_preserve_other_spans_and_keep_valid_json() {
        let input = r#"{ "keep" : {"n":9007199254740993,"s":"\u0061"}, "a":{"x":1}, "items":[1,2,3] }"#;
        for (path, replacement) in [
            ("a.x", json!(true)),
            ("new.nested", json!("é<&>")),
            ("items.1", json!({"k":3})),
        ] {
            let output = set_raw(input, path, &replacement.to_string());
            let parsed: Json = serde_json::from_str(&output).unwrap();
            assert!(parsed.is_object());
            assert_eq!(
                serde_json::from_str::<Json>(get(&output, path).json()).unwrap(),
                replacement
            );
            assert_eq!(get(&output, "keep").json(), get(input, "keep").json());
        }
        let output = delete(input, "items.1");
        assert_eq!(serde_json::from_str::<Json>(&output).unwrap()["items"], json!([1, 3]));
        assert_eq!(delete(input, "missing"), input);
        assert_eq!(get(&set_str(input, "text", "é<&>\n\""), "text").str(), "é<&>\n\"");
        let encoded = js_string("a\u{2028}<\u{1}");
        assert_eq!(serde_json::from_str::<String>(&encoded).unwrap(), "a\u{2028}<\u{1}");
    }
}
