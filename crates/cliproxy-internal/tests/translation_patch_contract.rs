//! Contract of the custom apply_patch adapter, independently specified from the tool's
//! single-string input and Responses event types. The bridge is a translated-route
//! feature. Native Codex transport keeps the client's payload unchanged (CONTRACT.md).
use cliproxy_internal::format::Format;
use cliproxy_internal::translation::apply_patch_responses::{
    Bridge, State, normalize_executor_request, normalize_request,
};
use serde_json::{Value, json};

const REQUEST: &[u8] = br#"{"tools":[{"type":"custom","name":"apply_patch"}]}"#;

fn raw(value: Value) -> Vec<u8> {
    serde_json::to_vec(&value).unwrap()
}
fn decoded(values: &[Vec<u8>]) -> Vec<Value> {
    values.iter().map(|v| serde_json::from_slice(v).unwrap()).collect()
}
fn event(bridge: &mut Bridge, input: Value) -> Vec<Value> {
    let (out, error) = bridge.transform(&raw(input));
    assert!(error.is_none(), "{error:?}");
    decoded(&out)
}
fn item(arguments: &str) -> Value {
    json!({"type":"function_call","id":"item-1","call_id":"call-1","name":"apply_patch","arguments":arguments})
}
fn terminal(arguments: &str) -> Value {
    json!({"type":"response.completed","response":{"id":"resp-1","output":[item(arguments)]}})
}

#[test]
fn normalization_changes_only_the_declared_patch_tool_and_its_history() {
    let patch = "*** Begin Patch\n*** Add File: example.txt\n+hello 🙂\n*** End Patch";
    let request = json!({
        "tools":[{"type":"custom","name":"apply_patch","description":"Edit files"},
            {"type":"function","name":"ordinary","parameters":{"type":"object"}}],
        "input":[{"type":"custom_tool_call","name":"apply_patch","call_id":"p","input":patch},
            {"type":"custom_tool_call_output","call_id":"p","output":"ok"},
            {"type":"custom_tool_call","name":"ordinary","call_id":"o","input":"opaque"}],
        "tool_choice":{"type":"custom","name":"apply_patch"}, "unknown":{"value":17}
    });
    let normalized: Value = serde_json::from_slice(&normalize_request(&raw(request.clone())).unwrap()).unwrap();
    assert_eq!(normalized["tools"][0]["type"], "function");
    assert_eq!(normalized["tools"][0]["parameters"]["required"], json!(["input"]));
    assert_eq!(
        normalized["tools"][0]["parameters"]["properties"]["input"]["type"],
        "string"
    );
    assert!(
        normalized["tools"][0]["description"]
            .as_str()
            .unwrap()
            .contains("Edit files")
    );
    assert_eq!(normalized["tools"][1], request["tools"][1]);
    assert_eq!(normalized["input"][0]["type"], "function_call");
    let arguments: Value = serde_json::from_str(normalized["input"][0]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(arguments, json!({"input":patch}));
    assert_eq!(normalized["input"][1]["type"], "function_call_output");
    assert_eq!(normalized["input"][1]["output"], "ok");
    assert_eq!(normalized["input"][2], request["input"][2]);
    assert_eq!(
        normalized["tool_choice"],
        json!({"type":"function","name":"apply_patch"})
    );
    assert_eq!(normalized["unknown"], request["unknown"]);
}

#[test]
fn normalization_rejects_non_string_history_and_keeps_unrelated_bytes() {
    for bad in [json!(null), json!(3), json!({}), json!([])] {
        let request = json!({"input":[{"type":"custom_tool_call","name":"apply_patch","input":bad}]});
        assert!(normalize_request(&raw(request)).is_err());
    }
    let request = br#"{ "tools" : [{ "type":"function", "name":"ordinary" }], "opaque" : 1.500 }"#;
    assert_eq!(normalize_request(request).unwrap(), request);
    assert_eq!(normalize_executor_request(request, None).unwrap(), request);
    assert!(normalize_request(b"{").is_err());
}

#[test]
fn patch_arguments_survive_every_utf8_character_split() {
    let patch = "*** Begin Patch\n+中文🙂 <>& \"\\\n*** End Patch";
    let arguments = json!({"input":patch}).to_string();
    let splits = arguments
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(arguments.len()));
    for split in splits {
        let mut bridge = Bridge::new(REQUEST);
        let mut output = event(
            &mut bridge,
            json!({"type":"response.output_item.added","output_index":0,"item":item("")}),
        );
        for fragment in [&arguments[..split], &arguments[split..]] {
            output.extend(event(&mut bridge,json!({"type":"response.function_call_arguments.delta","item_id":"item-1","output_index":0,"delta":fragment})));
        }
        output.extend(event(
            &mut bridge,
            json!({"type":"response.output_item.done","output_index":0,"item":item(&arguments)}),
        ));
        output.extend(event(&mut bridge, terminal(&arguments)));
        let assembled: String = output
            .iter()
            .filter(|v| v["type"] == "response.custom_tool_call_input.delta")
            .map(|v| v["delta"].as_str().unwrap())
            .collect();
        assert_eq!(assembled, patch, "split {split}");
        let done: Vec<&Value> = output
            .iter()
            .filter(|v| v["type"] == "response.custom_tool_call_input.done")
            .collect();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0]["input"], patch);
        let completed = output.iter().find(|v| v["type"] == "response.completed").unwrap();
        assert_eq!(
            completed["response"]["output"][0],
            json!({"type":"custom_tool_call","id":"item-1","call_id":"call-1","name":"apply_patch","input":patch})
        );
        assert!(bridge.finish().is_ok());
    }
}

#[test]
fn buffered_patch_results_restore_exact_input_and_all_other_fields() {
    for patch in ["", "simple", "中文🙂\n<>&\"\\"] {
        let args = json!({"input":patch}).to_string();
        for envelope in [false, true] {
            let response = json!({"id":"resp-1","status":"completed","output":[item(&args)],"usage":{"input_tokens":2,"output_tokens":3,"total_tokens":5},"unknown":{"a":1}});
            let input = if envelope {
                json!({"type":"response.completed","response":response.clone()})
            } else {
                response.clone()
            };
            let mut bridge = Bridge::new(REQUEST);
            let out: Value = serde_json::from_slice(&bridge.transform_non_stream(&raw(input)).unwrap()).unwrap();
            let out = if envelope { &out["response"] } else { &out };
            assert_eq!(out["id"], response["id"]);
            assert_eq!(out["status"], response["status"]);
            assert_eq!(out["usage"], response["usage"]);
            assert_eq!(out["unknown"], response["unknown"]);
            assert_eq!(
                out["output"],
                json!([{"type":"custom_tool_call","id":"item-1","call_id":"call-1","name":"apply_patch","input":patch}])
            );
        }
    }
}

#[test]
fn invalid_arguments_emit_one_safe_failure_then_stop() {
    for args in [
        "{}",
        "null",
        r#"{"input":3}"#,
        r#"{"input":"a","input":"b"}"#,
        r#"{"input":"ok","extra":"secret"}"#,
        r#"{"input":"unfinished"#,
    ] {
        let mut bridge = Bridge::new(REQUEST);
        let (out, error) = bridge.transform(&raw(
            json!({"type":"response.output_item.done","output_index":0,"item":item(args)}),
        ));
        assert!(error.is_some(), "{args}");
        let out = decoded(&out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["type"], "response.failed");
        assert_eq!(out[0]["response"]["error"]["code"], "invalid_tool_arguments");
        assert!(!out[0].to_string().contains("secret"));
        assert!(bridge.tool_input_error().is_some());
        assert!(bridge.finish().is_err());
        assert!(bridge.transform(&raw(terminal(r#"{"input":"later"}"#))).0.is_empty());
        assert!(bridge.fail("another error").0.is_empty());
    }
}

#[test]
fn acquired_patch_calls_need_validated_final_arguments() {
    let mut bridge = Bridge::new(REQUEST);
    event(
        &mut bridge,
        json!({"type":"response.output_item.added","output_index":0,"item":item("")}),
    );
    assert!(bridge.finish().is_err());
    event(
        &mut bridge,
        json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":r#"{"input":"com"#}),
    );
    assert!(bridge.finish().is_err());
    let valid = r#"{"input":"complete"}"#;
    event(
        &mut bridge,
        json!({"type":"response.output_item.done","output_index":0,"item":item(valid)}),
    );
    assert!(bridge.finish().is_ok());
}

#[test]
fn unrelated_and_native_custom_events_preserve_their_bytes() {
    for request in [REQUEST, b"{}"] {
        let mut bridge = Bridge::new(request);
        for bytes in [
            br#"{ "type":"response.output_text.delta", "delta":"<>&", "unknown":1.50 }"#.as_slice(),
            br#"{ "type":"response.output_item.added", "item":{"type":"custom_tool_call","name":"apply_patch","id":"n","call_id":"c","input":""} }"#,
            br#"{ "type":"response.custom_tool_call_input.delta", "item_id":"n", "delta":"opaque" }"#,
        ] {
            let (out,error) = bridge.transform(bytes);
            assert!(error.is_none());
            assert_eq!(out,vec![bytes.to_vec()]);
        }
    }
}

#[test]
fn executor_state_requires_a_response_terminal_after_patch_activity() {
    let arguments = r#"{"input":"patch"}"#;
    let mut state = State::new(Format::OpenAIResponse, REQUEST, REQUEST);
    assert!(state.active());
    let (out, error) = state.transform(&raw(
        json!({"type":"response.output_item.done","output_index":0,"item":item(arguments)}),
    ));
    assert!(error.is_none());
    assert!(!out.is_empty());
    assert!(state.has_patch_activity());
    assert!(state.finish().is_err());
    assert!(state.transform(&raw(terminal(arguments))).1.is_none());
    assert!(state.finish().is_ok());
    assert!(
        state
            .transform(br#"{"type":"response.output_text.delta","delta":"late"}"#)
            .0
            .is_empty()
    );
}

#[test]
fn stream_done_rejects_incomplete_patch_activity() {
    let mut state = State::new(Format::OpenAIResponse, REQUEST, REQUEST);
    state.transform(&raw(
        json!({"type":"response.output_item.added","output_index":0,"item":item("")}),
    ));
    let (out, error) = state.transform(b"[DONE]");
    assert!(error.is_some());
    assert_eq!(decoded(&out)[0]["type"], "response.failed");
    assert!(state.transform(b"[DONE]").0.is_empty());
    let mut inactive = State::new(Format::OpenAIResponse, b"{}", b"{}");
    assert!(!inactive.active());
    assert_eq!(inactive.transform(b"[DONE]").0, vec![b"[DONE]".to_vec()]);
    assert!(inactive.finish().is_ok());
}

#[test]
fn patch_previews_wait_for_call_identity_and_bind_to_the_final_item() {
    for initial_id in [false, true] {
        for initial_call_id in [false, true] {
            for final_in_terminal in [false, true] {
                let mut bridge = Bridge::new(REQUEST);
                let mut initial = item("");
                if !initial_id {
                    initial["id"] = json!("");
                }
                if !initial_call_id {
                    initial["call_id"] = json!("");
                }
                let mut out = event(
                    &mut bridge,
                    json!({"type":"response.output_item.added","output_index":0,"item":initial}),
                );
                out.extend(event(&mut bridge, json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":r#"{"input":"hello"#})));
                if !initial_id || !initial_call_id {
                    assert!(out.is_empty(), "a preview cannot name an unidentified call");
                }
                let args = r#"{"input":"hello 🙂"}"#;
                out.extend(event(
                    &mut bridge,
                    json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":" 🙂\"}"}),
                ));
                if !final_in_terminal {
                    out.extend(event(
                        &mut bridge,
                        json!({"type":"response.output_item.done","output_index":0,"item":item(args)}),
                    ));
                }
                out.extend(event(&mut bridge, terminal(args)));
                let preview: String = out
                    .iter()
                    .filter(|e| e["type"] == "response.custom_tool_call_input.delta")
                    .map(|e| e["delta"].as_str().unwrap())
                    .collect();
                assert_eq!(preview, "hello 🙂");
                for e in out.iter().filter(|e| {
                    e["type"] == "response.custom_tool_call_input.delta"
                        || e["type"] == "response.custom_tool_call_input.done"
                }) {
                    assert_eq!(e["item_id"], "item-1");
                }
                assert_eq!(
                    out.iter()
                        .filter(|e| e["type"] == "response.custom_tool_call_input.done")
                        .count(),
                    1
                );
                assert!(bridge.finish().is_ok());
            }
        }
    }
}

#[test]
fn a_patch_snapshot_cannot_replace_previously_emitted_input() {
    for snapshot_first in [false, true] {
        let mut bridge = Bridge::new(REQUEST);
        event(
            &mut bridge,
            json!({"type":"response.output_item.added","output_index":0,"item":item("")}),
        );
        let delta = json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"item-1","delta":r#"{"input":"safe"#});
        let snapshot = json!({"type":"response.function_call_arguments.done","output_index":0,"item_id":"item-1","arguments":r#"{"input":"different"}"#});
        let (first, second) = if snapshot_first {
            (snapshot, delta)
        } else {
            (delta, snapshot)
        };
        event(&mut bridge, first);
        let (out, error) = bridge.transform(&raw(second));
        assert!(error.is_some());
        let out = decoded(&out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["type"], "response.failed");
        assert!(bridge.finish().is_err());
        assert!(bridge.transform(&raw(terminal(r#"{"input":"safe"}"#))).0.is_empty());
    }
}

#[test]
fn folded_dispatchers_restore_only_the_declared_patch_child() {
    let request = raw(json!({"tools":[{"type":"namespace","name":"files","tools":[
        {"type":"custom","name":"apply_patch"}, {"type":"function","name":"lookup"}
    ]}]}));
    for child in ["apply_patch", "lookup"] {
        for key in ["output_index", "item_id", "call_id"] {
            let mut state = State::new(Format::OpenAIResponse, &request, &request);
            state.add_dispatcher("files", "files");
            let start = json!({"type":"response.output_item.added","output_index":0,"item":{
                "type":"function_call","id":"item-1","call_id":"call-1","name":"files","arguments":""
            }});
            let (mut out, error) = state.transform(&raw(start));
            assert!(error.is_none());
            let wrapped = json!({"name":child,"arguments":{"input":"edit 🙂"}}).to_string();
            let mut delta = json!({"type":"response.function_call_arguments.delta","delta":wrapped});
            delta[key] = match key {
                "output_index" => json!(0),
                "item_id" => json!("item-1"),
                _ => json!("call-1"),
            };
            let (more, error) = state.transform(&raw(delta));
            assert!(error.is_none(), "{child}/{key}: {error:?}");
            out.extend(more);
            let done = json!({"type":"response.output_item.done","output_index":0,"item":{
                "type":"function_call","id":"item-1","call_id":"call-1","name":child,"namespace":"files","arguments":r#"{"input":"edit 🙂"}"#
            }});
            let (more, error) = state.transform(&raw(done));
            assert!(error.is_none(), "{child}/{key}: {error:?}");
            out.extend(more);
            let out = decoded(&out);
            let custom: Vec<_> = out
                .iter()
                .filter(|e| e["type"] == "response.custom_tool_call_input.done")
                .collect();
            if child == "apply_patch" {
                assert_eq!(custom.len(), 1);
                assert_eq!(custom[0]["input"], "edit 🙂");
                let restored = out.iter().find(|e| e["type"] == "response.output_item.done").unwrap();
                assert_eq!(restored["item"]["type"], "custom_tool_call");
                assert_eq!(restored["item"]["name"], "apply_patch");
                assert_eq!(restored["item"]["namespace"], "files");
            } else {
                assert!(custom.is_empty());
                assert!(
                    out.iter()
                        .any(|e| e["type"] == "response.output_item.done" && e["item"]["type"] == "function_call")
                );
            }
        }
    }
}

#[test]
fn contradictory_identity_never_rebinds_an_emitted_patch_preview() {
    for field in ["id", "call_id", "name"] {
        let mut bridge = Bridge::new(REQUEST);
        event(
            &mut bridge,
            json!({"type":"response.output_item.added","output_index":0,"item":item("")}),
        );
        event(
            &mut bridge,
            json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"item-1","delta":r#"{"input":"preview"#}),
        );
        let mut changed = item(r#"{"input":"preview"}"#);
        changed[field] = json!("different");
        let (out, error) = bridge.transform(&raw(
            json!({"type":"response.output_item.done","output_index":0,"item":changed}),
        ));
        assert!(error.is_some(), "{field}");
        assert_eq!(decoded(&out)[0]["type"], "response.failed");
        assert!(bridge.finish().is_err());
    }
}

#[test]
fn interleaved_patch_and_ordinary_calls_keep_content_and_identity_separate() {
    let mut bridge = Bridge::new(REQUEST);
    let ordinary =
        json!({"type":"function_call","id":"ordinary","call_id":"other-call","name":"lookup","arguments":"{}"});
    let mut out = event(
        &mut bridge,
        json!({"type":"response.output_item.added","output_index":0,"item":item("")}),
    );
    let ordinary_start = raw(json!({"type":"response.output_item.added","output_index":1,"item":ordinary}));
    let (passed, error) = bridge.transform(&ordinary_start);
    assert!(error.is_none());
    let mut passed = decoded(&passed);
    passed[0].as_object_mut().unwrap().remove("sequence_number");
    assert_eq!(passed, [serde_json::from_slice::<Value>(&ordinary_start).unwrap()]);
    out.extend(event(&mut bridge,json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"item-1","delta":r#"{"input":"one"#})));
    let ordinary_delta = raw(
        json!({"type":"response.function_call_arguments.delta","output_index":1,"item_id":"ordinary","delta":"opaque"}),
    );
    let (passed, error) = bridge.transform(&ordinary_delta);
    assert!(error.is_none());
    let mut passed = decoded(&passed);
    passed[0].as_object_mut().unwrap().remove("sequence_number");
    assert_eq!(passed, [serde_json::from_slice::<Value>(&ordinary_delta).unwrap()]);
    out.extend(event(
        &mut bridge,
        json!({"type":"response.output_item.done","output_index":0,"item":item(r#"{"input":"one edit"}"#)}),
    ));
    out.extend(event(
        &mut bridge,
        json!({"type":"response.completed","response":{"output":[item(r#"{"input":"one edit"}"#),ordinary]}}),
    ));
    let done = out.iter().find(|e| e["type"] == "response.completed").unwrap();
    assert_eq!(done["response"]["output"][0]["input"], "one edit");
    assert_eq!(done["response"]["output"][1], ordinary);
    assert!(bridge.finish().is_ok());
}

#[test]
fn original_dispatcher_input_cannot_be_replaced_during_restoration() {
    let request =
        raw(json!({"tools":[{"type":"namespace","name":"files","tools":[{"type":"custom","name":"apply_patch"}]}]}));
    for original_input in ["original", "restored"] {
        let mut state = State::new(Format::OpenAIResponse, &request, &request);
        state.add_dispatcher("files", "files");
        let start = json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"i","call_id":"c","name":"files","arguments":""}});
        assert!(state.transform(&raw(start)).1.is_none());
        let delta = json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"i","delta":json!({"name":"apply_patch","arguments":{"input":original_input}}).to_string()});
        assert!(state.transform(&raw(delta)).1.is_none());
        let source = json!({"type":"response.output_item.done","output_index":0,"item":{
            "type":"function_call","id":"i","call_id":"c","name":"files","arguments":json!({"name":"apply_patch","arguments":{"input":original_input}}).to_string()
        }});
        state.remember_dispatcher_event(&raw(source));
        let restored = json!({"type":"response.output_item.done","output_index":0,"item":{
            "type":"function_call","id":"i","call_id":"c","name":"apply_patch","namespace":"files","arguments":r#"{"input":"restored"}"#
        }});
        let (out, error) = state.transform(&raw(restored));
        if original_input == "restored" {
            assert!(error.is_none());
            assert!(
                decoded(&out)
                    .iter()
                    .any(|e| e["type"] == "response.custom_tool_call_input.done" && e["input"] == "restored")
            );
        } else if error.is_some() {
            assert_eq!(decoded(&out)[0]["type"], "response.failed");
            assert!(state.finish().is_err());
        } else {
            // The retained source wrapper is authoritative during restoration.
            // It may keep that value or refuse contradictory evidence, but it
            // must never publish the restored substitute as the patch input.
            let out = decoded(&out);
            let done = out
                .iter()
                .find(|e| e["type"] == "response.custom_tool_call_input.done")
                .unwrap();
            assert_eq!(done["input"], original_input);
        }
    }
}

#[test]
fn a_successful_response_or_upstream_failure_seals_patch_state_and_transport() {
    for upstream_failure in [false, true] {
        let mut state = State::new(Format::OpenAIResponse, REQUEST, REQUEST);
        let final_event = if upstream_failure {
            json!({"type":"response.failed","response":{"error":{"code":"upstream","message":"refused"}}})
        } else {
            terminal(r#"{"input":"done"}"#)
        };
        let (out, error) = state.transform(&raw(final_event));
        assert!(error.is_none());
        assert!(!out.is_empty());
        assert!(
            state
                .transform(&raw(
                    json!({"type":"response.function_call_arguments.delta","item_id":"item-1","delta":"late"})
                ))
                .0
                .is_empty()
        );
        let sentinel = b"data: [DONE]\n\n";
        assert_eq!(state.stream(sentinel).0, [sentinel.to_vec()]);
        assert!(state.stream(b"data: {}\n\n").0.is_empty());
    }
}

#[test]
fn ordinary_function_declarations_own_their_name_before_additional_custom_tools() {
    let function =
        json!({"type":"function","name":"apply_patch","description":"ordinary","parameters":{"type":"object"}});
    let body = json!({"tools":[function.clone()],"input":[{"type":"additional_tools","tools":[{"type":"custom","name":"apply_patch"}]}]});
    let out: Value = serde_json::from_slice(&normalize_request(&raw(body)).unwrap()).unwrap();
    assert_eq!(out["tools"][0], function);
    let mut bridge = Bridge::new(&raw(out));
    let ordinary = raw(json!({"type":"response.output_item.done","output_index":0,"item":item("{} ")}));
    let (out, error) = bridge.transform(&ordinary);
    assert!(error.is_none());
    assert_eq!(out, [ordinary]);
}

#[test]
fn each_event_identity_key_can_join_arguments_to_the_same_patch_call() {
    for key in ["output_index", "item_id", "call_id"] {
        let mut bridge = Bridge::new(REQUEST);
        let arguments = r#"{"input":"joined"}"#;
        event(
            &mut bridge,
            json!({"type":"response.output_item.added","output_index":2,"item":item("")}),
        );
        let mut delta = json!({"type":"response.function_call_arguments.delta","delta":arguments});
        delta[key] = match key {
            "output_index" => json!(2),
            "item_id" => json!("item-1"),
            _ => json!("call-1"),
        };
        let mut out = event(&mut bridge, delta);
        out.extend(event(
            &mut bridge,
            json!({"type":"response.output_item.done","output_index":2,"item":item(arguments)}),
        ));
        out.extend(event(&mut bridge, terminal(arguments)));
        let preview: String = out
            .iter()
            .filter(|e| e["type"] == "response.custom_tool_call_input.delta")
            .map(|e| e["delta"].as_str().unwrap())
            .collect();
        assert_eq!(preview, "joined");
        let done = out
            .iter()
            .find(|e| e["type"] == "response.custom_tool_call_input.done")
            .unwrap();
        assert_eq!(done["item_id"], "item-1");
        assert_eq!(done["input"], "joined");
        assert!(bridge.finish().is_ok());
    }
}

#[test]
fn sparse_terminal_snapshots_keep_completed_patch_and_ordinary_items_in_order() {
    for include_new_item in [false, true] {
        let mut bridge = Bridge::new(REQUEST);
        event(
            &mut bridge,
            json!({"type":"response.output_item.done","output_index":0,"item":item(r#"{"input":"edit"}"#)}),
        );
        let message = json!({"type":"message","id":"m","role":"assistant","content":[{"type":"output_text","text":"explanation"}]});
        event(
            &mut bridge,
            json!({"type":"response.output_item.done","output_index":1,"item":message}),
        );
        let new = json!({"type":"function_call","id":"new","call_id":"new-call","name":"lookup","arguments":"{}"});
        let final_output = if include_new_item {
            json!([new.clone()])
        } else {
            json!([])
        };
        let out = event(
            &mut bridge,
            json!({"type":"response.completed","response":{"output":final_output}}),
        );
        let completed = out.iter().find(|e| e["type"] == "response.completed").unwrap();
        let output = completed["response"]["output"].as_array().unwrap();
        assert_eq!(output[0]["type"], "custom_tool_call");
        assert_eq!(output[0]["input"], "edit");
        assert_eq!(output[1], message);
        if include_new_item {
            assert_eq!(output[2], new);
        } else {
            assert_eq!(output.len(), 2);
        }
        assert!(bridge.finish().is_ok());
    }
}

#[test]
fn nested_tool_choice_keeps_unchanged_bytes() {
    let choice = r#"{ "type" : "allowed_tools", "x": 1.50, "tools" : [ {"type":"function","name":"other"} , { "type" : "custom" , "name":"apply_patch", "u":[1] }, {"type":"allowed_tools","tools":[ {"type":"custom","name":"apply_patch"} ]} ] }"#;
    let body = format!(
        r#"{{"model":"m","input":"q","tools":[{{"type":"custom","name":"apply_patch"}}],"tool_choice":{choice}}}"#
    );
    let out = cliproxy_internal::translation::apply_patch_responses::normalize_executor_request(body.as_bytes(), None)
        .unwrap();
    let expected = choice
        .replace(r#""type" : "custom""#, r#""type" : "function""#)
        .replace(
            r#"{"type":"custom","name":"apply_patch"}"#,
            r#"{"type":"function","name":"apply_patch"}"#,
        );
    assert_eq!(
        cliproxy_internal::protocol::json::get(&out, "tool_choice").raw(),
        expected.as_bytes(),
        "{}",
        String::from_utf8_lossy(&out)
    );
}
