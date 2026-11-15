//! Payload selector behavior for the supported comparison operators.
use cpa_common::json;
use serde_json::json;

fn matches(item: &[u8], term: &str) -> bool {
    let path = format!("#({term})");
    let (query, end) = json::parse_query(path.as_bytes()).expect("supported selector");
    assert_eq!(end, path.len());
    query.matches_raw(item)
}

#[test]
fn numeric_predicates_follow_numeric_ordering() {
    for left in [-10, -1, 0, 1, 10] {
        for right in [-10, -1, 0, 1, 10] {
            for (op, expected) in [
                ("==", left == right),
                ("!=", left != right),
                ("<", left < right),
                ("<=", left <= right),
                (">", left > right),
                (">=", left >= right),
            ] {
                let item = json!({"value":left}).to_string();
                assert_eq!(matches(item.as_bytes(), &format!("value{op}{right}")), expected);
            }
        }
    }
}

#[test]
fn string_and_presence_predicates_select_only_matching_members() {
    for (body, term, expected) in [
        (r#"{"name":"alpha"}"#, r#"name=="alpha""#, true),
        (r#"{"name":"alpha"}"#, r#"name!="alpha""#, false),
        (r#"{"name":"alpha"}"#, r#"name%"a*""#, true),
        (r#"{"name":"alpha"}"#, r#"name!%"a*""#, false),
        (r#"{"name":"é"}"#, r#"name=="é""#, true),
        (r#"{"value":true}"#, "value==true", true),
        (r#"{"value":false}"#, "value==true", false),
        (r#"{"value":null}"#, "value", true),
        (r#"{"other":1}"#, "value", false),
        (r#"{"nested":{"value":2}}"#, "nested.value==2", true),
    ] {
        assert_eq!(matches(body.as_bytes(), term), expected, "{term} on {body}");
    }
    for bad in [b"#(".as_slice(), b"#(value==\"unterminated)", b"#(nested.(value)"] {
        assert!(json::parse_query(bad).is_none());
    }
}

#[test]
fn predicate_filters_keep_array_order() {
    let body = br#"{"items":[{"id":1,"keep":true},{"id":2,"keep":false},{"id":3,"keep":true}]}"#;
    let ids = json::get(body, "items")
        .array()
        .into_iter()
        .filter(|item| matches(item.raw(), "keep==true"))
        .map(|item| item.get("id").int())
        .collect::<Vec<_>>();
    assert_eq!(ids, [1, 3]);
}

#[test]
fn predicates_match_unicode_characters_empty_strings_and_scalar_roots() {
    for (body, term, expected) in [
        (r#"{"name":"é"}"#, r#"name%"?""#, true),
        (r#"{"name":"😀"}"#, r#"name%"?""#, true),
        (r#"{"name":"é😀"}"#, r#"name%"?""#, false),
        (r#"{"name":"é😀"}"#, r#"name%"??""#, true),
        (r#"{"name":""}"#, r#"name=="""#, true),
        (r#"{"name":""}"#, r#"name!="""#, false),
        (r#"{"name":""}"#, r#"name!="present""#, true),
        ("42", "==42", true),
        ("42", ">43", false),
        ("-2", "<0", true),
        ("true", "==true", true),
        ("false", "!=true", true),
        (r#""é""#, r#"%"?""#, true),
        (r#""""#, r#"=="""#, true),
    ] {
        assert_eq!(matches(body.as_bytes(), term), expected, "{term} on {body}");
    }
}
