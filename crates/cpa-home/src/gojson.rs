//! A small writer for the flat JSON objects Home receives as RESP keys and payloads:
//! fields in the order written, `,omitempty` options left out, and map keys sorted.
//! Home parses them as JSON, so escaping and member order are not wire requirements.
//! The output for given fields is deterministic, and the in-flight packer measures
//! these exact bytes against Home's part, revision and count limits
//! (`inflight::pack`), so a part it sends is the size it checked. Strings are
//! HTML-escaped, as Go's `json.Marshal` writes them.

use std::collections::BTreeMap;

pub(crate) struct Object {
    out: Vec<u8>,
    first: bool,
}

impl Object {
    pub(crate) fn new() -> Self {
        Self {
            out: vec![b'{'],
            first: true,
        }
    }

    fn key(&mut self, key: &str) -> &mut Vec<u8> {
        if !self.first {
            self.out.push(b',');
        }
        self.first = false;
        string_into(&mut self.out, key.as_bytes());
        self.out.push(b':');
        &mut self.out
    }

    pub(crate) fn str(mut self, key: &str, value: &str) -> Self {
        string_into(self.key(key), value.as_bytes());
        self
    }

    /// `,omitempty` string.
    pub(crate) fn str_opt(self, key: &str, value: &str) -> Self {
        if value.is_empty() { self } else { self.str(key, value) }
    }

    pub(crate) fn int(mut self, key: &str, value: i64) -> Self {
        let text = value.to_string();
        self.key(key).extend_from_slice(text.as_bytes());
        self
    }

    pub(crate) fn raw(mut self, key: &str, raw: &[u8]) -> Self {
        self.key(key).extend_from_slice(raw);
        self
    }

    /// `map[string]string,omitempty`.
    pub(crate) fn map_opt(self, key: &str, map: &BTreeMap<String, String>) -> Self {
        if map.is_empty() {
            return self;
        }
        let mut inner = Object::new();
        for (k, v) in map {
            inner = inner.str(k, v);
        }
        self.raw(key, &inner.finish())
    }

    pub(crate) fn strs(self, key: &str, items: &[String]) -> Self {
        let mut raw = vec![b'['];
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                raw.push(b',');
            }
            string_into(&mut raw, item.as_bytes());
        }
        raw.push(b']');
        self.raw(key, &raw)
    }

    pub(crate) fn finish(mut self) -> Vec<u8> {
        self.out.push(b'}');
        self.out
    }
}

/// A JSON string, HTML-escaped.
pub(crate) fn string_into(out: &mut Vec<u8>, value: &[u8]) {
    cpa_common::json::marshal_str(out, value, true);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_writer_preserves_json_values_and_omits_empty_options() {
        let map = BTreeMap::from([("b".to_owned(), "2".to_owned()), ("a".to_owned(), "<1>".to_owned())]);
        let encoded = Object::new()
            .str("s", "x&y")
            .str_opt("empty", "")
            .int("n", -3)
            .map_opt("m", &map)
            .strs("l", &["\u{2028}".to_owned()])
            .finish();
        // Home consumes JSON values. Escaping and map iteration order are not wire requirements.
        let actual: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(
            actual,
            serde_json::json!({"s":"x&y", "n":-3, "m":{"a":"<1>","b":"2"}, "l":["\u{2028}"]})
        );
        assert!(actual.get("empty").is_none());
    }
}
