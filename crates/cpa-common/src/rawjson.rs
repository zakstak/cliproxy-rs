//! The Go-compatible JSON reader and editor behind `&str` signatures: tidwall/gjson
//! v1.18.0, tidwall/sjson v1.2.5 and Go encoding/json through [`crate::json`].
//!
//! Claude request rewriting must reproduce Go's bytes exactly: CCH signs the final
//! body, and prompt caching keys on it. The Codex observers (failure classification,
//! quota headers, WebSocket bookkeeping, the Images adapter and Alpha Search) and the
//! shared Responses WebSocket handler read through it too. Reading never changes the
//! document: callers forward the bytes they inspected.
//!
//! Lookups follow Go's gjson: a path that is missing under the first of duplicate keys
//! moves on to a later member, a lookup into text that does not start with JSON finds
//! the first `{` or `[`, numbers format as Go's `Result.String()` does, and `Bool()`,
//! `Int()` and `Float()` coerce Go's way. [`Value::array`] is Go's `Array()`;
//! [`Value::items`] is the elements of an array and nothing otherwise.

use std::borrow::Cow;

use crate::json::{self, Res};
/// JSON kinds exposed by the Go-compatible reader. This adapter does not use the
/// third-party gjson crate.
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
            // Go: nothing for null, else the value itself.
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

/// Go's `encoding/json` string marshalling (HTML-escaping, U+2028/9 escaped).
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

    /// The reader semantics the executors and the Responses WebSocket handler rely on
    /// since the third-party gjson crate was removed (docs/GJSON-MIGRATION.md).
    #[test]
    fn reads_follow_go_gjson() {
        // The first of duplicate keys wins; a path missing under it moves on to a later
        // member, as Go's gjson does.
        let dup = r#"{"type":"a","type":"b","error":{"x":1},"error":{"message":"second"}}"#;
        assert_eq!(get(dup, "type").str(), "a");
        assert_eq!(get(dup, "error").json(), r#"{"x":1}"#);
        assert_eq!(get(dup, "error.message").str(), "second");
        // Raw spelling survives in `json()`; `str()` is Go's `Result.String()`.
        let nums = r#"{"a":12.50,"b":1e400,"c":123456789012345678901234567890,"d":7}"#;
        assert_eq!(get(nums, "a").json(), "12.50");
        assert_eq!(get(nums, "a").str(), "12.5");
        assert_eq!(get(nums, "b").json(), "1e400");
        assert_eq!(get(nums, "c").json(), "123456789012345678901234567890");
        assert_eq!(get(nums, "c").kind(), Kind::Number);
        assert_eq!(get(nums, "d").str(), "7");
        // Go `Int()`: integer strings parse, other strings read 0, numbers truncate.
        let ints = r#"{"s":"429","f":"429.5","n":429.9}"#;
        assert_eq!(get(ints, "s").i64(), 429);
        assert_eq!(get(ints, "f").i64(), 0);
        assert_eq!(get(ints, "n").i64(), 429);
        // `items()` is the elements of an array and nothing else; `array()` is Go's.
        let doc = r#"{"list":[1,{"k":2}],"obj":{"k":1},"null":null}"#;
        let items = get(doc, "list").items();
        assert_eq!(items.iter().map(Value::json).collect::<Vec<_>>(), ["1", r#"{"k":2}"#]);
        assert!(get(doc, "obj").items().is_empty());
        assert_eq!(get(doc, "obj").array().len(), 1);
        assert!(get(doc, "missing").items().is_empty());
        assert!(get(doc, "null").array().is_empty());
        // Escapes decode in `str()` only; a lone surrogate becomes U+FFFD; an escaped key
        // matches its decoded name.
        let esc = r#"{"k\u0065y":"a\ud800b","t":"\u0068i"}"#;
        assert_eq!(get(esc, "key").str(), "a\u{fffd}b");
        assert_eq!(get(esc, "t").str(), "hi");
        assert_eq!(get(esc, "t").json(), r#""\u0068i""#);
        // Go's lookup starts at the first `{` or `[`, also after leading text.
        assert_eq!(
            get(r#"upstream: {"error":{"message":"m"}}"#, "error.message").str(),
            "m"
        );
    }

    #[test]
    fn set_and_delete_match_sjson_byte_layout() {
        // Expected bytes from sjson v1.2.5 (see tests/reference fixtures for the
        // generated cross-check; these are the hand-traced core cases).
        assert_eq!(set_raw(r#"{"a":1 }"#, "b", "2"), r#"{"a":1 ,"b":2}"#);
        assert_eq!(set_raw("{}", "b", "2"), r#"{"b":2}"#);
        assert_eq!(set_raw(r#"{"a":{"x":1}}"#, "a.x", "true"), r#"{"a":{"x":true}}"#);
        assert_eq!(
            set_raw(r#"{"a":1}"#, "m.user_id", r#""u""#),
            r#"{"a":1,"m":{"user_id":"u"}}"#
        );
        assert_eq!(set_raw(r#"{"a":[1,2]}"#, "a.3", "9"), r#"{"a":[1,2,null,9]}"#);
        assert_eq!(set_raw(r#"{"a":[]}"#, "a.1", "9"), r#"{"a":[null,9]}"#);
        assert_eq!(delete(r#"{"a":1, "b":2,"c":3}"#, "b"), r#"{"a":1,"c":3}"#);
        assert_eq!(delete(r#"{"a":1, "b":2}"#, "a"), r#"{ "b":2}"#);
        assert_eq!(delete(r#"{ "a" : 1 }"#, "a"), r#"{ }"#);
        assert_eq!(delete(r#"{"a":[1,2,3]}"#, "a.0"), r#"{"a":[2,3]}"#);
        assert_eq!(delete(r#"{"a":[1,2,3]}"#, "a.2"), r#"{"a":[1,2]}"#);
        assert_eq!(delete(r#"{"a":1}"#, "zz"), r#"{"a":1}"#);
        assert_eq!(set_str(r#"{"a":1}"#, "t", "x<y"), r#"{"a":1,"t":"x<y"}"#);
        assert_eq!(set_str(r#"{"a":1}"#, "t", "é<"), r#"{"a":1,"t":"é\u003c"}"#);
        assert_eq!(js_string("a\u{2028}<\u{1}"), "\"a\\u2028<\\u0001\"");
    }
}
