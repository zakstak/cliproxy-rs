//! JSON behavior contracts: semantic reads and edits with untouched wire bytes preserved.
use cliproxy_internal::protocol::json::{self as gj, Kind};
use serde_json::{Value, json};

fn value(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).expect("the edited document is valid JSON")
}

#[test]
fn reads_supported_paths_and_preserves_raw_number_spelling() {
    let input = br#"{ "metadata":{"user_id":"u\u0031"}, "items":[{"id":1},{"id":2}], "n":9007199254740993, "decimal":1.2300e+04, "flag":true, "nil":null }"#;
    for (path, expected) in [
        ("metadata.user_id", json!("u1")),
        ("items.1.id", json!(2)),
        ("flag", json!(true)),
        ("nil", Value::Null),
    ] {
        let got = gj::get(input, path);
        assert!(got.exists(), "{path}");
        assert_eq!(value(got.raw()), expected, "{path}");
    }
    assert_eq!(gj::get(input, "n").raw(), b"9007199254740993");
    assert_eq!(gj::get(input, "decimal").raw(), b"1.2300e+04");
    assert_eq!(gj::get(input, "metadata.user_id").str(), "u1");
    assert!(!gj::get(input, "missing").exists());
    assert!(!gj::get(input, "items.9.id").exists());
    assert_eq!(gj::get(input, "items").array().len(), 2);
    assert_eq!(gj::get(input, "n").kind, Kind::Number);
    let mut keys = vec![];
    gj::get(input, "metadata").each(|key, _| {
        keys.push(key.str().into_owned());
        true
    });
    assert_eq!(keys, ["user_id"]);
}

#[test]
fn edits_change_the_selected_value_and_preserve_unrelated_bytes() {
    let input = br#"{ "untouched" : {"n":9007199254740993,"s":"\u0061"}, "items":[{"id":1},{"id":2}], "old":false }"#;
    let untouched = br#""untouched" : {"n":9007199254740993,"s":"\u0061"}"#;
    for (path, replacement) in [
        ("old", json!(true)),
        ("items.1.id", json!(7)),
        ("new.nested", json!([1, "x"])),
        ("items.0", json!({"ok":true})),
    ] {
        let output = gj::try_set_raw(input, path, replacement.to_string()).unwrap();
        assert_eq!(value(gj::get(&output, path).raw()), replacement, "{path}");
        assert!(output.windows(untouched.len()).any(|span| span == untouched));
        let deleted = gj::try_delete(&output, path).unwrap();
        assert!(value(&deleted).is_object());
        assert!(deleted.windows(untouched.len()).any(|span| span == untouched));
    }
    assert_eq!(gj::try_delete(input, "missing").unwrap(), input);
    let output = gj::try_set_str(input, "text", "line\n\"quoted\"\\é<&>").unwrap();
    assert_eq!(value(&output)["text"], "line\n\"quoted\"\\é<&>");
}

#[test]
fn escaped_member_deletion_keeps_valid_json() {
    // Issue #146: deleting an escaped key removes the whole member.
    for (input, path, expected) in [
        (r#"{"\"x":1,"y":2}"#, r#"\"x"#, json!({"y":2})),
        (r#"{"y":2,"a\\":1}"#, r#"a\\"#, json!({"y":2})),
        (
            r#"{"outer":{"\"x":1,"keep":2}}"#,
            r#"outer.\"x"#,
            json!({"outer":{"keep":2}}),
        ),
    ] {
        assert_eq!(value(&gj::try_delete(input.as_bytes(), path).unwrap()), expected);
    }
}

#[test]
fn strings_and_finite_numbers_round_trip_as_json_values() {
    for text in ["", "plain", "é😀", "<&>", "\u{2028}\u{2029}", "line\n\t\r\0\"\\"] {
        assert_eq!(value(&gj::quote(text)), json!(text));
        for html in [false, true] {
            let mut output = vec![];
            gj::marshal_str(&mut output, text.as_bytes(), html);
            assert_eq!(value(&output), json!(text));
        }
    }
    for number in [0.0, -0.0, 1.5, 1e-7, 1e21, f64::MIN_POSITIVE, f64::MAX, -42.25] {
        let encoded = gj::json_float(number).unwrap();
        assert_eq!(encoded.parse::<f64>().unwrap(), number);
        assert_eq!(gj::fmt_float(number).parse::<f64>().unwrap(), number);
    }
    for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(gj::json_float(invalid).is_none());
    }
}

#[test]
fn number_preserving_decode_keeps_precision_and_rejects_invalid_documents() {
    for input in [
        r#"{"n":9007199254740993,"fraction":1.2300e+04}"#,
        r#"[0,-1,true,null,"é",{"x":2}]"#,
    ] {
        let out = gj::GoValue::parse(input.as_bytes()).unwrap().marshal();
        assert_eq!(value(&out), value(input.as_bytes()));
    }
    let output = gj::GoValue::parse(br#"{"n":9007199254740993,"decimal":1.2300e+04,"huge":1e400}"#)
        .unwrap()
        .marshal();
    for spelling in [b"9007199254740993".as_slice(), b"1.2300e+04", b"1e400"] {
        assert!(output.windows(spelling.len()).any(|span| span == spelling));
    }
    assert!(gj::GoValue::parse_f64(b"[1e400]").is_none());
    for invalid in [b"{".as_slice(), b"[1,]", b"{\"n\":}", b"null trailing"] {
        assert!(gj::GoValue::parse(invalid).is_none());
        assert!(!gj::std_valid(invalid));
    }
}

/// A validated document nests at most 10000 arrays and objects; brackets inside strings
/// do not count.
#[test]
fn std_valid_refuses_documents_nested_deeper_than_10000() {
    let nested = |n: usize| [vec![b'['; n], vec![b']'; n]].concat();
    assert!(cliproxy_internal::protocol::json::std_valid(&nested(10_000)));
    assert!(!cliproxy_internal::protocol::json::std_valid(&nested(10_001)));
    assert!(cliproxy_internal::protocol::json::std_valid(br#"{"a":"[[[\"]]]"}"#));
    assert!(!cliproxy_internal::protocol::json::std_valid(b"{"));
}

/// Issue #146: reads and edits descend once per path component. A path of more than
/// `WALK_DEPTH_LIMIT` components is not found and cannot be edited, instead of
/// overflowing a 2 MiB worker stack; one at the limit still works.
#[test]
fn paths_beyond_the_walk_limit_are_refused_on_a_worker_stack() {
    let on_worker = |f: Box<dyn FnOnce() + Send>| {
        std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(f)
            .unwrap()
            .join()
            .unwrap()
    };
    let doc = |depth: usize| format!("{}1{}", r#"{"a":"#.repeat(depth), "}".repeat(depth));
    let path = |depth: usize| vec!["a"; depth].join(".");
    on_worker(Box::new(move || {
        let limit = gj::WALK_DEPTH_LIMIT;
        let at = doc(limit);
        assert_eq!(gj::get(at.as_bytes(), &path(limit)).raw(), b"1");
        assert_eq!(gj::try_delete(at.as_bytes(), &path(limit)).unwrap().len(), at.len() - 5);
        assert!(gj::try_set_raw(b"{}", &path(limit), "2").is_ok());
        // An escaped dot is part of a key, not a separator.
        assert!(!gj::get(br#"{"k":1}"#, &format!("{}k", r"\.".repeat(limit + 1))).exists());
        let deep = doc(2_000);
        let long = path(2_000);
        assert!(!gj::get(deep.as_bytes(), &long).exists());
        let message = format!("path has more than {limit} components");
        assert_eq!(gj::try_delete(deep.as_bytes(), &long), Err(message.clone()));
        assert_eq!(gj::try_set_raw(deep.as_bytes(), &long, "2"), Err(message));
        let mut out = deep.clone().into_bytes();
        assert!(!gj::set_raw(&mut out, &long, "2"));
        assert_eq!(out, deep.as_bytes(), "a refused edit keeps the document");
    }));
}
