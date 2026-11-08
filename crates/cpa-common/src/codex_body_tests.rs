//! The request boundary's grammar, limits and error policy. Contract-fixture tests live
//! with the executor (`crates/cpa-exec/src/codex_boundary_tests.rs`).

use super::*;

/// JSON text with `~` standing for a backslash, so escapes stay readable here.
fn json(s: &str) -> Vec<u8> {
    s.replace('~', "\\").into_bytes()
}

#[test]
fn valid_json_of_every_shape_is_accepted() {
    for raw in [
        "{}",
        "  {\n\t\"model\" : \"m\" }\r\n",
        r#"{"a":1e400,"b":-0.0,"c":123456789012345678901234567890,"d":0.5E-7,"e":-0}"#,
        r#"{"a":[],"b":{},"c":[[],[{}]],"d":true,"e":false,"f":null,"g":"é ☃"}"#,
        r#"{"x":{"model":1,"stream":"yes","previous_response_id":[]}}"#,
        r#"{"previous_response_id":null,"stream":false}"#,
    ] {
        CodexBody::parse(raw.as_bytes()).unwrap_or_else(|e| panic!("{raw}: {e}"));
    }
    let escapes = json(r#"{"s":"~"~~~/~b~f~n~r~t~u0041~ud83d~ude00"}"#);
    CodexBody::parse(&escapes).unwrap();
}

/// Members point into the input: numbers keep their spelling and nothing is copied.
#[test]
fn members_are_slices_of_the_input() {
    let raw = br#"{"model":"gpt-test","big":123456789012345678901234567890,"tiny":1e-400,"obj":{"k":[1, 2]}}"#;
    let body = CodexBody::parse(raw).unwrap();
    let big = body.get("big").unwrap();
    assert_eq!(big.kind, Kind::Number);
    assert_eq!(big.raw, b"123456789012345678901234567890");
    assert_eq!(body.get("tiny").unwrap().raw, b"1e-400");
    let obj = body.get("obj").unwrap();
    assert_eq!((obj.kind, obj.raw), (Kind::Object, &br#"{"k":[1, 2]}"#[..]));
    let model = body.get("model").unwrap();
    assert_eq!(model.raw, br#""gpt-test""#);
    assert!(matches!(model.key, Cow::Borrowed("model")));
    let inside = raw.as_ptr_range();
    assert!(inside.contains(&big.raw.as_ptr()));
    assert!(body.get("missing").is_none());
}

/// An escaped key is decoded, so it is found under its plain spelling.
#[test]
fn escaped_keys_are_decoded() {
    let raw = json(r#"{"mod~u0065l":"m"}"#);
    let body = CodexBody::parse(&raw).unwrap();
    let model = body.get("model").unwrap();
    assert!(matches!(model.key, Cow::Owned(_)));
    assert_eq!(model.kind, Kind::String);
}

#[test]
fn each_boundary_error_is_explicit() {
    use BodyError::*;
    let model_string = FieldType {
        field: "model",
        expected: "a string",
    };
    let cases: Vec<(Vec<u8>, BodyError)> = vec![
        (b"{\"model\":\"m\xff\"}".to_vec(), InvalidUtf8 { offset: 11 }),
        (br#"{"model":"m"} x"#.to_vec(), Malformed { offset: 14 }),
        (br#"{"model":"m""#.to_vec(), Malformed { offset: 12 }),
        (b"".to_vec(), Malformed { offset: 0 }),
        (b"   ".to_vec(), Malformed { offset: 3 }),
        (br#"{'a':1}"#.to_vec(), Malformed { offset: 1 }),
        (br#"{"a":01}"#.to_vec(), Malformed { offset: 6 }),
        (br#"{"a":1.}"#.to_vec(), Malformed { offset: 7 }),
        (br#"{"a":tru}"#.to_vec(), Malformed { offset: 5 }),
        (br#"{"a":1,}"#.to_vec(), Malformed { offset: 7 }),
        (b"{\"a\":\"x\ty\"}".to_vec(), Malformed { offset: 7 }),
        (json(r#"{"a":"~x"}"#), Malformed { offset: 7 }),
        (json(r#"{"a":"~u12G4"}"#), Malformed { offset: 8 }),
        (b"[1,".to_vec(), Malformed { offset: 3 }),
        (br#"{"model":"a","model":"b"}"#.to_vec(), DuplicateKey { offset: 13 }),
        (json(r#"{"a":1,"~u0061":2}"#), DuplicateKey { offset: 7 }),
        (
            br#"{"input":[{"type":"message","type":"x"}]}"#.to_vec(),
            DuplicateKey { offset: 28 },
        ),
        (br#"[{"model":"m"}]"#.to_vec(), NotObject),
        (br#""text""#.to_vec(), NotObject),
        (b"null".to_vec(), NotObject),
        (br#"{"model":1}"#.to_vec(), model_string),
        (br#"{"model":null}"#.to_vec(), model_string),
        (br#"{"model":["m"]}"#.to_vec(), model_string),
        (
            br#"{"stream":"true"}"#.to_vec(),
            FieldType {
                field: "stream",
                expected: "a boolean",
            },
        ),
        (
            br#"{"previous_response_id":5}"#.to_vec(),
            FieldType {
                field: "previous_response_id",
                expected: "a string or null",
            },
        ),
    ];
    for (raw, expected) in cases {
        let got = CodexBody::parse(&raw).unwrap_err();
        assert_eq!(got, expected, "{}", String::from_utf8_lossy(&raw));
        assert_eq!(got.status(), 400);
    }
}

/// Grammar errors anywhere in the body win over a type error on a routing member.
#[test]
fn the_whole_body_is_validated_before_member_types() {
    let err = CodexBody::parse(br#"{"model":1,"x":{"a":1,"a":2}}"#).unwrap_err();
    assert!(matches!(err, BodyError::DuplicateKey { .. }), "{err:?}");
}

/// A lone or mismatched surrogate escape has no Unicode scalar value.
#[test]
fn lone_surrogates_are_malformed() {
    for raw in [
        r#"{"a":"~ud800"}"#,
        r#"{"a":"~udc00"}"#,
        r#"{"a":"~ud800~u0041"}"#,
        r#"{"a":"~ud800x"}"#,
    ] {
        let err = CodexBody::parse(&json(raw)).unwrap_err();
        assert_eq!(err, BodyError::Malformed { offset: 6 }, "{raw}");
    }
}

/// Duplicate detection keeps working past the keys held on the stack.
#[test]
fn duplicates_are_found_in_large_objects() {
    let mut raw = String::from("{");
    for i in 0..40 {
        raw.push_str(&format!("\"k{i}\":{i},"));
    }
    let ok = format!("{raw}\"k40\":0}}");
    CodexBody::parse(ok.as_bytes()).unwrap();
    let repeated = format!("{raw}\"k3\":0}}");
    let err = CodexBody::parse(repeated.as_bytes()).unwrap_err();
    assert_eq!(err, BodyError::DuplicateKey { offset: raw.len() });
}

#[test]
fn depth_is_bounded() {
    // The root object is level 1; each array adds one.
    let nest = |levels: usize| format!("{{\"a\":{}{}}}", "[".repeat(levels - 1), "]".repeat(levels - 1));
    CodexBody::parse(nest(MAX_DEPTH).as_bytes()).unwrap();
    let err = CodexBody::parse(nest(MAX_DEPTH + 1).as_bytes()).unwrap_err();
    // The first array that is too deep starts after `{"a":` and 99 brackets.
    assert_eq!(
        err,
        BodyError::TooDeep {
            limit: MAX_DEPTH,
            offset: 5 + MAX_DEPTH - 1
        }
    );
    let err = CodexBody::parse(nest(100_000).as_bytes()).unwrap_err();
    assert!(matches!(err, BodyError::TooDeep { .. }), "{err:?}");
}

#[test]
fn size_is_bounded_before_anything_else() {
    let raw = br#"{"model":"m"}"#;
    assert!(CodexBody::parse_limited(raw, raw.len()).is_ok());
    let err = CodexBody::parse_limited(raw, raw.len() - 1).unwrap_err();
    assert_eq!(err, BodyError::TooLarge { limit: raw.len() - 1 });
    assert_eq!(err.status(), 413);
    let err = CodexBody::parse_limited(b"\xff\xff", 1).unwrap_err();
    assert_eq!(err, BodyError::TooLarge { limit: 1 });
}

/// Messages name positions and protocol fields, never body content.
#[test]
fn messages_never_echo_the_body() {
    let secret = "sk-SECRET-VALUE";
    for raw in [
        format!(r#"{{"{secret}":1,"{secret}":2}}"#),
        format!(r#"{{"model":"{secret}"}} {secret}"#),
        format!(r#"{{"stream":"{secret}"}}"#),
        format!("{{\"model\":\"{secret}\u{1}\"}}"),
        format!("[\"{secret}\"]"),
    ] {
        let err = CodexBody::parse(raw.as_bytes()).unwrap_err();
        assert!(!err.to_string().contains("SECRET"), "{err}");
        let exec: ExecError = err.into();
        assert_eq!(exec.scope, FailureScope::Request);
        assert!(!String::from_utf8_lossy(&exec.body).contains("SECRET"));
    }
}

#[test]
fn string_members_decode() {
    let raw = json(r#"{"model":"gpt~u002d5","n":1}"#);
    let body = CodexBody::parse(&raw).unwrap();
    assert_eq!(body.get("model").unwrap().text().as_deref(), Some("gpt-5"));
    assert_eq!(body.get("n").unwrap().text(), None);
    let plain = br#"{"model":"gpt-5"}"#;
    let body = CodexBody::parse(plain).unwrap();
    assert!(matches!(
        body.get("model").unwrap().text(),
        Some(Cow::Borrowed("gpt-5"))
    ));
}

/// Setting a member changes only that member's value bytes, or adds it first.
#[test]
fn with_member_edits_one_value_in_place() {
    let raw = br#"{ "model" : "alias", "n":1.50, "x":{"model":"inner"} }"#;
    let body = CodexBody::parse(raw).unwrap();
    assert_eq!(
        body.with_member("model", r#""gpt-5""#),
        br#"{ "model" : "gpt-5", "n":1.50, "x":{"model":"inner"} }"#
    );
    assert_eq!(
        body.with_member("stream", "true"),
        br#"{"stream":true, "model" : "alias", "n":1.50, "x":{"model":"inner"} }"#
    );
    let empty = CodexBody::parse(b" { } ").unwrap();
    assert_eq!(empty.with_member("stream", "true"), br#" {"stream":true } "#);
    let flag = br#"{"stream":false,"a":[]}"#;
    let body = CodexBody::parse(flag).unwrap();
    assert_eq!(body.with_member("stream", "true"), br#"{"stream":true,"a":[]}"#);
}

/// Removing a member drops it and one separating comma; every other byte stays.
#[test]
fn without_member_removes_one_member_and_its_comma() {
    let cases: [(&[u8], &[u8]); 6] = [
        (
            br#"{"type":"response.create","model":"m","input":[]}"#,
            br#"{"model":"m","input":[]}"#,
        ),
        (br#"{ "type" : "x" ,  "model":"m" }"#, br#"{"model":"m" }"#),
        (
            br#"{"model":"m", "type":"x", "input":[{"type":"message"}]}"#,
            br#"{"model":"m", "input":[{"type":"message"}]}"#,
        ),
        (br#"{"model":"m","type":"x"}"#, br#"{"model":"m"}"#),
        (br#" {"type":"x"} "#, br#" {} "#),
        (br#"{"model":"m","t\u0079pe":"x"}"#, br#"{"model":"m"}"#),
    ];
    for (raw, want) in cases {
        let body = CodexBody::parse(raw).unwrap();
        assert_eq!(body.without_member("type"), want, "{}", String::from_utf8_lossy(raw));
        CodexBody::parse(&body.without_member("type")).unwrap();
    }
    let raw = br#"{"model":"m"}"#;
    assert_eq!(CodexBody::parse(raw).unwrap().without_member("type"), raw);
}

/// Contract probe: routing changes only model/stream, including when both need edits.
/// Unknown values remain raw JSON rather than passing through a serde value tree.
#[test]
fn routing_preserves_unknown_bytes_and_handles_both_member_orders() {
    for (raw, expected) in [
        (
            r#" {"model" : "alias", "stream" : false, "future":{"n":1e400,"s":"\u0061"}} "#,
            r#" {"model" : "upstream", "stream" : true, "future":{"n":1e400,"s":"\u0061"}} "#,
        ),
        (
            r#"{"stream":false,"mod\u0065l":"alias","future":[-0,1E-9]}"#,
            r#"{"stream":true,"mod\u0065l":"upstream","future":[-0,1E-9]}"#,
        ),
        (
            r#" {"future":123456789012345678901234567890} "#,
            r#" {"stream":true,"model":"upstream","future":123456789012345678901234567890} "#,
        ),
        (" { }\n", " {\"stream\":true,\"model\":\"upstream\" }\n"),
        (
            r#"{"stream":false,"future":null}"#,
            r#"{"model":"upstream","stream":true,"future":null}"#,
        ),
        (
            r#"{"model":"alias","future":false}"#,
            r#"{"stream":true,"model":"upstream","future":false}"#,
        ),
    ] {
        let body = CodexBody::parse(raw.as_bytes()).unwrap();
        assert_eq!(body.routed("upstream", true).as_ref(), expected.as_bytes(), "{raw}");
    }
}

#[test]
fn routing_borrows_unchanged_bodies_and_quotes_only_a_changed_model() {
    let raw = br#"{"model":"up\u0073tream","stream":true,"future":1e400}"#;
    let body = CodexBody::parse(raw).unwrap();
    let out = body.routed("upstream", true);
    assert!(matches!(out, Cow::Borrowed(_)));
    assert_eq!(out.as_ptr(), raw.as_ptr());
    let raw = br#"{"model":"upstream","stream":false}"#;
    let body = CodexBody::parse(raw).unwrap();
    assert!(matches!(body.routed("upstream", false), Cow::Borrowed(_)));
    assert_eq!(
        body.routed("a\"b\\c\n", false).as_ref(),
        br#"{"model":"a\"b\\c\n","stream":false}"#
    );
}

/// The HTTP body of a WebSocket frame in one copy: `type` goes with one comma, `model`
/// is routed and `stream: true` is set or added, whatever the member order.
#[test]
fn routing_without_a_member_is_one_edit() {
    for (raw, expected) in [
        (
            r#"{"type":"response.create","model":"alias","input":[{"type":"message"}]}"#,
            r#"{"stream":true,"model":"upstream","input":[{"type":"message"}]}"#,
        ),
        (
            r#"{"model":"upstream","type":"response.create","stream":false,"n":1.50}"#,
            r#"{"model":"upstream","stream":true,"n":1.50}"#,
        ),
        (
            r#" { "type" : "response.create" , "stream":true, "model":"upstream" } "#,
            r#" {"stream":true, "model":"upstream" } "#,
        ),
        (r#"{"type":"response.create"}"#, r#"{"stream":true,"model":"upstream"}"#),
        (
            r#"{"input":[],"model":"upstream","stream":true,"type":"response.create"}"#,
            r#"{"input":[],"model":"upstream","stream":true}"#,
        ),
        (
            r#"{"t\u0079pe":"response.create","model":"upstream","stream":true,"x":1e400}"#,
            r#"{"model":"upstream","stream":true,"x":1e400}"#,
        ),
    ] {
        let body = CodexBody::parse(raw.as_bytes()).unwrap();
        let out = body.routed_without("upstream", true, "type");
        assert_eq!(out.as_ref(), expected.as_bytes(), "{raw}");
        CodexBody::parse(&out).unwrap();
    }
    // Nothing to change: the caller's bytes, borrowed.
    let raw = br#"{"model":"upstream","stream":true}"#;
    let body = CodexBody::parse(raw).unwrap();
    assert!(matches!(
        body.routed_without("upstream", true, "type"),
        Cow::Borrowed(_)
    ));
}

/// `routed_without` gives what the separate steps give, on every member order: remove
/// the member, parse again, then route.
#[test]
fn routing_without_a_member_matches_the_separate_steps() {
    let members = [
        r#""type":"response.create""#,
        r#""model":"alias""#,
        r#""stream":false"#,
        r#""input":[{"type":"message","n":-0.0}]"#,
    ];
    let mut checked = 0;
    for mask in 0u32..(1 << members.len()) {
        let chosen: Vec<&str> = (0..members.len())
            .filter(|i| mask & (1 << i) != 0)
            .map(|i| members[i])
            .collect();
        for rotation in 0..chosen.len().max(1) {
            let mut order = chosen.clone();
            order.rotate_left(rotation);
            for separator in [",", " , "] {
                let raw = format!("{{{}}}", order.join(separator));
                let body = CodexBody::parse(raw.as_bytes()).unwrap();
                for (model, stream) in [("alias", false), ("upstream", false), ("upstream", true)] {
                    let untagged = body.without_member("type");
                    let stepwise = CodexBody::parse(&untagged).unwrap().routed(model, stream).into_owned();
                    let once = body.routed_without(model, stream, "type");
                    assert_eq!(
                        String::from_utf8_lossy(&once),
                        String::from_utf8_lossy(&stepwise),
                        "{raw} {model} {stream}"
                    );
                    checked += 1;
                }
            }
        }
    }
    assert!(checked > 100, "{checked}");
}

/// An array member's elements are slices of the body; other values have none.
#[test]
fn array_items_are_slices_of_the_input() {
    let raw = br#"{"images":[ {"image_url":"a"} ,"x",1e400,[[]],null ],"empty":[ ],"s":"[1]"}"#;
    let body = CodexBody::parse_object(raw).unwrap();
    let items = body.get("images").unwrap().items().unwrap();
    let expected: [&[u8]; 5] = [br#"{"image_url":"a"}"#, br#""x""#, b"1e400", b"[[]]", b"null"];
    assert_eq!(items, expected);
    assert!(raw.as_ptr_range().contains(&items[2].as_ptr()));
    assert_eq!(body.get("empty").unwrap().items(), Some(vec![]));
    assert_eq!(body.get("s").unwrap().items(), None);
    let keys: Vec<&str> = body.members().iter().map(|m| &*m.key).collect();
    assert_eq!(keys, ["images", "empty", "s"]);
    assert_eq!(body.raw(), raw);
}

/// `parse_object` applies every rule but the Responses member types.
#[test]
fn object_parsing_skips_only_the_responses_member_types() {
    for raw in [r#"{"model":5}"#, r#"{"stream":"yes"}"#, r#"{"previous_response_id":1}"#] {
        assert!(CodexBody::parse(raw.as_bytes()).is_err(), "{raw}");
        CodexBody::parse_object(raw.as_bytes()).unwrap();
    }
    for raw in [
        &b"{\"a\":\"\xff\"}"[..],
        br#"{"a":1,"a":2}"#,
        br#"{"a":1} x"#,
        br#"[1]"#,
        &json(r#"{"a":"~ud800"}"#)[..],
    ] {
        assert!(
            CodexBody::parse_object(raw).is_err(),
            "{}",
            String::from_utf8_lossy(raw)
        );
    }
}
