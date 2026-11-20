//! Local JSON Schema references, using small schemas and semantic assertions.
use cliproxy_internal::protocol::schema_refs as gs;
use serde_json::{Value, json};

fn inline(schema: Value) -> Value {
    serde_json::from_slice(&gs::inline_local_refs(&serde_json::to_vec(&schema).unwrap())).unwrap()
}

#[test]
fn local_references_inline_targets_and_keep_sibling_constraints() {
    let schema = json!({"$defs":{"label":{"type":"string","minLength":2,"description":"definition"}},"type":"object","properties":{"name":{"$ref":"#/$defs/label","description":"caller","maxLength":8}}});
    let result = inline(schema);
    assert_eq!(
        result["properties"]["name"],
        json!({"type":"string","minLength":2,"maxLength":8,"description":"caller"})
    );
}

#[test]
fn pointer_escapes_and_chained_references_resolve() {
    let result = inline(
        json!({"$defs":{"a/b~c":{"type":"integer"},"alias":{"$ref":"#/$defs/a~1b~0c"}},"properties":{"value":{"$ref":"#/$defs/alias"}}}),
    );
    assert_eq!(result["properties"]["value"], json!({"type":"integer"}));
}

#[test]
fn unresolved_external_and_cyclic_references_remain_valid_and_bounded() {
    for schema in [
        json!({"$ref":"https://example.invalid/schema","description":"remote"}),
        json!({"$ref":"#/$defs/missing","description":"missing"}),
        json!({"$defs":{"node":{"type":"object","properties":{"next":{"$ref":"#/$defs/node"}}}},"$ref":"#/$defs/node"}),
    ] {
        let output = inline(schema);
        assert!(output.is_object());
        assert!(output.to_string().len() < 4096, "cycles must degrade to a bounded hint");
    }
}

#[test]
fn schemas_without_references_keep_their_wire_bytes() {
    let input = br#"{ "properties": {"x":{"type":"string"}}, "default":{"$ref":"literal client data"} }"#;
    let output = gs::inline_local_refs(input);
    let parsed: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(parsed["default"], json!({"$ref":"literal client data"}));
    let plain = br#"{ "type" : "string", "description" : "\u0061" }"#;
    assert_eq!(gs::inline_local_refs(plain), plain);
}

/// A chain of 1,000 definitions in a 5-deep schema expands to about 2,000 nesting levels
/// within the inlining budget. Inlining runs on its own deep stack, so a caller on a 2 MiB
/// worker stack completes instead of aborting.
#[test]
fn a_definition_chain_expands_on_a_small_stack() {
    const N: usize = 1000;
    let mut defs = String::new();
    for i in 0..N {
        defs += &format!(
            r##""D{i}":{{"type":"object","properties":{{"a":{{"$ref":"#/$defs/D{}"}}}}}},"##,
            i + 1
        );
    }
    defs += &format!(r#""D{N}":{{"type":"string"}}"#);
    let schema = format!(
        r##"{{"type":"object","properties":{{"x":{{"type":"object","properties":{{"y":{{"$ref":"#/$defs/D0"}}}}}}}},"$defs":{{{defs}}}}}"##
    );
    let inlined = std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || gs::inline_local_refs(schema.as_bytes()))
        .unwrap()
        .join()
        .unwrap();
    let expanded = inlined.windows(5).filter(|w| w == br#""a":{"#).count();
    assert!(expanded >= N, "{expanded} of {N} references expanded");
}
