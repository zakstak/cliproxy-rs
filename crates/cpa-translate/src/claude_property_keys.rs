//! Tool property keys Claude rejects, on the translated routes into Claude (#77).
//!
//! Claude refuses a whole request when a tool's `input_schema` declares a property key
//! outside `^[a-zA-Z0-9_.-]{1,64}$` (its 400 names the pattern). OpenAI-format clients
//! and MCP servers use keys such as `not[labels]`, and clients resend their tools every
//! turn, so one such tool fails the conversation. The Responses and Chat Completions
//! translators therefore rename each rejected key to a valid one in the schema they send
//! and in replayed tool calls, and restore the client's keys in the tool calls Claude
//! returns. The schema keeps every constraint; native Claude requests are never touched.
//!
//! Renames are per tool and flat: one Claude key per rejected client key, chosen apart
//! from every key the tool's schema declares anywhere, so restoring is a rename of
//! object keys in the call's arguments.
//! ponytail: a free-form object value (`additionalProperties`) whose key equals a
//! generated name is restored too; a schema-guided walk would avoid that if it matters.
//! ponytail: a `$ref` pointer into a renamed property keeps the client's key; rewriting
//! its `properties/<key>` segments is the upgrade if MCP schemas start using them.

use std::collections::{HashMap, HashSet};

use cpa_common::json::{self as gj, Kind, WALK_DEPTH_LIMIT};
use cpa_common::schema::{SCHEMA_MAP_KEYWORDS, SCHEMA_VALUE_KEYWORDS};

const MAX_KEY: usize = 64;

/// Whether Claude accepts `key` as an input_schema property key.
fn accepted(key: &[u8]) -> bool {
    (1..=MAX_KEY).contains(&key.len())
        && key
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// One rejected client key's renames, both ways.
#[derive(Debug, Default)]
pub(crate) struct PropertyKeys {
    to_claude: HashMap<Vec<u8>, Vec<u8>>,
    to_client: HashMap<Vec<u8>, Vec<u8>>,
}

/// A JSON string token inside the walked document: its byte range (with quotes) and its
/// decoded text.
struct Token {
    start: usize,
    end: usize,
    text: Vec<u8>,
}

/// What the schema walk found: declared property keys, and the keys and strings that
/// name properties (`required`, `dependentRequired`, `dependentSchemas`, `dependencies`).
#[derive(Default)]
struct SchemaTokens {
    keys: Vec<Token>,
    references: Vec<Token>,
}

/// Keywords whose member keys name properties (`dependencies` is the older spelling of
/// both); array members list property names.
const DEPENDENCY_KEYWORDS: [&[u8]; 3] = [b"dependentRequired", b"dependentSchemas", b"dependencies"];

impl PropertyKeys {
    /// The renames a tool's input schema needs, or `None` when Claude accepts it.
    /// Deterministic for the same schema bytes, so the cached prompt prefix stays stable.
    pub(crate) fn of(schema: &[u8]) -> Option<Self> {
        let tokens = schema_tokens(schema);
        let mut taken: HashSet<&[u8]> = HashSet::new();
        let mut seen: HashSet<&[u8]> = HashSet::new();
        let mut rejected: Vec<&[u8]> = vec![];
        for key in &tokens.keys {
            if accepted(&key.text) {
                taken.insert(&key.text);
            } else if seen.insert(&key.text) {
                rejected.push(&key.text);
            }
        }
        if rejected.is_empty() {
            return None;
        }
        let mut keys = PropertyKeys::default();
        let mut assigned: HashSet<Vec<u8>> = HashSet::new();
        // The next suffix per base, so keys sharing a base never retry the same names.
        let mut next: HashMap<Vec<u8>, usize> = HashMap::new();
        for original in rejected {
            let base = sanitize(original);
            let mut name = base.clone();
            let n = next.entry(base.clone()).or_insert(2);
            while taken.contains(&name[..]) || assigned.contains(&name) {
                let suffix = format!("_{n}");
                name = base[..base.len().min(MAX_KEY - suffix.len())].to_vec();
                name.extend_from_slice(suffix.as_bytes());
                *n += 1;
            }
            assigned.insert(name.clone());
            keys.to_claude.insert(original.to_vec(), name.clone());
            keys.to_client.insert(name, original.to_vec());
        }
        Some(keys)
    }

    /// `schema` with its rejected property keys, and the references naming them, renamed.
    pub(crate) fn schema(&self, schema: &[u8]) -> Vec<u8> {
        let tokens = schema_tokens(schema);
        let edits = tokens
            .keys
            .iter()
            .chain(&tokens.references)
            .filter_map(|t| self.to_claude.get(&t.text).map(|name| (t, name)));
        splice(schema, edits)
    }

    /// Tool-call arguments with the client's keys renamed, for history sent to Claude.
    pub(crate) fn to_claude(&self, arguments: &[u8]) -> Vec<u8> {
        rename_keys(arguments, &self.to_claude)
    }

    /// Tool-call arguments from Claude with the client's keys restored. Bytes that are
    /// not valid JSON (a call cut off mid-stream) are returned as they are.
    pub(crate) fn to_client(&self, arguments: &[u8]) -> Vec<u8> {
        rename_keys(arguments, &self.to_client)
    }
}

/// Each byte outside the accepted set becomes `_` (one per character), cut to 64 bytes.
fn sanitize(key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(key.len().min(MAX_KEY));
    for c in String::from_utf8_lossy(key).chars() {
        if out.len() == MAX_KEY {
            break;
        }
        out.push(if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
            c as u8
        } else {
            b'_'
        });
    }
    if out.is_empty() {
        out.push(b'_');
    }
    out
}

fn offset(doc: &[u8], part: &[u8]) -> usize {
    part.as_ptr() as usize - doc.as_ptr() as usize
}

/// The token of an object member's key, from [`gj::object_members`]' unquoted slice.
fn key_token(doc: &[u8], key: &[u8], escaped: bool) -> Token {
    let start = offset(doc, key) - 1;
    let text = if escaped { gj::unescape(key) } else { key.to_vec() };
    Token {
        start,
        end: start + key.len() + 2,
        text,
    }
}

/// The elements of a JSON array, as slices of it, with their kind and decoded string.
fn array_items<'a>(array: &'a [u8], mut f: impl FnMut(&'a [u8], Kind, &[u8])) {
    gj::parse(array).each(|_, item| {
        if let std::borrow::Cow::Borrowed(raw) = item.raw {
            f(raw, item.kind, &item.s);
        }
        true
    });
}

/// A string token at `raw`, a slice of `doc` that holds a JSON string.
fn string_token(doc: &[u8], raw: &[u8], text: &[u8]) -> Token {
    let start = offset(doc, raw);
    Token {
        start,
        end: start + raw.len(),
        text: text.to_vec(),
    }
}

/// Property keys and their references at the schema positions of `schema` (JSON Schema
/// applicator and definition keywords; `const`, `default`, `enum` and other data are not
/// schemas). Walks keep their open subschemas on the heap and stop at
/// [`WALK_DEPTH_LIMIT`] nested subschemas, leaving deeper ones as they are.
fn schema_tokens(schema: &[u8]) -> SchemaTokens {
    let mut tokens = SchemaTokens::default();
    if !gj::valid(schema) {
        return tokens;
    }
    let mut stack: Vec<(&[u8], usize)> = vec![(schema, 0)];
    while let Some((node, depth)) = stack.pop() {
        if depth >= WALK_DEPTH_LIMIT {
            continue;
        }
        let mut children: Vec<&[u8]> = vec![];
        gj::object_members(node, |key, escaped, value| {
            let name = if escaped { gj::unescape(key) } else { key.to_vec() };
            let object = value.first() == Some(&b'{');
            let array = value.first() == Some(&b'[');
            if name == b"properties" && object {
                gj::object_members(value, |k, e, v| {
                    tokens.keys.push(key_token(schema, k, e));
                    children.push(v);
                    true
                });
            } else if name == b"required" && array {
                array_items(value, |raw, kind, text| {
                    if kind == Kind::String {
                        tokens.references.push(string_token(schema, raw, text));
                    }
                });
            } else if DEPENDENCY_KEYWORDS.contains(&&name[..]) && object {
                gj::object_members(value, |k, e, v| {
                    tokens.references.push(key_token(schema, k, e));
                    if v.first() == Some(&b'[') {
                        array_items(v, |raw, kind, text| {
                            if kind == Kind::String {
                                tokens.references.push(string_token(schema, raw, text));
                            }
                        });
                    } else {
                        children.push(v);
                    }
                    true
                });
            } else if SCHEMA_MAP_KEYWORDS.contains(&&name[..]) && object {
                gj::object_members(value, |_, _, v| {
                    children.push(v);
                    true
                });
            } else if SCHEMA_VALUE_KEYWORDS.contains(&&name[..]) {
                if object {
                    children.push(value);
                } else if array {
                    array_items(value, |raw, _, _| children.push(raw));
                }
            }
            true
        });
        // Reversed, so the stack visits subschemas in document order.
        stack.extend(
            children
                .into_iter()
                .rev()
                .filter(|c| c.first() == Some(&b'{'))
                .map(|c| (c, depth + 1)),
        );
    }
    tokens
}

/// Every object key of `json` found in `map`, renamed; values are never touched.
fn rename_keys(json: &[u8], map: &HashMap<Vec<u8>, Vec<u8>>) -> Vec<u8> {
    if map.is_empty() || !gj::valid(json) {
        return json.to_vec();
    }
    let root = &json[json.iter().take_while(|b| b.is_ascii_whitespace()).count()..];
    let mut edits: Vec<(Token, &Vec<u8>)> = vec![];
    let mut stack: Vec<(&[u8], usize)> = vec![(root, 0)];
    while let Some((node, depth)) = stack.pop() {
        if depth >= WALK_DEPTH_LIMIT {
            continue;
        }
        let mut children: Vec<&[u8]> = vec![];
        match node.first() {
            Some(b'{') => gj::object_members(node, |key, escaped, value| {
                let token = key_token(json, key, escaped);
                if let Some(name) = map.get(&token.text) {
                    edits.push((token, name));
                }
                children.push(value);
                true
            }),
            Some(b'[') => array_items(node, |raw, _, _| children.push(raw)),
            _ => {}
        }
        stack.extend(
            children
                .into_iter()
                .filter(|c| matches!(c.first(), Some(b'{' | b'[')))
                .map(|c| (c, depth + 1)),
        );
    }
    let edits: Vec<(&Token, &Vec<u8>)> = edits.iter().map(|(t, n)| (t, *n)).collect();
    splice(json, edits.into_iter())
}

/// `doc` with each token replaced by its quoted name, in one pass.
fn splice<'a>(doc: &[u8], edits: impl Iterator<Item = (&'a Token, &'a Vec<u8>)>) -> Vec<u8> {
    let mut edits: Vec<_> = edits.collect();
    if edits.is_empty() {
        return doc.to_vec();
    }
    edits.sort_by_key(|(t, _)| t.start);
    let mut out = Vec::with_capacity(doc.len() + edits.len() * 4);
    let mut at = 0;
    for (token, name) in edits {
        out.extend_from_slice(&doc[at..token.start]);
        gj::marshal_str(&mut out, name, false);
        at = token.end;
    }
    out.extend_from_slice(&doc[at..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_matches_claudes_pattern() {
        assert!(accepted(b"a.b-c_D9"));
        assert!(accepted(&[b'x'; 64]));
        assert!(!accepted(&[b'x'; 65]));
        assert!(!accepted(b""));
        assert!(!accepted(b"not[labels]"));
        assert!(!accepted("größe".as_bytes()));
    }

    #[test]
    fn schema_data_keywords_are_not_renamed() {
        let schema = br#"{"type":"object","properties":{"a b":{"type":"string","default":{"a b":1},"enum":["a b"]}},"examples":[{"a b":2}],"required":["a b"]}"#;
        let keys = PropertyKeys::of(schema).unwrap();
        assert_eq!(
            keys.schema(schema),
            br#"{"type":"object","properties":{"a_b":{"type":"string","default":{"a b":1},"enum":["a b"]}},"examples":[{"a b":2}],"required":["a_b"]}"#
        );
    }

    #[test]
    fn subschema_positions_are_walked() {
        let schema = br#"{"$defs":{"d":{"properties":{"x[1]":{}}}},"anyOf":[{"properties":{"y z":{}}}],"items":[{"properties":{"w!":{}}}],"properties":{"properties":{"properties":{"q?":{}}}}}"#;
        let keys = PropertyKeys::of(schema).unwrap();
        assert_eq!(
            keys.schema(schema),
            br#"{"$defs":{"d":{"properties":{"x_1_":{}}}},"anyOf":[{"properties":{"y_z":{}}}],"items":[{"properties":{"w_":{}}}],"properties":{"properties":{"properties":{"q_":{}}}}}"#
        );
    }

    #[test]
    fn escaped_keys_are_decoded_and_restored_exactly() {
        let schema = br#"{"properties":{"a\"b":{},"tab\tkey":{}}}"#;
        let keys = PropertyKeys::of(schema).unwrap();
        assert_eq!(keys.schema(schema), br#"{"properties":{"a_b":{},"tab_key":{}}}"#);
        let restored = keys.to_client(br#"{"a_b":1,"tab_key":[{"a_b":2}],"s":"a_b"}"#);
        assert_eq!(restored, br#"{"a\"b":1,"tab\tkey":[{"a\"b":2}],"s":"a_b"}"#);
        assert_eq!(
            keys.to_claude(&restored),
            br#"{"a_b":1,"tab_key":[{"a_b":2}],"s":"a_b"}"#
        );
    }

    #[test]
    fn dependency_keywords_follow_renames() {
        let schema = br#"{"properties":{"a b":{},"c":{}},"dependentRequired":{"a b":["c"],"c":["a b"]},"dependentSchemas":{"a b":{"required":["a b"]}},"dependencies":{"c":["a b"]}}"#;
        let keys = PropertyKeys::of(schema).unwrap();
        assert_eq!(
            keys.schema(schema),
            br#"{"properties":{"a_b":{},"c":{}},"dependentRequired":{"a_b":["c"],"c":["a_b"]},"dependentSchemas":{"a_b":{"required":["a_b"]}},"dependencies":{"c":["a_b"]}}"#
        );
    }

    #[test]
    fn leading_whitespace_does_not_skip_restoration() {
        let keys = PropertyKeys::of(br#"{"properties":{"a b":{}}}"#).unwrap();
        assert_eq!(keys.to_client(b" \n{\"a_b\":1}"), b" \n{\"a b\":1}");
    }

    #[test]
    fn keys_sharing_a_base_get_consecutive_suffixes() {
        let schema = br#"{"properties":{"a!":{},"a?":{},"a_2":{},"a#":{}}}"#;
        let keys = PropertyKeys::of(schema).unwrap();
        assert_eq!(
            keys.schema(schema),
            br#"{"properties":{"a_":{},"a__2":{},"a_2":{},"a__3":{}}}"#
        );
    }

    #[test]
    fn accepted_schemas_and_invalid_arguments_pass_through() {
        assert!(PropertyKeys::of(br#"{"properties":{"ok":{}}}"#).is_none());
        let keys = PropertyKeys::of(br#"{"properties":{"a b":{}}}"#).unwrap();
        assert_eq!(keys.to_client(br#"{"a_b":"#), br#"{"a_b":"#);
    }
}
