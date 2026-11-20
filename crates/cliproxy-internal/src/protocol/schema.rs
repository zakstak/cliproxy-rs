//! Go `internal/util` JSON Schema helpers shared by translators and executors
//! (claude_schema.go).

/// util.HasUnsupportedUnicodePropertyEscape: `\p{`, `\P{` or `\0` outside an escaped
/// backslash.
pub fn has_unsupported_unicode_property_escape(pattern: &[u8]) -> bool {
    let mut i = 0;
    while i < pattern.len() {
        if pattern[i] != b'\\' {
            i += 1;
            continue;
        }
        let Some(&next) = pattern.get(i + 1) else { break };
        if matches!(next, b'p' | b'P') && pattern.get(i + 2) == Some(&b'{') {
            return true;
        }
        if next == b'0' {
            return true;
        }
        i += 2;
    }
    false
}

/// util.SchemaMapKeywords: keywords whose values map names to subschemas.
pub const SCHEMA_MAP_KEYWORDS: [&[u8]; 6] = [
    b"properties",
    b"$defs",
    b"definitions",
    b"patternProperties",
    b"dependentSchemas",
    b"dependencies",
];

/// util.SchemaValueKeywords: keywords holding one subschema or a list of them.
pub const SCHEMA_VALUE_KEYWORDS: [&[u8]; 16] = [
    b"items",
    b"prefixItems",
    b"contains",
    b"additionalProperties",
    b"propertyNames",
    b"unevaluatedProperties",
    b"unevaluatedItems",
    b"additionalItems",
    b"contentSchema",
    b"anyOf",
    b"oneOf",
    b"allOf",
    b"not",
    b"if",
    b"then",
    b"else",
];
