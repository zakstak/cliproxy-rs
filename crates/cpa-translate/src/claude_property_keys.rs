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
//! Names are per tool: one Claude key per rejected client key, chosen apart from every
//! key the tool's schema declares anywhere. Arguments are renamed by location (#195): a
//! member key is renamed only where a subschema applying to that position names it (in
//! `properties`, `required` or a dependency keyword). The walk follows `allOf`, local
//! `$ref` pointers, member and element subschemas, and the branches the instance can
//! take: an `anyOf` or `oneOf` alternative, the `if` side, or a `contains` schema for
//! an element, that the instance's `type`, scalar `const`/`enum`, `required` keys or
//! member values of those kinds (through `allOf` and `$ref`) rule out does not apply.
//! `unevaluatedProperties` and `unevaluatedItems` apply only to members and elements
//! that nothing applying beside them evaluates. Keys of a free-form object (`additionalProperties`,
//! `patternProperties`, no schema) are data and stay as sent, in both directions, even
//! when they spell a renamed key.
//! ponytail: branches the bounded check cannot tell apart all apply, so a free-form map
//! in one and a declared object in another at the same position gets the declared
//! renames; a fuller validator (for example `jsonschema`) is the upgrade.
//! ponytail: an object that declares a renamed key and also accepts undeclared members
//! can receive a member that spells the generated name; it then collides with the
//! renamed key. Declared names avoid each other, but runtime keys are unknown.
//! ponytail: `$ref` resolution covers local JSON pointers (`#`, `#/...`) from the root;
//! anchors, `$id`-relative and remote references are not followed, so keys under them
//! stay as sent. A `$ref` pointer into a renamed property keeps the client's key (#196).
//! Schemas past [`MAX_SCHEMA_NODES`] subschemas are sent unchanged (no renames at all),
//! which bounds the per-member and per-element work of the argument walk.
//! `patternProperties` patterns are matched with ECMA-262's character classes; a
//! pattern the `regex` crate cannot express (lookaround, backreferences) is unknown,
//! and both its subschema and `additionalProperties` apply.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use cpa_common::json::{self as gj, Kind, WALK_DEPTH_LIMIT};
use cpa_common::schema::{SCHEMA_MAP_KEYWORDS, SCHEMA_VALUE_KEYWORDS};
use regex::bytes::{Regex, RegexBuilder};

const MAX_KEY: usize = 64;

/// The most subschemas (objects and booleans, at any depth) a tool's schema may have for
/// its keys to be renamed. Every per-member and per-element step of the argument walk
/// is bounded by the number of subschemas applying there, so this cap keeps that walk
/// linear in the arguments with a fixed factor. A larger schema is sent as the client
/// wrote it, and its arguments and returned calls are left alone, as for a schema
/// Claude accepts: the rename is declined as a whole, never applied in part.
/// ponytail: 512 nodes covers MCP and OpenAI tool schemas seen so far; a schema past it
/// with a rejected key gets Claude's 400 again. Merging member rules per set (so a
/// step's cost is independent of the set's size) is the upgrade that lifts it.
pub(crate) const MAX_SCHEMA_NODES: usize = 512;

/// Whether Claude accepts `key` as an input_schema property key.
fn accepted(key: &[u8]) -> bool {
    (1..=MAX_KEY).contains(&key.len())
        && key
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// A tool's renames, both ways, and the schema tree that places them.
#[derive(Debug, Default)]
pub(crate) struct PropertyKeys {
    to_claude: HashMap<Vec<u8>, Vec<u8>>,
    to_client: HashMap<Vec<u8>, Vec<u8>>,
    tree: Tree,
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

/// JSON Schema's type names.
const TYPE_NAMES: [&[u8]; 7] = [
    b"null", b"boolean", b"object", b"array", b"number", b"integer", b"string",
];

/// Keywords that never stop a subschema from deciding [`Node::check`] for sure: the
/// ones it evaluates and annotations.
/// `allOf` and `$ref` count when their targets do ([`Sets::check`]).
const CHECKED_KEYWORDS: [&[u8]; 21] = [
    b"allOf",
    b"$ref",
    b"$defs",
    b"definitions",
    b"type",
    b"const",
    b"enum",
    b"required",
    b"properties",
    b"title",
    b"description",
    b"$comment",
    b"default",
    b"examples",
    b"deprecated",
    b"readOnly",
    b"writeOnly",
    b"$schema",
    b"$id",
    b"$anchor",
    b"format",
];

/// A schema object the walk visited: an index into the tree's nodes, in visit order,
/// so the same schema bytes always give the same ids. The tool's schema itself is
/// [`ROOT`].
pub(crate) type NodeId = usize;
pub(crate) const ROOT: NodeId = 0;

/// The subschemas of a tool's schema, and what each says about the instance at its
/// position.
#[derive(Debug, Default)]
struct Tree {
    nodes: Vec<Node>,
    /// Each node's JSON pointer (client spelling, RFC 6901 escaped).
    pointers: HashMap<Vec<u8>, NodeId>,
}

#[derive(Debug, Default)]
struct Node {
    /// The node whose instance position this node describes: itself when it was reached
    /// through a member, element or definition keyword, or the root; its parent's
    /// position when it applies in place (`allOf`, `anyOf`, `oneOf`, `if`, `then`,
    /// `else`, `not`, dependent schemas).
    position: NodeId,
    /// JSON pointer, client spelling, RFC 6901 escaped.
    pointer: Vec<u8>,
    /// Property keys (client spelling) this schema names: `properties`, `required`,
    /// dependency keywords.
    named: HashSet<Vec<u8>>,
    /// Member subschemas by key: `properties`.
    properties: HashMap<Vec<u8>, Vec<NodeId>>,
    /// `patternProperties`: the pattern and its subschema.
    patterns: Vec<(Pattern, NodeId)>,
    /// Members no property or pattern of this node covers: `additionalProperties`.
    additional: Vec<NodeId>,
    /// Members that neither this node nor a subschema applying in place beside it
    /// evaluates: `unevaluatedProperties`.
    unevaluated: Vec<NodeId>,
    /// Elements by position: `prefixItems`, or `items` as an array.
    prefix: Vec<Option<NodeId>>,
    /// Elements past `prefix`: `items` as a schema, `additionalItems`.
    rest: Vec<NodeId>,
    /// Elements nothing applying here evaluates: `unevaluatedItems`.
    unevaluated_items: Vec<NodeId>,
    /// Elements that match it: `contains`.
    every: Vec<NodeId>,
    /// What this node evaluates whatever the keyword's value (`true` included), for
    /// `unevaluatedProperties` and `unevaluatedItems` beside it.
    evaluates: Evaluates,
    /// Subschemas that always apply here too: `allOf`, a resolved local `$ref`.
    all_of: Vec<NodeId>,
    /// Alternatives, each applying unless the instance rules it out: `anyOf`, `oneOf`.
    any_of: Vec<NodeId>,
    /// `if`, `then`, `else`.
    condition: [Option<NodeId>; 3],
    /// Subschemas that apply when the instance has the key: `dependentSchemas`, the
    /// schema form of `dependencies`.
    dependent: Vec<(Vec<u8>, NodeId)>,
    /// The `$ref` value, resolved once the walk is done.
    reference: Option<Vec<u8>>,
    /// The part of this schema an instance can be checked against cheaply.
    check: Check,
    /// The keys `check` and `dependent` look up, as each side spells them
    /// ([`Direction::index`]); filled once the names are known.
    keys: [KeyIndex; 2],
}

#[derive(Debug, Default)]
struct Evaluates {
    /// `additionalProperties`: every member.
    members: bool,
    /// `items` as a schema or `additionalItems`: every element past `prefix`.
    rest: bool,
    /// `unevaluatedProperties` and `unevaluatedItems`: whatever is left, as seen by an
    /// enclosing schema.
    unevaluated_members: bool,
    unevaluated_items: bool,
}

/// A node's `required`, property and dependent keys, hashed in one side's spelling, so
/// that a check walks the instance's members and never rehashes schema keys per
/// instance.
#[derive(Debug, Default)]
struct KeyIndex {
    required: HashSet<Vec<u8>>,
    members: HashMap<Vec<u8>, Vec<NodeId>>,
    dependent: HashMap<Vec<u8>, Vec<NodeId>>,
}

impl KeyIndex {
    /// Keys as the client spells them, or renamed with `names` for Claude's side.
    fn new(node: &Node, names: Option<&HashMap<Vec<u8>, Vec<u8>>>) -> KeyIndex {
        let spell = |key: &Vec<u8>| names.and_then(|n| n.get(key)).unwrap_or(key).clone();
        let mut index = KeyIndex {
            required: node.check.required.iter().map(spell).collect(),
            ..KeyIndex::default()
        };
        for (key, child) in &node.check.members {
            index.members.entry(spell(key)).or_default().push(*child);
        }
        for (key, schema) in &node.dependent {
            index.dependent.entry(spell(key)).or_default().push(*schema);
        }
        index
    }
}

#[derive(Debug)]
struct Pattern {
    source: Vec<u8>,
    compiled: OnceLock<Option<Regex>>,
}

impl Pattern {
    /// Whether `key` matches, with ECMA-262's (non-`u`) meaning of the pattern; `None`
    /// when the pattern has no faithful translation here or `key` holds a character
    /// outside the BMP, which ECMA-262 reads as two code units.
    fn matches(&self, key: &[u8]) -> Option<bool> {
        let compiled = self.compiled.get_or_init(|| {
            let source = ecma_pattern(std::str::from_utf8(&self.source).ok()?)?;
            RegexBuilder::new(&source).size_limit(1 << 20).build().ok()
        });
        let key_is_bmp = !key.iter().any(|&b| b >= 0xF0);
        compiled.as_ref().filter(|_| key_is_bmp).map(|r| r.is_match(key))
    }
}

/// ECMA-262 white space and line terminators, as a class body.
const ECMA_SPACE: &str =
    r"\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";

/// An ECMA-262 pattern rewritten for the `regex` crate: `\d`, `\w`, `\s` and their
/// negations, `\b`, `\B` and `.` get ECMA's character sets instead of Unicode's.
/// Only shared syntax and explicitly translated escapes pass through. Unsupported
/// syntax and what the crate rejects (lookaround,
/// backreferences, `\c`, `[^]`) leaves the pattern unknown. `None` for a negated
/// class escape inside a character class, which has no class-body equivalent.
/// ponytail: case-insensitive and other flags do not occur in JSON Schema patterns;
/// a full ECMA-262 regex engine is the upgrade if a schema needs lookaround.
fn ecma_pattern(source: &str) -> Option<String> {
    let mut out = String::with_capacity(source.len() + 16);
    let mut in_class = false;
    let mut chars = source.chars();
    while let Some(c) = chars.next() {
        match (c, in_class) {
            ('\\', _) => {
                let escaped = chars.next()?;
                let replacement = match (escaped, in_class) {
                    ('d', false) => "[0-9]".to_owned(),
                    ('D', false) => "[^0-9]".to_owned(),
                    ('w', false) => "[0-9A-Za-z_]".to_owned(),
                    ('W', false) => "[^0-9A-Za-z_]".to_owned(),
                    ('s', false) => format!("[{ECMA_SPACE}]"),
                    ('S', false) => format!("[^{ECMA_SPACE}]"),
                    ('b', false) => r"(?-u:\b)".to_owned(),
                    ('B', false) => r"(?-u:\B)".to_owned(),
                    ('d', true) => "0-9".to_owned(),
                    ('w', true) => "0-9A-Za-z_".to_owned(),
                    ('s', true) => ECMA_SPACE.to_owned(),
                    ('b', true) => r"\x08".to_owned(),
                    ('D' | 'W' | 'S', true) => return None,
                    ('n' | 'r' | 't', _) => format!("\\{escaped}"),
                    ('f', _) => r"\x0C".to_owned(),
                    ('v', _) => r"\x0B".to_owned(),
                    ('x' | 'u', _) => {
                        let width = if escaped == 'x' { 2 } else { 4 };
                        let mut hex = String::with_capacity(width);
                        for _ in 0..width {
                            let digit = chars.next()?;
                            if !digit.is_ascii_hexdigit() {
                                return None;
                            }
                            hex.push(digit);
                        }
                        format!("\\x{{{hex}}}")
                    }
                    (
                        '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '/' | '\\' | '|' | '-',
                        _,
                    ) => format!("\\{escaped}"),
                    // Identity escapes and Rust-specific escapes can have different
                    // meanings. Only the explicitly translated subset is certain.
                    _ => return None,
                };
                out.push_str(&replacement);
            }
            ('[', false) => {
                in_class = true;
                out.push('[');
                // A leading `^` negates. ECMA's `[]` and `[^]` close at once and reach the
                // crate as `[]`/`[^]`, which it rejects, so the pattern is unknown.
                let rest = chars.as_str();
                if let Some(stripped) = rest.strip_prefix('^') {
                    out.push('^');
                    chars = stripped.chars();
                }
            }
            ('[', true) => return None,
            // Group modifiers other than non-capturing and named groups are Rust-only
            // (`(?i)`, `(?P<n>)`) or rejected (lookaround); `{,n}` is a literal in ECMA.
            ('(', false)
                if chars.as_str().starts_with('?')
                    && !chars.as_str().starts_with("?:")
                    && !(chars.as_str().starts_with("?<")
                        && !chars.as_str().starts_with("?<=")
                        && !chars.as_str().starts_with("?<!")) =>
            {
                return None;
            }
            ('{', false) if chars.as_str().starts_with(',') => return None,
            ('&' | '~', true) => {
                // Rust treats `&&` and `~~` as set operators; ECMA treats them as
                // class members. Escape each character to preserve its literal meaning.
                out.push('\\');
                out.push(c);
            }
            ('-', true) if chars.as_str().starts_with('-') => return None,
            (']', true) => {
                in_class = false;
                out.push(']');
            }
            ('.', false) => out.push_str(r"[^\n\r\x{2028}\x{2029}]"),
            (other, _) => out.push(other),
        }
    }
    Some(out)
}

/// The keywords a branch discriminator usually uses (`type`, scalar `const` and
/// `enum`, `required`, and those on its properties), enough to tell which alternative
/// or conditional branch an instance takes without a general validator.
#[derive(Debug, Default)]
struct Check {
    types: Option<Vec<Vec<u8>>>,
    /// `const` and `enum`, each a set the instance must be in. Lists with a structured
    /// value are left out (and make the check incomplete).
    values: Vec<Values>,
    /// Distinct `required` keys.
    required: Vec<Vec<u8>>,
    /// Property subschemas, checked against the member value.
    members: Vec<(Vec<u8>, NodeId)>,
    /// Whether the node has only keywords the check evaluates or annotations.
    complete: bool,
    /// The `false` schema: nothing matches.
    never: bool,
}

#[derive(Debug)]
enum Scalar {
    Null,
    Bool(bool),
    Number(Number),
    String(Vec<u8>),
}

/// A `const` or `enum` list, hashed so that a lookup costs the same however long the
/// list is.
#[derive(Debug, Default)]
struct Values {
    exact: HashSet<ValueKey>,
    /// The `f64` bits of numbers `f64` does not hold exactly.
    inexact: HashSet<u64>,
}

#[derive(Debug, PartialEq, Eq, Hash)]
enum ValueKey {
    Null,
    Bool(bool),
    String(Vec<u8>),
    Number(u64),
}

/// `f64` bits with `-0.0` as `0.0`, so equal numbers share a key.
fn number_bits(value: f64) -> u64 {
    if value == 0.0 { 0 } else { value.to_bits() }
}

impl Values {
    fn new(list: Vec<Scalar>) -> Values {
        let mut values = Values::default();
        for v in list {
            match v {
                Scalar::Number(n) if !n.exact => {
                    values.inexact.insert(number_bits(n.value));
                }
                v => {
                    values.exact.insert(v.key());
                }
            }
        }
        values
    }

    /// [`Scalar::same`] against every entry, in one lookup.
    fn contains(&self, value: &Scalar) -> Option<bool> {
        match value {
            Scalar::Number(n) => {
                let bits = number_bits(n.value);
                let exact = self.exact.contains(&ValueKey::Number(bits));
                if exact && n.exact {
                    Some(true)
                } else if exact || self.inexact.contains(&bits) {
                    None
                } else {
                    Some(false)
                }
            }
            v => Some(self.exact.contains(&v.key())),
        }
    }
}

/// A JSON number as `f64`, and whether that holds it exactly.
#[derive(Debug, Clone, Copy)]
struct Number {
    value: f64,
    exact: bool,
}

impl Number {
    fn parse(raw: &[u8]) -> Option<Number> {
        let value: f64 = std::str::from_utf8(raw).ok()?.parse().ok()?;
        // ponytail: up to 15 significant digits round-trip through f64; longer literals
        // compare as unknown rather than parsing big decimals.
        let mantissa = raw.split(|&b| b == b'e' || b == b'E').next().unwrap_or_default();
        let digits = mantissa
            .iter()
            .filter(|b| b.is_ascii_digit())
            .skip_while(|&&b| b == b'0')
            .count();
        // Zero literals are exact; any other literal only in the normal range, where 15
        // significant digits round-trip (underflow and subnormals lose them).
        Some(Number {
            value,
            exact: digits == 0 || (value.is_normal() && digits <= 15),
        })
    }
}

impl Scalar {
    fn parse(raw: &[u8]) -> Option<Scalar> {
        match raw.first()? {
            b'n' => Some(Scalar::Null),
            b't' => Some(Scalar::Bool(true)),
            b'f' => Some(Scalar::Bool(false)),
            b'"' => Some(Scalar::String(gj::parse(raw).s.into_owned())),
            _ => Number::parse(raw).map(Scalar::Number),
        }
    }

    fn key(&self) -> ValueKey {
        match self {
            Scalar::Null => ValueKey::Null,
            Scalar::Bool(b) => ValueKey::Bool(*b),
            Scalar::String(s) => ValueKey::String(s.clone()),
            Scalar::Number(n) => ValueKey::Number(number_bits(n.value)),
        }
    }

    /// JSON Schema equality (`1` equals `1.0`); `None` when two numbers round to the
    /// same `f64` but either is not held exactly. Different `f64` values always differ,
    /// since rounding keeps order.
    #[cfg(test)]
    fn same(&self, other: &Scalar) -> Option<bool> {
        match (self, other) {
            (Scalar::Null, Scalar::Null) => Some(true),
            (Scalar::Bool(a), Scalar::Bool(b)) => Some(a == b),
            (Scalar::String(a), Scalar::String(b)) => Some(a == b),
            (Scalar::Number(a), Scalar::Number(b)) if a.value != b.value => Some(false),
            (Scalar::Number(a), Scalar::Number(b)) => (a.exact && b.exact).then_some(true),
            _ => Some(false),
        }
    }
}

/// At least one `Some(true)`: `Some(true)`; else any `None`: `None`; else `Some(false)`.
fn any_of(verdicts: impl Iterator<Item = Option<bool>>) -> Option<bool> {
    let mut verdict = Some(false);
    for v in verdicts {
        match v {
            Some(true) => return Some(true),
            None => verdict = None,
            Some(false) => {}
        }
    }
    verdict
}

/// An instance value being renamed, as the branch checks read it.
struct Instance<'a> {
    raw: &'a [u8],
    /// Object members by key as the instance spells it, the first of repeated keys.
    members: HashMap<Vec<u8>, &'a [u8]>,
}

/// Whether `raw` is of JSON Schema type `name`; `None` for `integer` on a number `f64`
/// does not hold exactly.
fn type_is(raw: &[u8], name: &[u8]) -> Option<bool> {
    Some(match (raw.first(), name) {
        (Some(b'{'), b"object") | (Some(b'['), b"array") | (Some(b'"'), b"string") => true,
        (Some(b'n'), b"null") | (Some(b't' | b'f'), b"boolean") => true,
        (Some(b'-' | b'0'..=b'9'), b"number") => true,
        (Some(b'-' | b'0'..=b'9'), b"integer") => {
            let n = Number::parse(raw)?;
            if !n.exact {
                return None;
            }
            n.value.fract() == 0.0
        }
        _ => false,
    })
}

/// Whether `type`, `const` and `enum` allow `raw` (a structured value matches no
/// scalar list); `None` when a number comparison cannot be made exactly.
fn scalar_check(check: &Check, raw: &[u8]) -> Option<bool> {
    if check.never {
        return Some(false);
    }
    let mut verdict = Some(true);
    if let Some(types) = &check.types {
        match any_of(types.iter().map(|t| type_is(raw, t))) {
            Some(false) => return Some(false),
            None => verdict = None,
            Some(true) => {}
        }
    }
    let value = Scalar::parse(raw).filter(|_| !matches!(raw.first(), Some(b'{' | b'[')));
    for list in &check.values {
        let Some(value) = &value else {
            return Some(false);
        };
        match list.contains(value) {
            Some(false) => return Some(false),
            None => verdict = None,
            Some(true) => {}
        }
    }
    verdict
}

impl PropertyKeys {
    /// The renames a tool's input schema needs, or `None` when Claude accepts it.
    /// Deterministic for the same schema bytes, so the cached prompt prefix stays stable.
    pub(crate) fn of(schema: &[u8]) -> Option<Self> {
        let (tokens, tree) = walk(schema);
        if tree.nodes.len() > MAX_SCHEMA_NODES {
            return None;
        }
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
        let mut to_claude = HashMap::new();
        let mut to_client = HashMap::new();
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
            to_claude.insert(original.to_vec(), name.clone());
            to_client.insert(name, original.to_vec());
        }
        let mut tree = tree;
        for node in &mut tree.nodes {
            node.keys = [KeyIndex::new(node, None), KeyIndex::new(node, Some(&to_claude))];
        }
        Some(PropertyKeys {
            to_claude,
            to_client,
            tree,
        })
    }

    /// `schema` with its rejected property keys, and the references naming them, renamed.
    pub(crate) fn schema(&self, schema: &[u8]) -> Vec<u8> {
        let (tokens, _) = walk(schema);
        let edits = tokens
            .keys
            .iter()
            .chain(&tokens.references)
            .filter_map(|t| self.to_claude.get(&t.text).map(|name| (t, name)));
        splice(schema, edits)
    }

    /// Tool-call arguments with the client's keys renamed, for history sent to Claude.
    pub(crate) fn to_claude(&self, arguments: &[u8]) -> Vec<u8> {
        self.instance_to_claude(ROOT, arguments)
    }

    /// Tool-call arguments from Claude with the client's keys restored. Bytes that are
    /// not valid JSON (a call cut off mid-stream) are returned as they are.
    pub(crate) fn to_client(&self, arguments: &[u8]) -> Vec<u8> {
        self.instance_to_client(ROOT, arguments)
    }

    /// `instance`, a JSON value that schema `node` constrains, with the client's keys
    /// renamed for Claude where the schemas applying at that position name them. A node
    /// inside an in-place applicator (`allOf`, `anyOf`, `if`, ...) constrains its
    /// parent's instance, so the whole set applying there is used, siblings included.
    /// Invalid JSON is returned as it is.
    pub(crate) fn instance_to_claude(&self, node: NodeId, instance: &[u8]) -> Vec<u8> {
        self.rename(node, instance, Direction::ToClaude)
    }

    /// [`Self::instance_to_claude`] the other way: Claude's names back to the client's.
    pub(crate) fn instance_to_client(&self, node: NodeId, instance: &[u8]) -> Vec<u8> {
        self.rename(node, instance, Direction::ToClient)
    }

    /// One walk of `json` alongside the schema nodes that apply at each position. Member
    /// and element transitions are computed once per node set, and a set without
    /// branches resolves once, so like elements of a long array cost a lookup each.
    fn rename(&self, node: NodeId, json: &[u8], direction: Direction) -> Vec<u8> {
        let Some(position) = self.tree.nodes.get(node).map(|n| n.position) else {
            return json.to_vec();
        };
        if !gj::valid(json) {
            return json.to_vec();
        }
        let root = &json[json.iter().take_while(|b| b.is_ascii_whitespace()).count()..];
        let mut sets = Sets::new(self, direction);
        let start = sets.intern(vec![position]);
        let mut edits: Vec<(Token, &Vec<u8>)> = vec![];
        let mut stack: Vec<(&[u8], usize, usize, usize)> = vec![(root, start, NONE, 0)];
        while let Some((value, start, maybe, depth)) = stack.pop() {
            if depth >= WALK_DEPTH_LIMIT {
                continue;
            }
            match value.first() {
                Some(b'{') => {
                    let mut members: Vec<(Token, &[u8])> = vec![];
                    let mut instance = Instance {
                        raw: value,
                        members: HashMap::new(),
                    };
                    gj::object_members(value, |key, escaped, member| {
                        let token = key_token(json, key, escaped);
                        instance.members.entry(token.text.clone()).or_insert(member);
                        members.push((token, member));
                        true
                    });
                    let set = sets.apply(start, maybe, &instance);
                    for (token, member) in members {
                        let rename = self.place(&sets, set, &token.text, direction);
                        let client = match (direction, rename) {
                            (Direction::ToClient, Some(original)) => &original[..],
                            _ => &token.text[..],
                        };
                        let child = sets.member(set, client);
                        if let Some(name) = rename {
                            edits.push((token, name));
                        }
                        if child != EMPTY && matches!(member.first(), Some(b'{' | b'[')) {
                            stack.push((member, child, NONE, depth + 1));
                        }
                    }
                }
                Some(b'[') => {
                    let instance = Instance {
                        raw: value,
                        members: HashMap::new(),
                    };
                    let set = sets.apply(start, maybe, &instance);
                    let mut index = 0;
                    array_items(value, |raw, _, _| {
                        let (child, maybe) = sets.element(set, index);
                        index += 1;
                        if (child != EMPTY || maybe != NONE) && matches!(raw.first(), Some(b'{' | b'[')) {
                            stack.push((raw, child, maybe, depth + 1));
                        }
                    });
                }
                _ => {}
            }
        }
        let edits: Vec<(&Token, &Vec<u8>)> = edits.iter().map(|(t, n)| (t, *n)).collect();
        splice(json, edits.into_iter())
    }

    /// A member key's new name when a schema in `set` names the renamed key.
    fn place(&self, sets: &Sets<'_>, set: usize, key: &[u8], direction: Direction) -> Option<&Vec<u8>> {
        match direction {
            Direction::ToClaude => self.to_claude.get(key).filter(|_| sets.names(set, key)),
            Direction::ToClient => self.to_client.get(key).filter(|original| sets.names(set, original)),
        }
    }
}

#[derive(Clone, Copy)]
enum Direction {
    ToClaude,
    ToClient,
}

impl Direction {
    /// [`Node::keys`] for the instance's side: the client's keys going to Claude,
    /// Claude's coming back.
    fn index(self) -> usize {
        match self {
            Direction::ToClaude => 0,
            Direction::ToClient => 1,
        }
    }
}

/// The empty node set: nothing below it is renamed.
const EMPTY: usize = 0;
/// The empty list of [`Maybe`] nodes.
const NONE: usize = 0;

/// A node that applies to an array element depending on the element.
#[derive(Debug)]
enum Maybe {
    /// `contains`: applies to the elements it does not rule out.
    Matching(NodeId),
    /// `unevaluatedItems` beside `contains` schemas: applies unless one of them
    /// certainly matches (and so evaluates) the element.
    Unless(NodeId, Vec<NodeId>),
}

/// Interned sets of schema nodes, with memoized transitions. A "start" set holds the
/// nodes reached through a member or element keyword; [`Sets::apply`] adds the nodes
/// that apply in place for a given instance.
struct Sets<'k> {
    keys: &'k PropertyKeys,
    direction: Direction,
    sets: Vec<Vec<NodeId>>,
    ids: HashMap<Vec<NodeId>, usize>,
    members: HashMap<(usize, Vec<u8>), usize>,
    elements: HashMap<(usize, usize), (usize, usize)>,
    /// Lists of nodes that apply to an element only if it matches or fails a check;
    /// index [`NONE`] is the empty list.
    maybes: Vec<Vec<Maybe>>,
    /// Start sets whose closure takes no branch, with that closure.
    plain: HashMap<usize, usize>,
    /// Per set, the longest `prefix` among its nodes: elements past it share a set.
    prefix: Vec<usize>,
    /// Per set, built when [`Sets::apply`] first returns it.
    index: Vec<Option<Index>>,
}

/// Branch-check verdicts for one instance: by node, and by member subschema and member
/// value (its position in the arguments). An entry in progress reads as unknown.
#[derive(Default)]
struct Memo {
    checks: HashMap<NodeId, Option<bool>>,
    members: HashMap<(NodeId, usize), Option<bool>>,
}

/// What an applicable set says about object members, merged once so that each member
/// costs a lookup however many subschemas apply.
#[derive(Default)]
struct Index {
    named: HashSet<Vec<u8>>,
    properties: HashMap<Vec<u8>, Vec<NodeId>>,
    /// Nodes with `patternProperties` or `additionalProperties` subschemas: checked per
    /// member.
    open: Vec<NodeId>,
    /// Nodes with `unevaluatedProperties` subschemas, with what is evaluated beside each.
    unevaluated: Vec<(NodeId, Evaluated)>,
}

/// The members a node's in-place neighbourhood evaluates.
#[derive(Default)]
struct Evaluated {
    all: bool,
    keys: HashSet<Vec<u8>>,
    patterns: Vec<NodeId>,
}

impl<'k> Sets<'k> {
    fn new(keys: &'k PropertyKeys, direction: Direction) -> Self {
        let mut sets = Sets {
            keys,
            direction,
            sets: vec![],
            ids: HashMap::new(),
            members: HashMap::new(),
            elements: HashMap::new(),
            maybes: vec![vec![]],
            plain: HashMap::new(),
            prefix: vec![],
            index: vec![],
        };
        sets.intern(vec![]);
        sets
    }

    fn node(&self, n: NodeId) -> &'k Node {
        &self.keys.tree.nodes[n]
    }

    fn intern(&mut self, mut nodes: Vec<NodeId>) -> usize {
        nodes.sort_unstable();
        nodes.dedup();
        if let Some(&id) = self.ids.get(&nodes) {
            return id;
        }
        let id = self.sets.len();
        let prefix = nodes.iter().map(|&n| self.node(n).prefix.len()).max().unwrap_or(0);
        self.prefix.push(prefix);
        self.index.push(None);
        self.ids.insert(nodes.clone(), id);
        self.sets.push(nodes);
        id
    }

    /// The nodes that apply to `instance` given the start set: the start nodes, `allOf`
    /// and `$ref` targets, alternatives the instance does not rule out, the `if` branch
    /// it takes (both when the check cannot tell) and dependent schemas whose key it has,
    /// plus the `maybe` nodes the element check admits. `not` never applies: it says what
    /// the instance is not.
    fn apply(&mut self, start: usize, maybe: usize, instance: &Instance<'_>) -> usize {
        if start == EMPTY && maybe == NONE {
            return EMPTY;
        }
        if maybe == NONE
            && let Some(&id) = self.plain.get(&start)
        {
            return id;
        }
        let mut branched = maybe != NONE;
        let mut memo = Memo::default();
        let mut seen: HashSet<NodeId> = HashSet::new();
        let mut queue = self.sets[start].clone();
        for m in &self.maybes[maybe] {
            match m {
                Maybe::Matching(n) => {
                    if self.check(*n, instance, &mut memo) != Some(false) {
                        queue.push(*n);
                    }
                }
                Maybe::Unless(n, contains) => {
                    if !contains
                        .iter()
                        .any(|&c| self.check(c, instance, &mut memo) == Some(true))
                    {
                        queue.push(*n);
                    }
                }
            }
        }
        while let Some(n) = queue.pop() {
            if !seen.insert(n) {
                continue;
            }
            let node = self.node(n);
            queue.extend(&node.all_of);
            for &alternative in &node.any_of {
                branched = true;
                if self.check(alternative, instance, &mut memo) != Some(false) {
                    queue.push(alternative);
                }
            }
            // Without `if`, `then` and `else` are ignored.
            if let [Some(condition), then, otherwise] = node.condition {
                branched = true;
                match self.check(condition, instance, &mut memo) {
                    Some(true) => queue.extend([Some(condition), then].into_iter().flatten()),
                    Some(false) => queue.extend(otherwise),
                    None => queue.extend([Some(condition), then, otherwise].into_iter().flatten()),
                }
            }
            let dependent = &node.keys[self.direction.index()].dependent;
            if !dependent.is_empty() {
                branched = true;
                for key in instance.members.keys() {
                    if let Some(schemas) = dependent.get(key) {
                        queue.extend(schemas);
                    }
                }
            }
        }
        let id = self.intern(seen.into_iter().collect());
        if !branched {
            self.plain.insert(start, id);
        }
        self.build_index(id);
        id
    }

    fn build_index(&mut self, set: usize) {
        if self.index[set].is_some() {
            return;
        }
        let mut index = Index::default();
        for &n in &self.sets[set] {
            let node = self.node(n);
            index.named.extend(node.named.iter().cloned());
            for (key, children) in &node.properties {
                index.properties.entry(key.clone()).or_default().extend(children);
            }
            if !node.patterns.is_empty() || !node.additional.is_empty() {
                index.open.push(n);
            }
            if !node.unevaluated.is_empty() {
                let mut evaluated = Evaluated::default();
                for m in self.beneath(set, n) {
                    let other = self.node(m);
                    evaluated.all |= other.evaluates.members || (m != n && other.evaluates.unevaluated_members);
                    evaluated.keys.extend(other.properties.keys().cloned());
                    if !other.patterns.is_empty() {
                        evaluated.patterns.push(m);
                    }
                }
                index.unevaluated.push((n, evaluated));
            }
        }
        self.index[set] = Some(index);
    }

    /// Whether `instance` matches node `n`: `Some(false)` when the check rules it out,
    /// `Some(true)` when the node and its `allOf` and `$ref` targets hold only checked
    /// keywords and all pass, `None` when the check cannot tell. Each node is checked
    /// once per instance ([`Memo`]); a reference cycle reads as unknown.
    fn check(&self, n: NodeId, instance: &Instance<'_>, memo: &mut Memo) -> Option<bool> {
        if let Some(&verdict) = memo.checks.get(&n) {
            return verdict;
        }
        memo.checks.insert(n, None);
        let mut verdict = self.check_one(n, instance, memo);
        if verdict != Some(false) {
            for &target in &self.node(n).all_of {
                match self.check(target, instance, memo) {
                    Some(false) => {
                        verdict = Some(false);
                        break;
                    }
                    None => verdict = None,
                    Some(true) => {}
                }
            }
        }
        memo.checks.insert(n, verdict);
        verdict
    }

    /// [`Self::check`] for one node's own keywords.
    fn check_one(&self, n: NodeId, instance: &Instance<'_>, memo: &mut Memo) -> Option<bool> {
        let check = &self.node(n).check;
        let mut sure = check.complete;
        match scalar_check(check, instance.raw) {
            Some(false) => return Some(false),
            None => sure = false,
            Some(true) => {}
        }
        let keys = &self.node(n).keys[self.direction.index()];
        if instance.raw.first() == Some(&b'{') && !(keys.required.is_empty() && keys.members.is_empty()) {
            // More distinct required keys than members: one is missing.
            if keys.required.len() > instance.members.len() {
                return Some(false);
            }
            // Walk the instance's members (its own size), never the schema's keys.
            let mut present = 0;
            for (key, value) in &instance.members {
                present += usize::from(keys.required.contains(key));
                for &child in keys.members.get(key).into_iter().flatten() {
                    match self.check_member(child, value, memo) {
                        Some(false) => return Some(false),
                        Some(true) => {}
                        None => sure = false,
                    }
                }
            }
            if present < keys.required.len() {
                return Some(false);
            }
        }
        sure.then_some(true)
    }

    /// A member value against its property subschema and that subschema's `allOf` and
    /// `$ref` targets: scalar keywords only, so an object or array value whose schemas
    /// constrain its members is never certain.
    fn check_member(&self, n: NodeId, value: &[u8], memo: &mut Memo) -> Option<bool> {
        let key = (n, value.as_ptr() as usize);
        if let Some(&verdict) = memo.members.get(&key) {
            return verdict;
        }
        memo.members.insert(key, None);
        let check = &self.node(n).check;
        let structured = matches!(value.first(), Some(b'{' | b'['));
        let mut verdict = match scalar_check(check, value) {
            Some(false) => Some(false),
            None => None,
            Some(true) if !check.complete => None,
            Some(true) if structured && !(check.required.is_empty() && check.members.is_empty()) => None,
            Some(true) => Some(true),
        };
        if verdict != Some(false) {
            for &target in &self.node(n).all_of {
                match self.check_member(target, value, memo) {
                    Some(false) => {
                        verdict = Some(false);
                        break;
                    }
                    None => verdict = None,
                    Some(true) => {}
                }
            }
        }
        memo.members.insert(key, verdict);
        verdict
    }

    /// `n` and the nodes of `set` that apply in place beneath it: the subschemas whose
    /// evaluation `n`'s `unevaluated*` keywords see.
    fn beneath(&self, set: usize, n: NodeId) -> Vec<NodeId> {
        let nodes = &self.sets[set];
        let mut seen: HashSet<NodeId> = HashSet::new();
        let mut queue = vec![n];
        while let Some(m) = queue.pop() {
            if !nodes.binary_search(&m).is_ok() || !seen.insert(m) {
                continue;
            }
            let node = self.node(m);
            queue.extend(&node.all_of);
            queue.extend(&node.any_of);
            queue.extend(node.condition.iter().flatten());
            queue.extend(node.dependent.iter().map(|(_, d)| *d));
        }
        seen.into_iter().collect()
    }

    /// Whether a schema in `set` names property `key` (client spelling).
    fn names(&self, set: usize, key: &[u8]) -> bool {
        self.index[set].as_ref().is_some_and(|i| i.named.contains(key))
    }

    /// The start set at member `key` (client spelling) of an object at `set`.
    fn member(&mut self, set: usize, key: &[u8]) -> usize {
        if set == EMPTY {
            return EMPTY;
        }
        if let Some(&id) = self.members.get(&(set, key.to_vec())) {
            return id;
        }
        let Some(index) = self.index[set].as_ref() else {
            return EMPTY;
        };
        let mut next = index.properties.get(key).cloned().unwrap_or_default();
        for &n in &index.open {
            let node = self.node(n);
            // A pattern that may match applies; `additionalProperties` applies unless one
            // certainly matches. An unknown pattern applies both, as undecided branches do.
            let mut matched = false;
            for (pattern, child) in &node.patterns {
                match pattern.matches(key) {
                    Some(true) => {
                        matched = true;
                        next.push(*child);
                    }
                    None => next.push(*child),
                    Some(false) => {}
                }
            }
            if !matched && !node.properties.contains_key(key) {
                next.extend(&node.additional);
            }
        }
        for (n, evaluated) in &index.unevaluated {
            let matched = || {
                evaluated
                    .patterns
                    .iter()
                    .any(|&m| self.node(m).patterns.iter().any(|(p, _)| p.matches(key) == Some(true)))
            };
            if !evaluated.all && !evaluated.keys.contains(key) && !matched() {
                next.extend(&self.node(*n).unevaluated);
            }
        }
        let id = self.intern(next);
        self.members.insert((set, key.to_vec()), id);
        id
    }

    /// The start set at element `index` of an array at `set`, and the nodes that apply
    /// depending on the element ([`Maybe`]).
    fn element(&mut self, set: usize, index: usize) -> (usize, usize) {
        if set == EMPTY {
            return (EMPTY, NONE);
        }
        let index = index.min(self.prefix[set]);
        if let Some(&ids) = self.elements.get(&(set, index)) {
            return ids;
        }
        let mut next = vec![];
        let mut maybe = vec![];
        for &n in &self.sets[set] {
            let node = self.node(n);
            match node.prefix.get(index) {
                Some(child) => next.extend(child),
                None => next.extend(&node.rest),
            }
            maybe.extend(node.every.iter().map(|&c| Maybe::Matching(c)));
            if !node.unevaluated_items.is_empty() {
                let beneath = self.beneath(set, n);
                let evaluated = beneath.iter().any(|&m| {
                    let other = self.node(m);
                    index < other.prefix.len() || other.evaluates.rest || (m != n && other.evaluates.unevaluated_items)
                });
                if !evaluated {
                    let contains: Vec<NodeId> = beneath.iter().flat_map(|&m| &self.node(m).every).copied().collect();
                    for &u in &node.unevaluated_items {
                        if contains.is_empty() {
                            next.push(u);
                        } else {
                            maybe.push(Maybe::Unless(u, contains.clone()));
                        }
                    }
                }
            }
        }
        let start = self.intern(next);
        let maybe = if maybe.is_empty() {
            NONE
        } else {
            self.maybes.push(maybe);
            self.maybes.len() - 1
        };
        self.elements.insert((set, index), (start, maybe));
        (start, maybe)
    }
}

impl Tree {
    /// The node a local `$ref` names: `#` or `#/<pointer>`, percent-decoded, then RFC
    /// 6901. `None` for any other reference, an invalid escape or a pointer to no node.
    pub(crate) fn resolve(&self, reference: &[u8]) -> Option<NodeId> {
        let fragment = percent_decode(reference.strip_prefix(b"#")?)?;
        if fragment.is_empty() {
            return (!self.nodes.is_empty()).then_some(ROOT);
        }
        let rest = fragment.strip_prefix(b"/")?;
        let mut pointer = vec![];
        for segment in rest.split(|&b| b == b'/') {
            pointer.push(b'/');
            push_segment(&mut pointer, &unescape_segment(segment)?);
        }
        self.pointers.get(&pointer).copied()
    }
}

fn percent_decode(text: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if text[i] == b'%' {
            let hex = std::str::from_utf8(text.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(text[i]);
            i += 1;
        }
    }
    Some(out)
}

/// RFC 6901 `~1` to `/` and `~0` to `~`; `None` for any other `~`.
fn unescape_segment(segment: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(segment.len());
    let mut bytes = segment.iter();
    while let Some(&b) = bytes.next() {
        if b == b'~' {
            out.push(match bytes.next()? {
                b'0' => b'~',
                b'1' => b'/',
                _ => return None,
            });
        } else {
            out.push(b);
        }
    }
    Some(out)
}

fn push_segment(pointer: &mut Vec<u8>, segment: &[u8]) {
    for &b in segment {
        match b {
            b'~' => pointer.extend_from_slice(b"~0"),
            b'/' => pointer.extend_from_slice(b"~1"),
            _ => pointer.push(b),
        }
    }
}

/// A subschema the walk found: its keyword, the member key or index after it, and its
/// bytes.
struct Child<'a> {
    keyword: Vec<u8>,
    segment: Option<Vec<u8>>,
    schema: &'a [u8],
}

/// Keywords whose subschemas describe a different instance position.
fn moves_position(keyword: &[u8]) -> bool {
    matches!(
        keyword,
        b"properties"
            | b"patternProperties"
            | b"additionalProperties"
            | b"unevaluatedProperties"
            | b"items"
            | b"prefixItems"
            | b"additionalItems"
            | b"unevaluatedItems"
            | b"contains"
            | b"$defs"
            | b"definitions"
            | b"propertyNames"
            | b"contentSchema"
    )
}

/// One walk of `schema`'s subschema positions (JSON Schema applicator and definition
/// keywords; `const`, `default`, `enum` and other data are not schemas): the property
/// keys and the references naming them, and the [`Tree`] of its subschemas. Walks keep
/// their open subschemas on the heap and stop at [`WALK_DEPTH_LIMIT`] nested subschemas,
/// leaving deeper ones as they are.
fn walk(schema: &[u8]) -> (SchemaTokens, Tree) {
    let mut tokens = SchemaTokens::default();
    let mut tree = Tree::default();
    if !gj::valid(schema) {
        return (tokens, tree);
    }
    let root = &schema[schema.iter().take_while(|b| b.is_ascii_whitespace()).count()..];
    if root.first() != Some(&b'{') {
        return (tokens, tree);
    }
    tree.nodes.push(Node::default());
    tree.pointers.insert(vec![], ROOT);
    let mut stack: Vec<(&[u8], usize, NodeId)> = vec![(root, 0, ROOT)];
    while let Some((value, depth, id)) = stack.pop() {
        let mut children: Vec<Child<'_>> = vec![];
        let mut node = std::mem::take(&mut tree.nodes[id]);
        let mut complete = true;
        // `additionalItems` applies only beside an array-valued `items`.
        let (mut tuple_items, mut additional_items) = (false, false);
        gj::object_members(value, |key, escaped, member| {
            let name = if escaped { gj::unescape(key) } else { key.to_vec() };
            let object = member.first() == Some(&b'{');
            let array = member.first() == Some(&b'[');
            complete &= CHECKED_KEYWORDS.contains(&&name[..]);
            match &name[..] {
                b"additionalProperties" => node.evaluates.members = true,
                b"items" if !array => node.evaluates.rest = true,
                b"items" => tuple_items = true,
                b"additionalItems" => additional_items = true,
                b"unevaluatedProperties" => node.evaluates.unevaluated_members = true,
                b"unevaluatedItems" => node.evaluates.unevaluated_items = true,
                _ => {}
            }
            if name == b"properties" && object {
                gj::object_members(member, |k, e, v| {
                    let token = key_token(schema, k, e);
                    node.named.insert(token.text.clone());
                    children.push(Child {
                        keyword: name.clone(),
                        segment: Some(token.text.clone()),
                        schema: v,
                    });
                    tokens.keys.push(token);
                    true
                });
            } else if name == b"required" && array {
                array_items(member, |raw, kind, text| {
                    if kind == Kind::String {
                        node.named.insert(text.to_vec());
                        node.check.required.push(text.to_vec());
                        tokens.references.push(string_token(schema, raw, text));
                    }
                });
            } else if DEPENDENCY_KEYWORDS.contains(&&name[..]) && object {
                gj::object_members(member, |k, e, v| {
                    let token = key_token(schema, k, e);
                    node.named.insert(token.text.clone());
                    if v.first() == Some(&b'[') {
                        array_items(v, |raw, kind, text| {
                            if kind == Kind::String {
                                node.named.insert(text.to_vec());
                                tokens.references.push(string_token(schema, raw, text));
                            }
                        });
                    } else {
                        children.push(Child {
                            keyword: name.clone(),
                            segment: Some(token.text.clone()),
                            schema: v,
                        });
                    }
                    tokens.references.push(token);
                    true
                });
            } else if SCHEMA_MAP_KEYWORDS.contains(&&name[..]) && object {
                gj::object_members(member, |k, e, v| {
                    let k = if e { gj::unescape(k) } else { k.to_vec() };
                    children.push(Child {
                        keyword: name.clone(),
                        segment: Some(k),
                        schema: v,
                    });
                    true
                });
            } else if SCHEMA_VALUE_KEYWORDS.contains(&&name[..]) {
                if object || member == b"true" || member == b"false" {
                    children.push(Child {
                        keyword: name.clone(),
                        segment: None,
                        schema: member,
                    });
                } else if array {
                    let mut i = 0;
                    array_items(member, |raw, _, _| {
                        children.push(Child {
                            keyword: name.clone(),
                            segment: Some(i.to_string().into_bytes()),
                            schema: raw,
                        });
                        i += 1;
                    });
                }
            } else if name == b"$ref" && member.first() == Some(&b'"') {
                node.reference = Some(gj::parse(member).s.into_owned());
            } else if name == b"type" {
                if member.first() == Some(&b'"') {
                    node.check.types = Some(vec![gj::parse(member).s.into_owned()]);
                } else if array {
                    let mut types = vec![];
                    array_items(member, |_, kind, text| {
                        if kind == Kind::String {
                            types.push(text.to_vec());
                        }
                    });
                    node.check.types = Some(types);
                }
            } else if name == b"const" {
                match Scalar::parse(member).filter(|_| !object && !array) {
                    Some(value) => node.check.values.push(Values::new(vec![value])),
                    None => complete = false,
                }
            } else if name == b"enum" && array {
                let mut values = Some(vec![]);
                array_items(member, |raw, _, _| {
                    let value = Scalar::parse(raw).filter(|_| !matches!(raw.first(), Some(b'{' | b'[')));
                    match (&mut values, value) {
                        (Some(list), Some(v)) => list.push(v),
                        _ => values = None,
                    }
                });
                match values {
                    Some(list) => node.check.values.push(Values::new(list)),
                    None => complete = false,
                }
            }
            true
        });
        node.check.complete = complete;
        // Distinct entries, in order, so a `required` check costs at most the instance's
        // own size and a `type` check at most the seven type names; any other name makes
        // the type unknown.
        let mut seen: HashSet<Vec<u8>> = HashSet::new();
        node.check.required.retain(|key| seen.insert(key.clone()));
        if let Some(types) = &mut node.check.types {
            let mut seen: HashSet<Vec<u8>> = HashSet::new();
            if types.iter().all(|t| TYPE_NAMES.contains(&&t[..])) {
                types.retain(|t| seen.insert(t.clone()));
            } else {
                node.check.types = None;
                node.check.complete = false;
            }
        }
        node.evaluates.rest |= tuple_items && additional_items;
        for Child {
            keyword,
            segment,
            schema: child,
        } in children
        {
            let tuple = segment.is_some() && (keyword == b"items" || keyword == b"prefixItems");
            // `true` and `false` are schemas too: they evaluate and apply like `{}` and
            // `{"not":{}}`, so they stay in the tree.
            let boolean = match child {
                b"true" => Some(true),
                b"false" => Some(false),
                _ => None,
            };
            if (child.first() != Some(&b'{') && boolean.is_none()) || depth + 1 >= WALK_DEPTH_LIMIT {
                if tuple {
                    // Keeps later positions in place; nothing is renamed here.
                    node.prefix.push(None);
                }
                if keyword == b"allOf" {
                    // Past the depth limit (or not a schema): not checked.
                    node.check.complete = false;
                }
                continue;
            }
            let mut pointer = node.pointer.clone();
            pointer.push(b'/');
            push_segment(&mut pointer, &keyword);
            if let Some(segment) = &segment {
                pointer.push(b'/');
                push_segment(&mut pointer, segment);
            }
            let child_id = tree.nodes.len();
            let position = if moves_position(&keyword) {
                child_id
            } else {
                node.position
            };
            tree.pointers.entry(pointer.clone()).or_insert(child_id);
            tree.nodes.push(Node {
                position,
                pointer,
                check: Check {
                    complete: boolean.is_some(),
                    never: boolean == Some(false),
                    ..Check::default()
                },
                ..Node::default()
            });
            let key = segment.unwrap_or_default();
            match &keyword[..] {
                b"properties" => {
                    node.properties.entry(key.clone()).or_default().push(child_id);
                    node.check.members.push((key, child_id));
                }
                b"patternProperties" => node.patterns.push((
                    Pattern {
                        source: key,
                        compiled: OnceLock::new(),
                    },
                    child_id,
                )),
                b"additionalProperties" => node.additional.push(child_id),
                b"unevaluatedProperties" => node.unevaluated.push(child_id),
                _ if tuple => node.prefix.push(Some(child_id)),
                b"items" => node.rest.push(child_id),
                b"additionalItems" if tuple_items => node.rest.push(child_id),
                b"unevaluatedItems" => node.unevaluated_items.push(child_id),
                b"contains" => node.every.push(child_id),
                b"allOf" => node.all_of.push(child_id),
                b"anyOf" | b"oneOf" => node.any_of.push(child_id),
                b"if" => node.condition[0] = Some(child_id),
                b"then" => node.condition[1] = Some(child_id),
                b"else" => node.condition[2] = Some(child_id),
                b"dependentSchemas" | b"dependencies" => node.dependent.push((key, child_id)),
                // `not`, definitions, `propertyNames`, `contentSchema`: walked for their
                // keys, never applied to an argument position.
                _ => {}
            }
            if boolean.is_none() {
                stack.push((child, depth + 1, child_id));
            }
        }
        tree.nodes[id] = node;
    }
    for n in 0..tree.nodes.len() {
        if let Some(reference) = tree.nodes[n].reference.take() {
            match tree.resolve(&reference) {
                Some(target) => tree.nodes[n].all_of.push(target),
                // An unresolved reference may hold any constraint.
                None => tree.nodes[n].check.complete = false,
            }
        }
    }
    (tokens, tree)
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
        // `tab\tkey` has no schema, so the object inside it is data and keeps `a_b`.
        let restored = keys.to_client(br#"{"a_b":1,"tab_key":[{"a_b":2}],"s":"a_b"}"#);
        assert_eq!(restored, br#"{"a\"b":1,"tab\tkey":[{"a_b":2}],"s":"a_b"}"#);
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

    /// Both directions, as a client and Claude would spell the same arguments.
    fn round_trip(schema: &[u8], client: &str, claude: &str) {
        let keys = PropertyKeys::of(schema).unwrap();
        assert_eq!(String::from_utf8(keys.to_claude(client.as_bytes())).unwrap(), claude);
        assert_eq!(String::from_utf8(keys.to_client(claude.as_bytes())).unwrap(), client);
    }

    #[test]
    fn free_form_maps_keep_both_spellings() {
        let schema = br##"{"type":"object","properties":{"x y":{"type":"string"},"data":{"type":"object","additionalProperties":true},"any":{},"list":{"type":"array"}}}"##;
        round_trip(
            schema,
            r##"{"x y":"declared","data":{"x y":1,"x_y":2},"any":{"x_y":3},"list":[{"x y":4,"x_y":5}]}"##,
            r##"{"x_y":"declared","data":{"x y":1,"x_y":2},"any":{"x_y":3},"list":[{"x y":4,"x_y":5}]}"##,
        );
    }

    #[test]
    fn member_and_element_subschemas_are_followed() {
        let schema = br##"{"properties":{
            "map":{"additionalProperties":{"properties":{"a b":{}}}},
            "pat":{"patternProperties":{"^p":{"properties":{"a b":{}}}},"additionalProperties":true},
            "tuple":{"prefixItems":[{"properties":{"a b":{}}},true],"items":{"properties":{"c d":{}}}},
            "old":{"items":[{"properties":{"a b":{}}}],"additionalItems":{"properties":{"c d":{}}}},
            "has":{"contains":{"properties":{"c d":{}}}}
        }}"##;
        round_trip(
            schema,
            r##"{"map":{"k":{"a b":1,"c d":0}},"pat":{"p1":{"a b":2},"q":{"a b":3}},"tuple":[{"a b":4},{"a b":5},{"c d":6}],"old":[{"a b":7},{"c d":8}],"has":[{"c d":9}]}"##,
            r##"{"map":{"k":{"a_b":1,"c d":0}},"pat":{"p1":{"a_b":2},"q":{"a b":3}},"tuple":[{"a_b":4},{"a b":5},{"c_d":6}],"old":[{"a_b":7},{"c_d":8}],"has":[{"c_d":9}]}"##,
        );
    }

    #[test]
    fn local_refs_and_all_of_apply_in_place() {
        let schema = br##"{"$defs":{"d":{"properties":{"a b":{}}}},"properties":{
            "r":{"$ref":"#/$defs/d"},
            "s":{"allOf":[{"properties":{"a b":{}}}],"additionalProperties":true},
            "esc":{"$ref":"#/$defs/%64"},
            "remote":{"$ref":"https://example.com/d"},
            "n":{"not":{"properties":{"a b":{}}}}
        }}"##;
        round_trip(
            schema,
            r##"{"r":{"a b":1},"s":{"a b":2},"esc":{"a b":3},"remote":{"a b":4},"n":{"a b":5}}"##,
            r##"{"r":{"a_b":1},"s":{"a_b":2},"esc":{"a_b":3},"remote":{"a b":4},"n":{"a b":5}}"##,
        );
    }

    #[test]
    fn recursive_refs_follow_the_instance() {
        let schema = br##"{"$defs":{"node":{"properties":{"child id":{},"kids":{"type":"array","items":{"$ref":"#/$defs/node"}}}}},"$ref":"#/$defs/node"}"##;
        round_trip(
            schema,
            r##"{"child id":1,"kids":[{"child id":2,"kids":[{"child id":3}]}]}"##,
            r##"{"child_id":1,"kids":[{"child_id":2,"kids":[{"child_id":3}]}]}"##,
        );
    }

    #[test]
    fn discriminated_branches_apply_only_where_the_instance_takes_them() {
        let one_of = br##"{"properties":{"kind":{"type":"string"},"data":{}},"oneOf":[
            {"properties":{"kind":{"const":"a"},"data":{"properties":{"x y":{}}}}},
            {"properties":{"kind":{"enum":["b"]},"data":{"additionalProperties":true}}}
        ]}"##;
        round_trip(
            one_of,
            r##"{"kind":"a","data":{"x y":1}}"##,
            r##"{"kind":"a","data":{"x_y":1}}"##,
        );
        round_trip(
            one_of,
            r##"{"kind":"b","data":{"x_y":"opaque","x y":2}}"##,
            r##"{"kind":"b","data":{"x_y":"opaque","x y":2}}"##,
        );
        let if_then = br##"{"properties":{"kind":{},"data":{}},
            "if":{"properties":{"kind":{"const":"a"}},"required":["kind"]},
            "then":{"properties":{"data":{"properties":{"x y":{}}}}},
            "else":{"properties":{"data":{"additionalProperties":true}}}}"##;
        round_trip(
            if_then,
            r##"{"kind":"a","data":{"x y":1}}"##,
            r##"{"kind":"a","data":{"x_y":1}}"##,
        );
        round_trip(
            if_then,
            r##"{"kind":"b","data":{"x_y":1}}"##,
            r##"{"kind":"b","data":{"x_y":1}}"##,
        );
        round_trip(if_then, r##"{"data":{"x_y":1}}"##, r##"{"data":{"x_y":1}}"##);
        // A discriminator whose own key is renamed is read in each side's spelling.
        let renamed = br##"{"properties":{"the kind":{},"data":{}},"anyOf":[
            {"properties":{"the kind":{"const":1},"data":{"properties":{"x y":{}}}}},
            {"properties":{"the kind":{"const":2}}}
        ]}"##;
        round_trip(
            renamed,
            r##"{"the kind":1,"data":{"x y":1}}"##,
            r##"{"the_kind":1,"data":{"x_y":1}}"##,
        );
        round_trip(
            renamed,
            r##"{"the kind":2,"data":{"x_y":1}}"##,
            r##"{"the_kind":2,"data":{"x_y":1}}"##,
        );
    }

    #[test]
    fn referenced_alternatives_are_checked_through_their_targets() {
        // Pydantic's discriminated union: alternatives are `$ref`s to definitions.
        let schema = br##"{"$defs":{
            "A":{"type":"object","properties":{"kind":{"const":"a"},"body":{"properties":{"x y":{}}}},"required":["kind"]},
            "B":{"type":"object","properties":{"kind":{"const":"b"},"body":{"additionalProperties":true}},"required":["kind"]}
        },"properties":{"event":{"oneOf":[{"$ref":"#/$defs/A"},{"allOf":[{"$ref":"#/$defs/B"}]}]}}}"##;
        round_trip(
            schema,
            r##"{"event":{"kind":"a","body":{"x y":1}}}"##,
            r##"{"event":{"kind":"a","body":{"x_y":1}}}"##,
        );
        round_trip(
            schema,
            r##"{"event":{"kind":"b","body":{"x_y":1}}}"##,
            r##"{"event":{"kind":"b","body":{"x_y":1}}}"##,
        );
        // An `if` decided through a reference picks one side.
        let if_ref = br##"{"$defs":{"isA":{"properties":{"kind":{"const":"a"}},"required":["kind"]}},
            "properties":{"kind":{},"body":{}},"if":{"$ref":"#/$defs/isA"},
            "then":{"properties":{"body":{"properties":{"x y":{}}}}},
            "else":{"properties":{"body":{"additionalProperties":true}}}}"##;
        let keys = PropertyKeys::of(if_ref).unwrap();
        assert_eq!(
            keys.to_client(br##"{"kind":"a","body":{"x_y":1}}"##),
            br##"{"kind":"a","body":{"x y":1}}"##
        );
        assert_eq!(
            keys.to_client(br##"{"kind":"b","body":{"x_y":1}}"##),
            br##"{"kind":"b","body":{"x_y":1}}"##
        );
    }

    #[test]
    fn member_checks_follow_references() {
        // The condition's member schema is a `$ref` to a scalar `const`.
        let schema = br##"{"$defs":{"kindA":{"const":"a"}},"properties":{"payload":{
            "if":{"properties":{"kind":{"$ref":"#/$defs/kindA"}},"required":["kind"]},
            "then":{"properties":{"data":{"properties":{"x y":{"type":"string"}}}}},
            "else":{"properties":{"data":{"additionalProperties":true}}}}}}"##;
        round_trip(
            schema,
            r##"{"payload":{"kind":"b","data":{"x_y":"opaque"}}}"##,
            r##"{"payload":{"kind":"b","data":{"x_y":"opaque"}}}"##,
        );
        round_trip(
            schema,
            r##"{"payload":{"kind":"a","data":{"x y":"declared"}}}"##,
            r##"{"payload":{"kind":"a","data":{"x_y":"declared"}}}"##,
        );
        // A member schema with structure of its own decides nothing for an object value.
        let nested = br##"{"properties":{"p":{"anyOf":[
            {"properties":{"meta":{"type":"object","required":["v"]},"data":{"properties":{"x y":{}}}}},
            {"properties":{"meta":{"type":"object"},"data":{"additionalProperties":true}}}
        ]}}}"##;
        round_trip(
            nested,
            r##"{"p":{"meta":{},"data":{"x y":1}}}"##,
            r##"{"p":{"meta":{},"data":{"x_y":1}}}"##,
        );
    }

    #[test]
    fn contains_applies_only_to_matching_elements() {
        let schema = br##"{"properties":{"items":{"type":"array","contains":{"type":"object",
            "properties":{"kind":{"const":"a"},"x y":{"type":"string"}},"required":["kind","x y"]}}}}"##;
        round_trip(
            schema,
            r##"{"items":[{"kind":"a","x y":"declared"},{"kind":"b","x_y":"opaque"},"s"]}"##,
            r##"{"items":[{"kind":"a","x_y":"declared"},{"kind":"b","x_y":"opaque"},"s"]}"##,
        );
    }

    #[test]
    fn unevaluated_properties_skip_members_evaluated_beside_them() {
        let schema = br##"{"properties":{"payload":{"type":"object",
            "allOf":[{"properties":{"data":{"type":"object","additionalProperties":true}}}],
            "unevaluatedProperties":{"type":"object","properties":{"x y":{"type":"string"}}}}}}"##;
        round_trip(
            schema,
            r##"{"payload":{"data":{"x_y":"opaque"},"extra":{"x y":"declared"}}}"##,
            r##"{"payload":{"data":{"x_y":"opaque"},"extra":{"x_y":"declared"}}}"##,
        );
        // Members a taken branch evaluates are evaluated; an untaken branch's are not.
        let branches = br##"{"properties":{"p":{"properties":{"kind":{}},
            "oneOf":[{"properties":{"kind":{"const":"a"},"data":{"additionalProperties":true}}},
                     {"properties":{"kind":{"const":"b"}}}],
            "unevaluatedProperties":{"properties":{"x y":{}}}}}}"##;
        round_trip(
            branches,
            r##"{"p":{"kind":"a","data":{"x_y":1}}}"##,
            r##"{"p":{"kind":"a","data":{"x_y":1}}}"##,
        );
        round_trip(
            branches,
            r##"{"p":{"kind":"b","data":{"x y":1}}}"##,
            r##"{"p":{"kind":"b","data":{"x_y":1}}}"##,
        );
        // `additionalProperties` beside it evaluates every member.
        let additional = br##"{"properties":{"p":{"properties":{"q":{"properties":{"x y":{}}}},
            "additionalProperties":true,"unevaluatedProperties":{"properties":{"x y":{}}}}}}"##;
        round_trip(additional, r##"{"p":{"m":{"x_y":1}}}"##, r##"{"p":{"m":{"x_y":1}}}"##);
    }

    #[test]
    fn unevaluated_items_skip_elements_evaluated_beside_them() {
        let schema = br##"{"properties":{"list":{
            "allOf":[{"prefixItems":[{"additionalProperties":true}]}],
            "contains":{"properties":{"kind":{"const":"c"}},"required":["kind"]},
            "unevaluatedItems":{"properties":{"x y":{}}}}}}"##;
        round_trip(
            schema,
            r##"{"list":[{"x_y":0},{"kind":"c","x_y":1},{"x y":2}]}"##,
            r##"{"list":[{"x_y":0},{"kind":"c","x_y":1},{"x_y":2}]}"##,
        );
    }

    #[test]
    fn boolean_subschemas_evaluate_and_apply() {
        // `properties: {data: true}` evaluates `data`, so `unevaluatedProperties` skips it.
        let evaluated = br##"{"properties":{"p":{"properties":{"data":true},
            "unevaluatedProperties":{"properties":{"x y":{"type":"string"}}}}}}"##;
        round_trip(
            evaluated,
            r##"{"p":{"data":{"x_y":1},"extra":{"x y":2}}}"##,
            r##"{"p":{"data":{"x_y":1},"extra":{"x_y":2}}}"##,
        );
        // A matching boolean pattern keeps `additionalProperties` off the member.
        for value in ["true", "false"] {
            let pattern = format!(
                r##"{{"properties":{{"p":{{"patternProperties":{{"^data$":{value}}},"additionalProperties":{{"properties":{{"x y":{{}}}}}}}}}}}}"##
            );
            round_trip(
                pattern.as_bytes(),
                r##"{"p":{"data":{"x_y":1},"other":{"x y":2}}}"##,
                r##"{"p":{"data":{"x_y":1},"other":{"x_y":2}}}"##,
            );
        }
        // `if: true` takes `then`; `if: false` takes `else`.
        let conditional = |value: &str| {
            format!(
                r##"{{"properties":{{"p":{{"if":{value},"then":{{"properties":{{"x y":{{}}}}}},"else":{{"additionalProperties":true}}}}}}}}"##
            )
        };
        round_trip(
            conditional("true").as_bytes(),
            r##"{"p":{"x y":1}}"##,
            r##"{"p":{"x_y":1}}"##,
        );
        round_trip(
            conditional("false").as_bytes(),
            r##"{"p":{"x_y":1}}"##,
            r##"{"p":{"x_y":1}}"##,
        );
        // `contains: true` evaluates every element; `contains: false` none.
        let contains = |value: &str| {
            format!(
                r##"{{"properties":{{"l":{{"contains":{value},"unevaluatedItems":{{"properties":{{"x y":{{}}}}}}}}}}}}"##
            )
        };
        round_trip(
            contains("true").as_bytes(),
            r##"{"l":[{"x_y":1}]}"##,
            r##"{"l":[{"x_y":1}]}"##,
        );
        round_trip(
            contains("false").as_bytes(),
            r##"{"l":[{"x y":1}]}"##,
            r##"{"l":[{"x_y":1}]}"##,
        );
        // A `false` alternative never applies; a `true` one adds nothing.
        let alternatives = br##"{"properties":{"p":{"anyOf":[false,true,{"properties":{"x y":{}}}]}}}"##;
        round_trip(alternatives, r##"{"p":{"x y":1}}"##, r##"{"p":{"x_y":1}}"##);
    }

    #[test]
    fn many_in_place_branches_each_declare_their_member() {
        let branches: Vec<String> = (0..150)
            .map(|i| format!(r##"{{"properties":{{"k {i}":{{"properties":{{"x y":{{}}}}}}}}}}"##))
            .collect();
        let schema = format!(r##"{{"properties":{{"p":{{"allOf":[{}]}}}}}}"##, branches.join(","));
        let client: Vec<String> = (0..150).map(|i| format!(r##""k {i}":{{"x y":{i}}}"##)).collect();
        let claude: Vec<String> = (0..150).map(|i| format!(r##""k_{i}":{{"x_y":{i}}}"##)).collect();
        round_trip(
            schema.as_bytes(),
            &format!(r##"{{"p":{{{}}}}}"##, client.join(",")),
            &format!(r##"{{"p":{{{}}}}}"##, claude.join(",")),
        );
    }

    /// A schema with `n` in-place subschemas beside a rejected key: the walk visits the
    /// root, `p`, `x y` and each `allOf` member with its `additionalProperties`.
    fn wide_schema(n: usize) -> String {
        let branch = r##"{"additionalProperties":{}}"##;
        format!(
            r##"{{"properties":{{"p":{{"properties":{{"x y":{{}}}},"allOf":[{}]}}}}}}"##,
            vec![branch; n].join(",")
        )
    }

    #[test]
    fn schemas_past_the_node_cap_are_left_as_sent() {
        // root, p, x y: 3 nodes; each branch adds 2.
        let fits = wide_schema((MAX_SCHEMA_NODES - 3) / 2);
        let keys = PropertyKeys::of(fits.as_bytes()).unwrap();
        assert_eq!(keys.to_claude(br##"{"p":{"x y":1}}"##), br##"{"p":{"x_y":1}}"##);

        let wide = wide_schema((MAX_SCHEMA_NODES - 3) / 2 + 1);
        assert!(PropertyKeys::of(wide.as_bytes()).is_none());
    }

    #[test]
    fn numbers_past_f64_precision_are_unknown_not_equal() {
        // 9007199254740993 rounds to 9007199254740992 in f64: the `if` cannot be decided,
        // so both sides apply and `else`'s declared key is renamed.
        let schema = br##"{"properties":{"p":{"properties":{"id":{}},
            "if":{"properties":{"id":{"const":9007199254740992}},"required":["id"]},
            "then":{},"else":{"properties":{"x y":{}}}}}}"##;
        round_trip(
            schema,
            r##"{"p":{"id":9007199254740993,"x y":1}}"##,
            r##"{"p":{"id":9007199254740993,"x_y":1}}"##,
        );
        // Exact numbers still decide: 1 equals 1.0, 2 does not.
        let exact = br##"{"properties":{"p":{"properties":{"id":{}},
            "if":{"properties":{"id":{"const":1}},"required":["id"]},
            "then":{"properties":{"x y":{}}},"else":{"additionalProperties":true}}}}"##;
        round_trip(exact, r##"{"p":{"id":1.0,"x y":1}}"##, r##"{"p":{"id":1.0,"x_y":1}}"##);
        round_trip(exact, r##"{"p":{"id":2,"x_y":1}}"##, r##"{"p":{"id":2,"x_y":1}}"##);
        assert_eq!(type_is(b"9007199254740993", b"integer"), None);
        assert_eq!(type_is(b"1.5", b"integer"), Some(false));
        assert_eq!(type_is(b"3", b"integer"), Some(true));
    }

    #[test]
    fn pattern_syntax_never_inherits_rust_only_meanings() {
        let pattern = |p: &str| Pattern {
            source: p.as_bytes().to_vec(),
            compiled: OnceLock::new(),
        };
        assert_eq!(pattern(r"^[a&&b]$").matches(b"&"), Some(true));
        assert_eq!(pattern(r"^[a~~b]$").matches(b"~"), Some(true));
        assert_eq!(pattern(r"^\a$").matches(b"a"), None);
        assert_eq!(pattern(r"^\a$").matches(b"\x07"), None);
        assert_eq!(pattern(r"^[a[b]c]$").matches(b"ac]"), None);
        assert_eq!(pattern(r"^a$").matches(b"a\n"), Some(false));
        assert_eq!(pattern(r"^\x61\u0062$").matches(b"ab"), Some(true));
        assert_eq!(pattern(r"(?i)^a$").matches(b"A"), None);
        assert_eq!(pattern(r"^(?P<n>a)$").matches(b"a"), None);
        assert_eq!(pattern(r"^a{,2}$").matches(b"a"), None);
        assert_eq!(pattern(r"^(?:a|b)(?<n>c)$").matches(b"bc"), Some(true));
    }

    #[test]
    fn patterns_use_ecma_character_classes() {
        let pattern = |p: &str| Pattern {
            source: p.as_bytes().to_vec(),
            compiled: OnceLock::new(),
        };
        assert_eq!(pattern(r"^\d+$").matches("١".as_bytes()), Some(false));
        assert_eq!(pattern(r"^\d+$").matches(b"12"), Some(true));
        assert_eq!(pattern(r"^\w$").matches("é".as_bytes()), Some(false));
        assert_eq!(pattern(r"^\W$").matches("é".as_bytes()), Some(true));
        assert_eq!(pattern(r"^[\d_]+$").matches(b"1_2"), Some(true));
        assert_eq!(pattern(r"^[\d]+$").matches("٣".as_bytes()), Some(false));
        assert_eq!(pattern(r"^\s$").matches("\u{FEFF}".as_bytes()), Some(true));
        assert_eq!(pattern(r"^\s$").matches("\u{85}".as_bytes()), Some(false));
        assert_eq!(pattern(r"^.$").matches(b"\r"), Some(false));
        assert_eq!(pattern(r"^.$").matches("é".as_bytes()), Some(true));
        // Multibyte literals and `.` read one character, not one byte.
        assert_eq!(pattern("^é$").matches("é".as_bytes()), Some(true));
        assert_eq!(pattern("^[éa]$").matches("é".as_bytes()), Some(true));
        assert_eq!(pattern(r"^..$").matches("é".as_bytes()), Some(false));
        assert_eq!(pattern(r"a\b").matches("aé".as_bytes()), Some(true));
        assert_eq!(pattern(r"^[^\d]$").matches(b"x"), Some(true));
        // Unknown: no faithful translation, a rejected construct, a non-BMP key.
        assert_eq!(pattern(r"^[\D]$").matches(b"x"), None);
        assert_eq!(pattern(r"^(?=a)").matches(b"a"), None);
        assert_eq!(pattern(r"^.$").matches("😀".as_bytes()), None);

        let schema = br##"{"properties":{"map":{"patternProperties":{"^\\d+$":true},
            "additionalProperties":{"properties":{"x y":{}}}}}}"##;
        round_trip(
            schema,
            r##"{"map":{"١":{"x y":1},"1":{"x_y":2}}}"##,
            r##"{"map":{"١":{"x_y":1},"1":{"x_y":2}}}"##,
        );
        // An unknown pattern applies with `additionalProperties`.
        let unknown = br##"{"properties":{"map":{"patternProperties":{"^(?=k)":true},
            "additionalProperties":{"properties":{"x y":{}}}}}}"##;
        round_trip(unknown, r##"{"map":{"k":{"x y":1}}}"##, r##"{"map":{"k":{"x_y":1}}}"##);
    }

    #[test]
    fn unknown_type_names_leave_the_check_undecided() {
        // `"float"` is no JSON Schema type: the `if` cannot be decided, so both sides apply.
        let schema = br##"{"properties":{"p":{"properties":{"id":{}},
            "if":{"properties":{"id":{"type":["float","string","string"]}},"required":["id","id"]},
            "then":{"additionalProperties":true},"else":{"properties":{"x y":{}}}}}}"##;
        round_trip(schema, r##"{"p":{"id":"s","x y":1}}"##, r##"{"p":{"id":"s","x_y":1}}"##);
        // Known names, repeated, still decide: a string `id` takes `then`.
        let known = br##"{"properties":{"p":{"properties":{"id":{}},
            "if":{"properties":{"id":{"type":["string","string"]}},"required":["id","id"]},
            "then":{"additionalProperties":true},"else":{"properties":{"x y":{}}}}}}"##;
        round_trip(known, r##"{"p":{"id":"s","x_y":1}}"##, r##"{"p":{"id":"s","x_y":1}}"##);
        round_trip(known, r##"{"p":{"id":1,"x y":1}}"##, r##"{"p":{"id":1,"x_y":1}}"##);
    }

    #[test]
    fn underflowing_numbers_are_not_exact() {
        let zero = Scalar::parse(b"0").unwrap();
        for tiny in [&b"1e-999"[..], b"1e-324", b"2e-324", b"4e-320"] {
            let n = Scalar::parse(tiny).unwrap();
            assert_eq!(
                n.same(&zero),
                if tiny == b"4e-320" { Some(false) } else { None },
                "{tiny:?}"
            );
            assert_eq!(type_is(tiny, b"integer"), None, "{tiny:?}");
        }
        assert_eq!(
            Scalar::parse(b"1e-324")
                .unwrap()
                .same(&Scalar::parse(b"2e-324").unwrap()),
            None
        );
        assert_eq!(Scalar::parse(b"0e-999").unwrap().same(&zero), Some(true));
        assert_eq!(Scalar::parse(b"-0.0").unwrap().same(&zero), Some(true));
        assert_eq!(Scalar::parse(b"1e-300").unwrap().same(&zero), Some(false));
        // In a branch: `id: 1e-999` cannot be told from `const: 0`, so both sides apply.
        let schema = br##"{"properties":{"p":{"properties":{"id":{}},
            "if":{"properties":{"id":{"const":0}},"required":["id"]},
            "then":{"additionalProperties":true},"else":{"properties":{"x y":{}}}}}}"##;
        round_trip(
            schema,
            r##"{"p":{"id":1e-999,"x y":1}}"##,
            r##"{"p":{"id":1e-999,"x_y":1}}"##,
        );
        round_trip(schema, r##"{"p":{"id":0.0,"x_y":1}}"##, r##"{"p":{"id":0.0,"x_y":1}}"##);
    }

    #[test]
    fn long_enums_and_required_lists_check_by_lookup() {
        let ids: Vec<String> = (1..=2000).map(|i| i.to_string()).collect();
        let keys: Vec<String> = (1..=2000).map(|i| format!(r#""k{i}""#)).collect();
        let schema = format!(
            r##"{{"properties":{{"rows":{{"items":{{"properties":{{"id":{{}}}},
            "if":{{"properties":{{"id":{{"enum":[{}]}}}},"required":["id"]}},
            "then":{{"properties":{{"x y":{{}}}}}},"else":{{"additionalProperties":true}},
            "anyOf":[{{"required":[{}]}},true]}}}}}}}}"##,
            ids.join(","),
            keys.join(",")
        );
        let keys = PropertyKeys::of(schema.as_bytes()).unwrap();
        let sets = Sets::new(&keys, Direction::ToClaude);
        let alternative = keys.tree.resolve(b"#/properties/rows/items/anyOf/0").unwrap();
        let condition = keys.tree.resolve(b"#/properties/rows/items/if").unwrap();
        let row = |raw: &'static [u8]| Instance {
            raw,
            members: [(b"id".to_vec(), &raw[6..raw.len() - 1])].into_iter().collect(),
        };
        let inside = row(br#"{"id":2000}"#);
        let outside = row(br#"{"id":2001}"#);
        let mut memo = Memo::default();
        assert_eq!(sets.check(condition, &inside, &mut memo), Some(true));
        let mut memo = Memo::default();
        assert_eq!(sets.check(condition, &outside, &mut memo), Some(false));
        // 2000 required keys against a one-member row: missing without scanning them.
        assert_eq!(sets.check(alternative, &inside, &mut Memo::default()), Some(false));
        let client = r##"{"rows":[{"id":2000,"x y":1},{"id":2001,"x y":2}]}"##;
        assert_eq!(
            keys.to_claude(client.as_bytes()),
            br##"{"rows":[{"id":2000,"x_y":1},{"id":2001,"x y":2}]}"##
        );
    }

    #[test]
    fn additional_items_needs_tuple_items() {
        // Without array-valued `items`, `additionalItems` evaluates nothing.
        let ignored = br##"{"properties":{"list":{"additionalItems":true,
            "unevaluatedItems":{"properties":{"x y":{}},"required":["x y"]}}}}"##;
        round_trip(ignored, r##"{"list":[{"x y":1}]}"##, r##"{"list":[{"x_y":1}]}"##);
        let schema_form = br##"{"properties":{"list":{"additionalItems":{"properties":{"x y":{}}}}}}"##;
        round_trip(schema_form, r##"{"list":[{"x_y":1}]}"##, r##"{"list":[{"x_y":1}]}"##);
        // Beside tuple `items`, it covers the rest.
        let tuple = br##"{"properties":{"list":{"items":[true],"additionalItems":{"properties":{"x y":{}}}}}}"##;
        round_trip(
            tuple,
            r##"{"list":[{"x_y":0},{"x y":1}]}"##,
            r##"{"list":[{"x_y":0},{"x_y":1}]}"##,
        );
    }

    #[test]
    fn long_renamed_discriminators_are_read_in_each_spelling() {
        // A 100-byte discriminator Claude rejects becomes a 64-byte name; the branch
        // check finds it in either spelling, as `required`, property and dependent key.
        let long = "k ".repeat(50);
        let claude = "k_".repeat(32);
        let schema = format!(
            r##"{{"properties":{{"rows":{{"items":{{"properties":{{"{long}":{{}}}},
            "oneOf":[{{"properties":{{"{long}":{{"const":"a"}},"d":{{"properties":{{"x y":{{}}}}}}}},"required":["{long}"]}},
                     {{"properties":{{"{long}":{{"const":"b"}},"d":{{"additionalProperties":true}}}}}}],
            "dependentSchemas":{{"{long}":{{"properties":{{"e":{{"properties":{{"z w":{{}}}}}}}}}}}}}}}}}}}}"##
        );
        round_trip(
            schema.as_bytes(),
            &format!(
                r##"{{"rows":[{{"{long}":"a","d":{{"x y":1}},"e":{{"z w":2}}}},{{"{long}":"b","d":{{"x_y":3}}}},{{"e":{{"z w":4}}}}]}}"##
            ),
            &format!(
                r##"{{"rows":[{{"{claude}":"a","d":{{"x_y":1}},"e":{{"z_w":2}}}},{{"{claude}":"b","d":{{"x_y":3}}}},{{"e":{{"z w":4}}}}]}}"##
            ),
        );
    }

    #[test]
    fn type_alternatives_pick_the_instance_shape() {
        let schema = br##"{"properties":{"v":{"anyOf":[
            {"type":"array","items":{"properties":{"x y":{}}}},
            {"type":"object","additionalProperties":{"type":"array"}}
        ]}}}"##;
        round_trip(schema, r##"{"v":[{"x y":1}]}"##, r##"{"v":[{"x_y":1}]}"##);
        round_trip(
            schema,
            r##"{"v":{"x y":[{"x y":1}]}}"##,
            r##"{"v":{"x y":[{"x y":1}]}}"##,
        );
    }

    #[test]
    fn dependent_schemas_apply_when_their_key_is_present() {
        let schema = br##"{"properties":{"flag":{},"data":{}},"dependentSchemas":{"flag":{"properties":{"data":{"properties":{"x y":{}}}}}}}"##;
        round_trip(
            schema,
            r##"{"flag":1,"data":{"x y":1}}"##,
            r##"{"flag":1,"data":{"x_y":1}}"##,
        );
        round_trip(schema, r##"{"data":{"x_y":1}}"##, r##"{"data":{"x_y":1}}"##);
    }

    #[test]
    fn required_names_a_key_where_it_applies() {
        let schema = br##"{"properties":{"x y":{}},"$defs":{"o":{"required":["x y"]}},
            "additionalProperties":{"$ref":"#/$defs/o"}}"##;
        round_trip(schema, r##"{"k":{"x y":1}}"##, r##"{"k":{"x_y":1}}"##);
    }

    #[test]
    fn an_in_place_node_maps_instances_at_its_parents_position() {
        let schema = br##"{"properties":{"p":{"properties":{"x y":{}},"additionalProperties":false,"allOf":[{"const":{"x y":"ok"}}]}}}"##;
        let keys = PropertyKeys::of(schema).unwrap();
        let node = keys.tree.resolve(b"#/properties/p/allOf/0").unwrap();
        assert_eq!(
            keys.instance_to_claude(node, br##"{"x y":"ok"}"##),
            br##"{"x_y":"ok"}"##
        );
        assert_eq!(
            keys.instance_to_client(node, br##"{"x_y":"ok"}"##),
            br##"{"x y":"ok"}"##
        );
        // A node's own position: `p`'s member schema, not the root.
        let p = keys.tree.resolve(b"#/properties/p").unwrap();
        assert_eq!(keys.instance_to_claude(p, br##"{"x y":1}"##), br##"{"x_y":1}"##);
        assert_eq!(keys.instance_to_claude(ROOT, br##"{"x y":1}"##), br##"{"x y":1}"##);
    }

    #[test]
    fn pointers_resolve_escapes_and_refuse_bad_ones() {
        let schema = br##"{"properties":{"a/b~c":{"properties":{"q r":{}}}}}"##;
        let keys = PropertyKeys::of(schema).unwrap();
        assert!(keys.tree.resolve(b"#/properties/a~1b~0c").is_some());
        assert!(keys.tree.resolve(b"#/properties/a%7E1b~0c").is_some());
        assert_eq!(keys.tree.resolve(b"#"), Some(ROOT));
        assert_eq!(keys.tree.resolve(b"#/properties/a~2b"), None);
        assert_eq!(keys.tree.resolve(b"#/properties/%zz"), None);
        assert_eq!(keys.tree.resolve(b"other#/properties"), None);
    }

    #[test]
    fn long_arrays_reuse_their_element_sets() {
        let keys = PropertyKeys::of(br##"{"properties":{"rows":{"items":{"properties":{"a b":{}}}}}}"##).unwrap();
        let rows: Vec<String> = (0..1000).map(|i| format!(r##"{{"a b":{i}}}"##)).collect();
        let client = format!(r##"{{"rows":[{}]}}"##, rows.join(","));
        let claude = keys.to_claude(client.as_bytes());
        assert_eq!(claude, client.replace("a b", "a_b").into_bytes());
        assert_eq!(keys.to_client(&claude), client.into_bytes());
    }
}
