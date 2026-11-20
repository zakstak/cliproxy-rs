//! Input-only patch adapter scenarios supplement the independently specified
//! patch_contract tests with adversarial operation sequences. These tests check
//! failure finality and custom-input field types, never recorded output chunks.
use cliproxy_internal::format::Format;
use cliproxy_internal::translation::apply_patch_responses::{
    Bridge, State, normalize_executor_request, normalize_request,
};
use cliproxy_internal::translation::{ResponseCtx, codex_responses_non_stream_with_bridge, pair};
use serde_json::Value;

/// The generator stores each byte as one rune.
fn bytes(v: &Value) -> Vec<u8> {
    v.as_str()
        .unwrap_or_default()
        .chars()
        .map(|c| u8::try_from(c as u32).unwrap())
        .collect()
}

fn text(v: &[u8]) -> String {
    String::from_utf8_lossy(v).into_owned()
}

type Got = (Vec<Vec<u8>>, Option<String>);

fn run(s: &Value) -> (Vec<Got>, Option<String>) {
    let ops = s["ops"].as_array().unwrap();
    let mut results = vec![];
    let kind = s["kind"].as_str().unwrap();
    match kind {
        "bridge" => {
            let mut b = Bridge::new(&bytes(&s["request"]));
            for op in ops {
                let input = bytes(&op["in"]);
                results.push(match op["op"].as_str().unwrap() {
                    "transform" => b.transform(&input),
                    "non_stream" => match b.transform_non_stream(&input) {
                        Ok(out) => (vec![out], None),
                        Err(e) => (vec![], Some(e)),
                    },
                    "finish" => (vec![], b.finish().err()),
                    "check_identity" => (vec![], b.check_identity(&input).err()),
                    "fail" => b.fail(&text(&input)),
                    other => panic!("op {other}"),
                });
            }
            (results, b.tool_input_error().map(str::to_owned))
        }
        "state" => {
            let source = Format::parse(s["source"].as_str().unwrap()).unwrap();
            let mut st = State::new(source, &bytes(&s["original"]), &bytes(&s["declarations"]));
            for op in ops {
                let input = bytes(&op["in"]);
                results.push(match op["op"].as_str().unwrap() {
                    "add_dispatcher" => {
                        st.add_dispatcher(op["name"].as_str().unwrap(), op["namespace"].as_str().unwrap());
                        (vec![], None)
                    }
                    "transform" => st.transform(&input),
                    "stream" => st.stream(&input),
                    "finish" => (vec![], st.finish().err()),
                    "bridge_finish" => (vec![], st.bridge.finish().err()),
                    "finish_stream" => st.finish_stream(),
                    "remember_event" => {
                        st.remember_dispatcher_event(&input);
                        (vec![], None)
                    }
                    "remember_args" => {
                        st.remember_dispatcher_arguments(&input);
                        (vec![], None)
                    }
                    "active" => (vec![st.active().to_string().into_bytes()], None),
                    other => panic!("op {other}"),
                });
            }
            (results, st.bridge.tool_input_error().map(str::to_owned))
        }
        "normalize" | "normalize_executor" => {
            let original = bytes(&s["original"]);
            for op in ops {
                let input = bytes(&op["in"]);
                let out = if kind == "normalize" {
                    normalize_request(&input)
                } else {
                    normalize_executor_request(&input, (!original.is_empty()).then_some(original.as_slice()))
                };
                results.push(match out {
                    Ok(out) => (vec![out], None),
                    Err(e) => (vec![], Some(e)),
                });
            }
            (results, None)
        }
        "codex_stream" | "codex_non_stream" => {
            let (original, translated) = (bytes(&s["original"]), bytes(&s["declarations"]));
            let ctx = ResponseCtx {
                model: s["model"].as_str().unwrap(),
                original_request: &original,
                translated_request: &translated,
                tool_search: false,
            };
            let bridged = s["bridged"].as_bool().unwrap_or(false);
            let codex = pair(Format::OpenAIResponse, Format::Codex).unwrap();
            if kind == "codex_stream" {
                let mut go = cliproxy_internal::translation::codex_responses_go_stream_with_bridge(
                    &ctx,
                    bridged.then(|| Bridge::new(&bytes(&s["request"]))),
                );
                for op in ops {
                    results.push((go.line(&bytes(&op["in"])).unwrap(), None));
                }
                let failed = go.tool_input_failed();
                return (results, failed.then(|| "<failed>".to_owned()));
            }
            let mut bridge = Bridge::new(&bytes(&s["request"]));
            for op in ops {
                let body = bytes(&op["in"]);
                let out = if bridged {
                    codex_responses_non_stream_with_bridge(&ctx, &body, &mut bridge)
                } else {
                    (codex.non_stream)(&ctx, &body)
                };
                results.push(match out {
                    Ok(out) => (vec![out], None),
                    Err(e) => {
                        assert_eq!(e.0, cliproxy_internal::translation::APPLY_PATCH_UPSTREAM_ERROR);
                        (vec![], None)
                    }
                });
            }
            (results, bridge.tool_input_error().map(str::to_owned))
        }
        other => panic!("kind {other}"),
    }
}

#[test]
fn patch_operation_contexts_keep_local_failures_terminal_and_inputs_typed() {
    let scenarios: Vec<Value> = serde_json::from_str(include_str!("translation/fixtures/patch_inputs.json")).unwrap();
    for scenario in scenarios {
        let (results, _) = run(&scenario);
        let mut failure_seen = false;
        for (outputs, _error) in results {
            for output in outputs {
                let payload = output.strip_prefix(b"data: ").unwrap_or(&output);
                let Ok(value) = serde_json::from_slice::<Value>(payload) else {
                    continue;
                };
                if value["type"] == "response.failed" {
                    assert!(!failure_seen, "{}: duplicate failure", scenario["name"]);
                    failure_seen = true;
                } else if value["type"] == "response.custom_tool_call_input.delta"
                    || value["type"] == "response.custom_tool_call_input.done"
                {
                    assert!(!failure_seen, "{}: custom input after failure", scenario["name"]);
                    let key = if value["type"] == "response.custom_tool_call_input.delta" {
                        "delta"
                    } else {
                        "input"
                    };
                    assert!(
                        value[key].is_string(),
                        "{}: custom input must be a string",
                        scenario["name"]
                    );
                    assert!(
                        value["item_id"].is_string(),
                        "{}: custom input must identify its call",
                        scenario["name"]
                    );
                } else if value["type"] == "response.completed" {
                    assert!(!failure_seen, "{}: success after failure", scenario["name"]);
                }
            }
        }
    }
}
