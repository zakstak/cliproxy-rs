//! Local `$ref` inlining for tool schemas (Go `InlineLocalRefs`,
//! internal/util/gemini_schema.go), used by the xAI request builder.
//!
//! The schema is decoded into Go's `any` model, its references are resolved, and it is
//! marshalled again, as Go does. Go's Gemini and Antigravity schema cleaners, which share
//! that file, went with those providers (issue #49). Pass a single schema, never a whole
//! request document.

use crate::json::{self as gj, GoValue};
use std::collections::{BTreeMap, HashMap};

/// InlineLocalRefs.
pub fn inline_local_refs(schema: &[u8]) -> Vec<u8> {
    on_ref_stack(schema, || inline_refs(schema.to_vec()))
}

/// Runs `f` on a scoped thread with Go's maximum goroutine stack (1 GiB, reserved, not
/// committed) when `schema` holds a `"$ref"`. Inlining nests each expansion inside its
/// reference, so a chain of definitions in a small input expands to thousands of levels
/// (about 6,000 within the budget), and the passes recurse per level like Go's, whose
/// stacks grow. Input-depth guards cannot see that. Cost: one thread spawn per schema with
/// a reference, on the request path of every xAI request whose tool schemas carry one.
fn on_ref_stack<T: Send>(schema: &[u8], f: impl FnOnce() -> T + Send) -> T {
    if !contains_seq(schema, br#""$ref""#) {
        return f();
    }
    std::thread::scope(|scope| {
        let handle = std::thread::Builder::new()
            .name("schema-refs".into())
            .stack_size(1 << 30)
            .spawn_scoped(scope, f)
            .expect("spawn the schema inlining thread");
        handle.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}

// ---------------------------------------------------------------------------------------
// Path helpers

/// literalValueKeywords: their values are client data, never subschemas.
fn is_literal_value_keyword(key: &[u8]) -> bool {
    matches!(key, b"default" | b"example" | b"examples" | b"const" | b"enum")
}

fn contains_seq(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || haystack.windows(needle.len()).any(|w| w == needle)
}

/// mergeHint: an existing copy of the hint is kept as is.
fn merge_hint(existing: &[u8], hint: &[u8]) -> Vec<u8> {
    if existing.is_empty() {
        return hint.to_vec();
    }
    if existing == hint
        || existing.starts_with(&[hint, b" ("].concat())
        || contains_seq(existing, &[&b"("[..], hint, b")"].concat())
    {
        return existing.to_vec();
    }
    [existing, b" (", hint, b")"].concat()
}

fn ref_name(reference: &[u8]) -> Vec<u8> {
    match reference.iter().rposition(|&c| c == b'/') {
        Some(i) if i + 1 < reference.len() => pointer_unescape(&reference[i + 1..]),
        _ => reference.to_vec(),
    }
}

/// `~1` -> `/`, then `~0` -> `~` (two strings.ReplaceAll passes).
fn pointer_unescape(part: &[u8]) -> Vec<u8> {
    let replace = |s: &[u8], from: &[u8], to: u8| {
        let mut out = Vec::with_capacity(s.len());
        let mut i = 0;
        while i < s.len() {
            if s[i..].starts_with(from) {
                out.push(to);
                i += from.len();
            } else {
                out.push(s[i]);
                i += 1;
            }
        }
        out
    };
    replace(&replace(part, b"~1", b'/'), b"~0", b'~')
}

// ---------------------------------------------------------------------------------------
// Go's `any` model

type Map = BTreeMap<String, GoValue>;

/// json.Decoder.Decode: only the first value is decoded; the decoder reports bytes after
/// it on the next call, so they never fail this one.
fn decode_first(text: &[u8]) -> Option<GoValue> {
    let start = text.iter().position(|c| !matches!(c, b' ' | b'\t' | b'\n' | b'\r'))?;
    let rest = &text[start..];
    let end = match rest[0] {
        b't' | b'n' => 4,
        b'f' => 5,
        _ => gj::parse(rest).raw.len(),
    };
    GoValue::parse(rest.get(..end)?)
}

// ---------------------------------------------------------------------------------------
// References

/// maxInlinedSchemaBytes: shared definitions fan out once per reference, so expansions past
/// this budget degrade to a typed hint like cycles do.
const MAX_INLINED_SCHEMA_BYTES: usize = 256 * 1024;
/// maxFallbackDescriptionBytes: the longest target description a degraded reference copies.
const MAX_FALLBACK_DESCRIPTION_BYTES: usize = 512;

type Active = std::collections::HashSet<String>;

/// The inlining budget of one call, and each reference's target size. Targets never
/// change within a call, so a reference used many times is sized once (Go sizes it on
/// every use, which is 14 s for 8,000 references to one 300 KB definition).
struct Budget {
    left: usize,
    sizes: HashMap<String, usize>,
}

impl Budget {
    fn new() -> Self {
        Self {
            left: MAX_INLINED_SCHEMA_BYTES,
            sizes: HashMap::new(),
        }
    }

    /// targetExpansionSize of `reference`'s `target`, memoized.
    fn target_size(&mut self, reference: &str, target: &GoValue) -> usize {
        if let Some(size) = self.sizes.get(reference) {
            return *size;
        }
        let size = json_value_size(target);
        self.sizes.insert(reference.to_owned(), size);
        size
    }
}

fn is_name_map_keyword(key: &str) -> bool {
    matches!(
        key,
        "properties" | "patternProperties" | "dependentSchemas" | "dependencies" | "$defs" | "definitions"
    )
}

/// inlineLocalRefs: every reference gets its own copy while the budget lasts, sibling
/// keywords override the target, and cycles end as a typed hint.
fn inline_refs(json: Vec<u8>) -> Vec<u8> {
    if !contains_seq(&json, br#""$ref""#) {
        return json;
    }
    let Some(root) = decode_first(&json) else {
        return json;
    };
    let mut budget = Budget::new();
    resolve_schema_root(&root, &mut budget).marshal()
}

/// resolveSchemaRoot: the schema body resolves before the definition containers.
fn resolve_schema_root(root: &GoValue, budget: &mut Budget) -> GoValue {
    let GoValue::Object(root_map) = root else {
        return resolve_local_refs(root, root, &mut Active::new(), budget);
    };
    let mut out = Map::new();
    for (key, item) in root_map {
        match key.as_str() {
            "$defs" | "definitions" => {
                out.insert(key.clone(), GoValue::Null); // resolved after the body
            }
            "$ref" => {
                out.insert(key.clone(), item.clone());
            }
            _ => {
                let resolved = resolve_schema_entry(root, key, item, &mut Active::new(), budget);
                out.insert(key.clone(), resolved);
            }
        }
    }
    if let Some(GoValue::String(reference)) = root_map.get("$ref")
        && reference.starts_with("#/")
        && let Some(target) = resolve_pointer(root, reference)
    {
        let size = budget.target_size(reference, target);
        if size <= budget.left {
            budget.left -= size;
            let mut active = Active::from([reference.clone()]);
            match resolve_local_refs(root, target, &mut active, budget).into_object() {
                Ok(mut merged) => {
                    for (key, value) in std::mem::take(&mut out) {
                        if key != "$ref" {
                            merged.insert(key, value);
                        }
                    }
                    out = merged;
                }
                Err(_) => out = resolve_ref_fallback(root, &out, target, reference, &mut Active::new(), budget),
            }
        } else {
            out = resolve_ref_fallback(root, &out, target, reference, &mut Active::new(), budget);
        }
        out.remove("$ref");
    }
    for key in ["$defs", "definitions"] {
        if let Some(item) = root_map.get(key) {
            let resolved = resolve_schema_entry(root, key, item, &mut Active::new(), budget);
            out.insert(key.into(), resolved);
        }
    }
    GoValue::Object(out)
}

/// jsonValueSize: the bytes encoding/json writes for a decoded value.
fn json_value_size(value: &GoValue) -> usize {
    match value {
        GoValue::Null => 4,
        GoValue::Bool(_) => 5,
        GoValue::Number(n) => n.len(),
        GoValue::String(s) => json_string_encoded_size(s),
        GoValue::Array(items) => 2 + items.iter().map(|i| json_value_size(i) + 1).sum::<usize>(),
        GoValue::Object(map) => {
            2 + map
                .iter()
                .map(|(k, v)| json_string_encoded_size(k) + 1 + json_value_size(v) + 1)
                .sum::<usize>()
        }
    }
}

/// jsonStringEncodedSize: with encoding/json's HTML escaping.
fn json_string_encoded_size(s: &str) -> usize {
    let b = s.as_bytes();
    let mut size = 2;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        size += match c {
            b'\n' | b'\r' | b'\t' => 2,
            0..0x20 => 6,
            b'"' | b'\\' => 2,
            b'<' | b'>' | b'&' => 6,
            0xe2 if i + 2 < b.len() && b[i + 1] == 0x80 && matches!(b[i + 2], 0xa8 | 0xa9) => {
                i += 2;
                6
            }
            _ => 1,
        };
        i += 1;
    }
    size
}

fn resolve_local_refs(root: &GoValue, value: &GoValue, active: &mut Active, budget: &mut Budget) -> GoValue {
    match value {
        GoValue::Array(items) => GoValue::Array(
            items
                .iter()
                .map(|i| resolve_local_refs(root, i, active, budget))
                .collect(),
        ),
        GoValue::Object(node) => {
            if let Some(GoValue::String(reference)) = node.get("$ref")
                && reference.starts_with("#/")
                && let Some(target) = resolve_pointer(root, reference)
            {
                if active.contains(reference) {
                    return GoValue::Object(resolve_ref_fallback(root, node, target, reference, active, budget));
                }
                // Each reference charges its stored target; nested references charge their own.
                let size = budget.target_size(reference, target);
                if size > budget.left {
                    return GoValue::Object(resolve_ref_fallback(root, node, target, reference, active, budget));
                }
                budget.left -= size;
                active.insert(reference.clone());
                let resolved = resolve_local_refs(root, target, active, budget);
                active.remove(reference);
                if let Ok(target_map) = resolved.into_object() {
                    let mut out = target_map;
                    for (key, item) in node {
                        if key == "$ref" {
                            continue;
                        }
                        let resolved = resolve_schema_entry(root, key, item, active, budget);
                        out.insert(key.clone(), resolved);
                    }
                    return GoValue::Object(out);
                }
            }
            let mut out = Map::new();
            for (key, item) in node {
                let resolved = resolve_schema_entry(root, key, item, active, budget);
                out.insert(key.clone(), resolved);
            }
            GoValue::Object(out)
        }
        other => other.clone(),
    }
}

/// resolveRefFallback: a reference that cannot expand (cycle or spent budget) becomes a
/// typed hint, and its sibling subschemas still resolve.
fn resolve_ref_fallback(
    root: &GoValue,
    node: &Map,
    target: &GoValue,
    reference: &str,
    active: &mut Active,
    budget: &mut Budget,
) -> Map {
    let mut out = cyclic_ref_fallback(node, target, reference);
    if let GoValue::Object(target_map) = target
        && let Some(GoValue::String(description)) = target_map.get("description")
        && !description.is_empty()
        && description.len() <= MAX_FALLBACK_DESCRIPTION_BYTES
        && !node.contains_key("description")
    {
        let cost = json_string_encoded_size(description);
        if cost > budget.left {
            let hint = [&b"See: "[..], &ref_name(reference.as_bytes())].concat();
            out.insert(
                "description".into(),
                GoValue::String(String::from_utf8_lossy(&hint).into_owned()),
            );
        } else {
            budget.left -= cost;
        }
    }
    for (key, item) in node {
        if key == "$ref" || key == "description" {
            continue;
        }
        let resolved = resolve_schema_entry(root, key, item, active, budget);
        out.insert(key.clone(), resolved);
    }
    out
}

/// resolveSchemaEntry: literal values are copied verbatim; name-map entries are subschemas.
fn resolve_schema_entry(
    root: &GoValue,
    key: &str,
    item: &GoValue,
    active: &mut Active,
    budget: &mut Budget,
) -> GoValue {
    if is_literal_value_keyword(key.as_bytes()) {
        return item.clone();
    }
    if is_name_map_keyword(key)
        && let GoValue::Object(names) = item
    {
        return GoValue::Object(
            names
                .iter()
                .map(|(name, v)| (name.clone(), resolve_local_refs(root, v, active, budget)))
                .collect(),
        );
    }
    resolve_local_refs(root, item, active, budget)
}

fn resolve_pointer<'a>(root: &'a GoValue, reference: &str) -> Option<&'a GoValue> {
    let mut current = root;
    for raw in reference["#/".len()..].split('/') {
        let part = String::from_utf8(pointer_unescape(raw.as_bytes())).unwrap_or_default();
        current = match current {
            GoValue::Object(map) => map.get(&part)?,
            GoValue::Array(items) => {
                let index: i64 = part.parse().ok()?;
                items.get(usize::try_from(index).ok()?)?
            }
            _ => return None,
        };
    }
    Some(current)
}

fn cyclic_ref_fallback(node: &Map, target: &GoValue, reference: &str) -> Map {
    let mut out = Map::new();
    if let GoValue::Object(target) = target {
        for key in ["type", "nullable", "description"] {
            if let Some(v) = target.get(key) {
                if key == "description" && matches!(v, GoValue::String(d) if d.len() > MAX_FALLBACK_DESCRIPTION_BYTES) {
                    continue;
                }
                out.insert(key.into(), v.clone());
            }
        }
    }
    for (key, value) in node {
        if key != "$ref" {
            out.insert(key.clone(), value.clone());
        }
    }
    let hint = [&b"See: "[..], &ref_name(reference.as_bytes())].concat();
    let description = match out.get("description") {
        Some(GoValue::String(d)) if !d.is_empty() => merge_hint(d.as_bytes(), &hint),
        _ => hint,
    };
    out.insert(
        "description".into(),
        GoValue::String(String::from_utf8_lossy(&description).into_owned()),
    );
    out
}
