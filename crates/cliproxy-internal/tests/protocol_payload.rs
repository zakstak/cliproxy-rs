//! Operator payload and custom-header behavior, independent of recorder output.
use cliproxy_internal::config::Config;
use cliproxy_internal::protocol::{headers, payload};
use http::HeaderMap;
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn request<'a>(body: &'a [u8], headers: Option<&'a HeaderMap>) -> payload::Request<'a> {
    payload::Request {
        target_executor: "claude",
        model: "model",
        requested_model: "model",
        protocol: "claude",
        from_protocol: "claude",
        root: "",
        original: body,
        request_path: "/v1/messages",
        headers,
    }
}

#[test]
fn payload_defaults_overrides_and_filters_follow_operator_rules() {
    let cfg = Config::parse(
        r#"payload:
  default:
    - models: [{name: "model"}]
      params: {temperature: 0.5, max_tokens: 500}
  override:
    - models: [{name: "model"}]
      params: {max_tokens: 1000}
  override-raw:
    - models: [{name: "model"}]
      params: {"thinking": '{"type":"enabled","budget_tokens":1024}'}
  filter:
    - models: [{name: "model"}]
      params: ["remove"]
"#,
    )
    .unwrap();
    let rules = payload::Rules::of(&cfg);
    assert!(std::sync::Arc::ptr_eq(&rules, &payload::Rules::of(&cfg)));
    let input = br#"{ "temperature":0.2, "remove":true, "untouched":{"n":9007199254740993,"s":"\u0061"} }"#;
    let (out, touched) = payload::apply_tracked(
        &rules,
        &request(input, None),
        input.to_vec(),
        &["max_tokens", "thinking", "remove"],
    );
    let parsed: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(parsed["temperature"], 0.2);
    assert_eq!(parsed["max_tokens"], 1000);
    assert_eq!(parsed["thinking"], json!({"type":"enabled","budget_tokens":1024}));
    assert!(parsed.get("remove").is_none());
    for path in ["max_tokens", "thinking", "remove"] {
        assert!(touched.contains(path), "{touched:?}");
    }
    let keep = br#""untouched":{"n":9007199254740993,"s":"\u0061"}"#;
    assert!(out.windows(keep.len()).any(|span| span == keep));
    let mut other = request(input, None);
    other.model = "different";
    other.requested_model = "different";
    assert_eq!(payload::apply(&rules, &other, input.to_vec()), input);
}

#[test]
fn custom_headers_expand_only_explicit_attributes_and_available_sources() {
    let attrs = BTreeMap::from([
        ("header:X-Literal".into(), "constant".into()),
        ("header:X-Copy".into(), "$x-client".into()),
        ("header:X-Session".into(), "prefix-$CPA-SESSION-ID".into()),
        ("header:X-Missing".into(), "$x-absent".into()),
        ("api_key".into(), "upstream-private".into()),
    ]);
    let mut client = HeaderMap::new();
    client.insert("x-client", "first".parse().unwrap());
    client.append("x-client", "second".parse().unwrap());
    client.insert("authorization", "Bearer client-private".parse().unwrap());
    let output: BTreeMap<_, _> = headers::custom_headers(&attrs, &client, Some("session"))
        .into_iter()
        .collect();
    assert_eq!(
        output,
        BTreeMap::from([
            ("X-Literal".into(), "constant".into()),
            ("X-Copy".into(), "first".into()),
            ("X-Session".into(), "prefix-session".into())
        ])
    );
    let output = headers::custom_headers(&attrs, &client, None);
    assert!(!output.iter().any(|(name, _)| name == "X-Session"));
    assert!(!output.iter().any(|(_, value)| value.contains("private")));
}

#[test]
fn requested_model_candidates_preserve_case_sensitive_rule_matching() {
    let cfg =
        Config::parse("payload:\n  override:\n    - models: [{name: 'alias'}]\n      params: {hit: true}\n").unwrap();
    let rules = payload::Rules::of(&cfg);
    for (resolved, requested, hit) in [
        ("upstream", "alias", true),
        ("alias", "alias", true),
        ("upstream", "ALIAS", false),
        ("upstream", "other", false),
    ] {
        let mut req = request(b"{}", None);
        req.model = resolved;
        req.requested_model = requested;
        let out = payload::apply(&rules, &req, b"{}".to_vec());
        let parsed: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(parsed.get("hit").is_some(), hit);
    }
}

/// Whole-element setters and whole-element reads on a `#(...)#` query edit every matching
/// element, on a small body and on a large one.
#[test]
fn wildcard_rules_edit_every_matching_element() {
    // Every section on one wildcard: defaults against an original without the array,
    // a whole-element default, a condition whose only match is the last element, and a
    // filter of fields inside elements.
    let cfg = Config::parse(
        r#"payload:
  default:
    - models: [{name: "m"}]
      params: {"items.#(k==3)#.x": 9}
    - models: [{name: "m"}]
      params: {"w.#(k==1)#": 5}
  default-raw:
    - models: [{name: "m"}]
      params: {"items.#(k==3)#.y": '{"a":1}'}
  override:
    - models: [{name: "m", exist: ["items.#(k==1)#"]}]
      params: {"items.#(k==1)#": "replaced"}
  override-raw:
    - models: [{name: "m", match: [{"items.#(k==2)#.n": 7}]}]
      params: {"items.#(k==2)#": '{"k":2,"n":4}'}
  filter:
    - models: [{name: "m"}]
      params: ["items.#(k==3)#.pad"]
"#,
    )
    .unwrap();
    let rules = payload::Rules::of(&cfg);
    let check = |n: usize| {
        let items: Vec<String> = (0..n)
            .map(|i| {
                let last = if i + 1 == n { 7 } else { 3 };
                format!(r#"{{"k":{},"n":{last},"pad":"{}"}}"#, 1 + i % 3, "p".repeat(40))
            })
            .collect();
        let body = format!(
            r#"{{"items":[{}],"w":[{}]}}"#,
            items.join(","),
            vec![r#"{"k":1}"#; n].join(",")
        );
        let original = format!(r#"{{"pad":"{}"}}"#, "p".repeat(20 * n));
        let req = payload::Request {
            target_executor: "openai",
            model: "m",
            requested_model: "m",
            protocol: "openai",
            from_protocol: "",
            root: "",
            original: original.as_bytes(),
            request_path: "",
            headers: None,
        };
        let out = payload::apply(&rules, &req, body.into_bytes());
        assert!(
            out.starts_with(br#"{"items":["replaced",{"k":2,"n":4},{"k":3,"n":3,"x":9,"y":{"a":1}},"replaced""#),
            "{}",
            String::from_utf8_lossy(&out[..200])
        );
        assert!(out.ends_with(br#",5,5]}"#));
    };
    // The last element (n - 1 = 1 mod 3) has k == 2 and is the only one with n == 7.
    check(500);
    check(8000);
}

/// Issue #146: a configured filter for a key holding an escaped quote removes the whole
/// member. sjson v1.2.5 left `{"\"x":,"y":2}`, which is invalid JSON
/// (docs/DIFFERENCES-FROM-GO.md "sjson deletion of escaped keys").
#[test]
fn filter_of_escaped_quote_key_keeps_valid_json() {
    let source = br#"{"\"x":1,"y":2}"#;
    for yaml in [
        "requests:\n  payload:\n    filter:\n      - models:\n          - name: \"*\"\n            protocol: claude\n        params:\n          - '\\\"x'\n",
        "payload:\n  filter:\n    - models: [{name: \"*\", protocol: claude}]\n      params: ['\\\"x']\n",
    ] {
        let cfg = Config::parse(yaml).unwrap();
        let rules = payload::Rules::of(&cfg);
        let req = payload::Request {
            target_executor: "claude",
            model: "claude-test",
            requested_model: "claude-test",
            protocol: "claude",
            from_protocol: "claude",
            root: "",
            original: source,
            request_path: "/v1/messages",
            headers: None,
        };
        let out = payload::apply(&rules, &req, source.to_vec());
        assert_eq!(out, br#"{"y":2}"#, "{}", String::from_utf8_lossy(&out));
        assert!(serde_json::from_slice::<Value>(&out).is_ok());
        // Untouched members keep their bytes, spacing and number spelling.
        let spaced = br#"{ "n" : 1.50e0 , "\"x" : {"deep":[1,"\""]}, "u":{"k":"\u00e9"} }"#;
        let out = payload::apply(&rules, &req, spaced.to_vec());
        assert_eq!(
            out,
            br#"{ "n" : 1.50e0 , "u":{"k":"\u00e9"} }"#,
            "{}",
            String::from_utf8_lossy(&out)
        );
    }
}

/// Issue #146: a configured filter with more path components than
/// `cliproxy_internal::protocol::json::WALK_DEPTH_LIMIT` leaves the body as it is on a 2 MiB worker stack;
/// sjson's recursion per component overflowed it.
#[test]
fn filter_with_a_very_long_path_keeps_the_body_on_a_worker_stack() {
    let depth = 2_000;
    let yaml = format!(
        "payload:\n  filter:\n    - models: [{{name: \"*\", protocol: claude}}]\n      params: ['{}']\n",
        vec!["a"; depth].join(".")
    );
    let body = format!("{}1{}", r#"{"a":"#.repeat(depth), "}".repeat(depth));
    let out = std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || {
            let cfg = Config::parse(&yaml).unwrap();
            let rules = payload::Rules::of(&cfg);
            let req = payload::Request {
                target_executor: "claude",
                model: "claude-test",
                requested_model: "claude-test",
                protocol: "claude",
                from_protocol: "claude",
                root: "",
                original: b"",
                request_path: "/v1/messages",
                headers: None,
            };
            (payload::apply(&rules, &req, body.clone().into_bytes()), body)
        })
        .unwrap()
        .join()
        .unwrap();
    assert_eq!(out.0, out.1.as_bytes());
}

/// Optional schema adaptation for translated Codex clients. Native Codex forwarding
/// does not use this helper (docs/codex/CONTRACT.md).
#[test]
fn codex_integer_tool_schemas_normalize_known_fields_and_preserve_other_data() {
    let mut official = HeaderMap::new();
    official.insert("user-agent", "codex_cli_rs/0.160.0".parse().unwrap());
    for (name, field) in [
        ("exec_command", "yield_time_ms"),
        ("functions__exec_command", "max_output_tokens"),
        ("write_stdin", "session_id"),
        ("sleep", "duration_ms"),
        ("collaboration__wait_agent", "timeout_ms"),
        ("tool_search", "limit"),
        ("create_goal", "token_budget"),
    ] {
        for field_type in [json!("number"), json!(["number", "null", "integer"])] {
            let mut parameters = json!({"type":"object", "properties":{"untouched":{"type":"number"}}});
            parameters["properties"][field] = json!({"type":field_type});
            let expected_type = if field_type.is_array() {
                json!(["integer", "null"])
            } else {
                json!("integer")
            };
            for (tool, parameter_path) in [
                (
                    json!({"type":"function","name":name,"parameters":parameters}),
                    "parameters",
                ),
                (
                    json!({"type":"function","function":{"name":name,"parameters":parameters}}),
                    "function.parameters",
                ),
                (json!({"name":name,"input_schema":parameters}), "input_schema"),
            ] {
                for additional in [false, true] {
                    let input = if additional {
                        json!({"input":[{"type":"additional_tools","tools":[tool]}],"other":9007199254740993u64})
                    } else {
                        json!({"tools":[tool],"other":9007199254740993u64})
                    };
                    let input = serde_json::to_vec(&input).unwrap();
                    let output = payload::normalize_codex_tool_integer_types(&input, &official);
                    let base = if additional { "input.0.tools.0" } else { "tools.0" };
                    assert_eq!(
                        serde_json::from_slice::<Value>(
                            cliproxy_internal::protocol::json::get(
                                &output,
                                &format!("{base}.{parameter_path}.properties.{field}.type")
                            )
                            .raw()
                        )
                        .unwrap(),
                        expected_type
                    );
                    assert_eq!(
                        cliproxy_internal::protocol::json::get(
                            &output,
                            &format!("{base}.{parameter_path}.properties.untouched.type")
                        )
                        .str(),
                        "number"
                    );
                    assert_eq!(
                        cliproxy_internal::protocol::json::get(&output, "other").raw(),
                        b"9007199254740993"
                    );
                    assert_eq!(
                        payload::normalize_codex_tool_integer_types(&output, &official),
                        output,
                        "idempotent normalization"
                    );
                    assert_eq!(
                        payload::normalize_codex_tool_integer_types(&input, &HeaderMap::new()),
                        input
                    );
                }
            }
        }
    }
    let unknown = br#"{ "tools":[{"type":"function","name":"operator-tool","parameters":{"properties":{"limit":{"type":"number"}}}}],"n":1.2300e+04 }"#;
    assert_eq!(payload::normalize_codex_tool_integer_types(unknown, &official), unknown);
}

#[test]
fn nested_integer_tool_fields_use_schema_paths_and_namespace_identity() {
    let mut official = HeaderMap::new();
    official.insert("user-agent", "codex_cli_rs/0.160.0".parse().unwrap());
    let body=br#"{"tools":[{"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"wait_agent","parameters":{"properties":{"timeout_ms":{"type":"number"}}}}]},{"type":"function","name":"web__run","parameters":{"properties":{"open":{"items":{"properties":{"lineno":{"type":"number"}}}}}}}]}"#;
    let output = payload::normalize_codex_tool_integer_types(body, &official);
    for path in [
        "tools.0.tools.0.parameters.properties.timeout_ms.type",
        "tools.1.parameters.properties.open.items.properties.lineno.type",
    ] {
        assert_eq!(cliproxy_internal::protocol::json::get(&output, path).str(), "integer");
    }
}

#[test]
fn payload_rule_gates_use_protocol_headers_and_original_request_fields() {
    let cfg = Config::parse(r#"payload:
  default:
    - models: [{name: "model", protocol: claude, from-protocol: openai, headers: {X-Team: "red-*"}, exist: [user], not-exist: [seed], match: [{stream: true}], not-match: [{n: 2}]}]
      params: {selected: true, max_tokens: 500}
  default-raw:
    - models: [{name: "model"}]
      params: {response_format: '{"type":"json_object"}'}
"#).unwrap();
    let rules = payload::Rules::of(&cfg);
    for (original, team, from, protocol, selected) in [
        (
            r#"{"user":"u","stream":true,"n":1,"max_tokens":12}"#,
            "red-team",
            "openai",
            "claude",
            true,
        ),
        (
            r#"{"user":"u","stream":true,"n":1,"seed":0}"#,
            "red-team",
            "openai",
            "claude",
            false,
        ),
        (r#"{"stream":true,"n":1}"#, "red-team", "openai", "claude", false),
        (
            r#"{"user":"u","stream":false,"n":1}"#,
            "red-team",
            "openai",
            "claude",
            false,
        ),
        (
            r#"{"user":"u","stream":true,"n":2}"#,
            "red-team",
            "openai",
            "claude",
            false,
        ),
        (
            r#"{"user":"u","stream":true,"n":1}"#,
            "blue-team",
            "openai",
            "claude",
            false,
        ),
        (
            r#"{"user":"u","stream":true,"n":1}"#,
            "red-team",
            "claude",
            "claude",
            false,
        ),
        (
            r#"{"user":"u","stream":true,"n":1}"#,
            "red-team",
            "openai",
            "openai",
            false,
        ),
    ] {
        let mut h = HeaderMap::new();
        h.insert("x-team", team.parse().unwrap());
        let mut req = request(original.as_bytes(), Some(&h));
        req.from_protocol = from;
        req.protocol = protocol;
        let output = payload::apply(&rules, &req, original.as_bytes().to_vec());
        let parsed: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(
            parsed.get("selected").is_some(),
            selected,
            "{original}, {team}, {from}, {protocol}"
        );
        assert_eq!(parsed["response_format"], json!({"type":"json_object"}));
        if selected {
            assert_eq!(parsed["max_tokens"], 12, "defaults must not overwrite a client value");
        }
    }
    // Defaults consult the original client request even when translation omitted a field.
    let original = br#"{"max_tokens":42}"#;
    let output = payload::apply(&rules, &request(original, None), b"{}".to_vec());
    assert!(!cliproxy_internal::protocol::json::get(&output, "max_tokens").exists());
}

#[test]
fn unsupported_selector_constructs_are_ignored_without_touching_the_request() {
    for path in ["@reverse", "[a].x", "a|@reverse", "a.#([k]).x"] {
        let cfg = Config::parse(&format!(
            "payload:\n  override:\n    - models: [{{name: 'model'}}]\n      params: {{'{path}': 9}}\n"
        ))
        .unwrap();
        let input = br#"{ "a":[{"k":1}], "n":1.2300e+04 }"#;
        assert_eq!(
            payload::apply(&payload::Rules::of(&cfg), &request(input, None), input.to_vec()),
            input,
            "{path}"
        );
    }
}

#[test]
fn nested_member_selectors_respect_parent_selection_and_escaped_literal_gate_keys() {
    let cfg = Config::parse(
        r#"payload:
  override:
    - models: [{name: model, exist: ['gate\|@literal']}]
      params:
        'groups.#(active==true)#.members.#(kind=="target")#.tag': selected
  filter:
    - models: [{name: model, exist: ['gate\|@literal']}]
      params:
        - 'groups.#(active==true)#.members.#(kind=="target")#.secret'
"#,
    )
    .unwrap();
    let rules = payload::Rules::of(&cfg);
    let mut body = json!({
        "gate|@literal": true,
        "groups":[
            {"active":true,"members":[{"kind":"target","id":"a","secret":"remove-a"},{"kind":"other","id":"b","secret":"keep-b"},{"kind":"target","id":"c","secret":"remove-c"}]},
            {"active":false,"members":[{"kind":"target","id":"d","secret":"keep-d"}]}
        ],
        "untouched":{"note":"keep this object"}
    });
    let raw = serde_json::to_vec(&body).unwrap();
    let output = payload::apply(&rules, &request(&raw, None), raw.clone());
    let mut expected = body.clone();
    for index in [0, 2] {
        let member = expected["groups"][0]["members"][index].as_object_mut().unwrap();
        member.insert("tag".into(), json!("selected"));
        member.shift_remove("secret");
    }
    assert_eq!(serde_json::from_slice::<Value>(&output).unwrap(), expected);
    // A similarly spelled key is not the configured escaped literal path.
    body.as_object_mut().unwrap().shift_remove("gate|@literal");
    body["gate"] = json!({"literal":true});
    let raw = serde_json::to_vec(&body).unwrap();
    assert_eq!(payload::apply(&rules, &request(&raw, None), raw.clone()), raw);
}
