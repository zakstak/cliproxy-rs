//! Client schema contracts, specified independently of historical recordings.
//! Sources: Anthropic Messages content/tool schemas, OpenAI Chat Completions and
//! Responses input/tool schemas, and docs/codex/CONTRACT.md translated-route rules.
//! This is format conversion only; it makes no claim that any endpoint accepts a
//! particular provider-specific feature outside those contracts.
use cliproxy_internal::format::Format;
use cliproxy_internal::translation::{RequestCtx, translate_request};
use serde_json::{Value, json};

fn pairs() -> [(Format, Format); 7] {
    use Format::{Claude, Codex, OpenAI, OpenAIResponse};
    [
        (Claude, OpenAI),
        (Claude, Codex),
        (OpenAI, Claude),
        (OpenAI, Codex),
        (OpenAIResponse, Claude),
        (OpenAIResponse, OpenAI),
        (OpenAIResponse, Codex),
    ]
}
fn convert(client: Format, upstream: Format, body: &Value) -> Value {
    let ctx = RequestCtx {
        model: "chosen",
        stream: true,
        tool_search: false,
        system_turns: false,
    };
    let raw = serde_json::to_vec(body).unwrap();
    serde_json::from_slice(&translate_request(client, upstream, &ctx, &raw).unwrap()).unwrap()
}
fn schema() -> Value {
    json!({"type":"object","properties":{"path":{"type":"string","enum":["a","b"]},"count":{"type":"integer","minimum":1}},"required":["path"],"additionalProperties":false})
}
fn declaration(client: Format) -> Value {
    match client {
        Format::Claude => json!({"name":"lookup","description":"Read one path","input_schema":schema()}),
        Format::OpenAI => {
            json!({"type":"function","function":{"name":"lookup","description":"Read one path","parameters":schema()}})
        }
        _ => json!({"type":"function","name":"lookup","description":"Read one path","parameters":schema()}),
    }
}
fn conversation(client: Format) -> Value {
    let tools = vec![declaration(client)];
    match client {
        Format::Claude => json!({"model":"original","system":"system instruction","tools":tools,"messages":[
            {"role":"user","content":"first question 🙂"},
            {"role":"assistant","content":[{"type":"text","text":"before call"},{"type":"tool_use","id":"call-1","name":"lookup","input":{"path":"a"}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","content":"result 🙂"},{"type":"text","text":"follow up"}]}
        ]}),
        Format::OpenAI => json!({"model":"original","tools":tools,"messages":[
            {"role":"system","content":"system instruction"}, {"role":"user","content":"first question 🙂"},
            {"role":"assistant","content":"before call","tool_calls":[{"id":"call-1","type":"function","function":{"name":"lookup","arguments":r#"{"path":"a"}"#}}]},
            {"role":"tool","tool_call_id":"call-1","content":"result 🙂"}, {"role":"user","content":"follow up"}
        ]}),
        _ => json!({"model":"original","instructions":"system instruction","tools":tools,"input":[
            {"type":"message","role":"user","content":"first question 🙂"},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"before call"}]},
            {"type":"function_call","call_id":"call-1","name":"lookup","arguments":r#"{"path":"a"}"#},
            {"type":"function_call_output","call_id":"call-1","output":"result 🙂"},
            {"type":"message","role":"user","content":"follow up"}
        ]}),
    }
}
fn strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => {
            for v in a {
                strings(v, out)
            }
        }
        Value::Object(o) => {
            for v in o.values() {
                strings(v, out)
            }
        }
        _ => {}
    }
}
fn contains(v: &Value, s: &str) -> bool {
    let mut values = vec![];
    strings(v, &mut values);
    values.iter().any(|v| v.contains(s))
}
#[test]
fn all_pairs_keep_conversation_text_call_result_references_and_tool_constraints() {
    for (client, upstream) in pairs() {
        let out = convert(client, upstream, &conversation(client));
        // Native Responses payloads retain the client-selected identity; translated
        // formats use the routing-selected model (Codex CONTRACT.md).
        assert_eq!(
            out["model"],
            if client == Format::OpenAIResponse && upstream == Format::Codex {
                "original"
            } else {
                "chosen"
            },
            "{client:?}->{upstream:?}"
        );
        for text in [
            "system instruction",
            "first question 🙂",
            "before call",
            "result 🙂",
            "follow up",
        ] {
            assert!(contains(&out, text), "{client:?}->{upstream:?}: {text}: {out}");
        }
        let tool = &out["tools"][0];
        let (name, description, parameters) = match upstream {
            Format::Claude => (&tool["name"], &tool["description"], &tool["input_schema"]),
            Format::OpenAI => (
                &tool["function"]["name"],
                &tool["function"]["description"],
                &tool["function"]["parameters"],
            ),
            _ => (&tool["name"], &tool["description"], &tool["parameters"]),
        };
        assert_eq!(name, "lookup");
        assert_eq!(description, "Read one path");
        assert_eq!(parameters["properties"], schema()["properties"]);
        assert_eq!(parameters["required"], json!(["path"]));
        assert_eq!(parameters["additionalProperties"], false);
        match upstream {
            Format::Claude => {
                let blocks: Vec<_> = out["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .flat_map(|m| m["content"].as_array().into_iter().flatten())
                    .collect();
                let call = blocks.iter().find(|b| b["type"] == "tool_use").unwrap();
                assert_eq!(call["id"], "call-1");
                assert_eq!(call["name"], "lookup");
                assert_eq!(call["input"], json!({"path":"a"}));
                let result = blocks.iter().find(|b| b["type"] == "tool_result").unwrap();
                assert_eq!(result["tool_use_id"], "call-1");
                assert!(contains(result, "result 🙂"));
            }
            Format::OpenAI => {
                let messages = out["messages"].as_array().unwrap();
                let call = messages
                    .iter()
                    .find_map(|m| m["tool_calls"].as_array().and_then(|a| a.first()))
                    .unwrap();
                assert_eq!(call["id"], "call-1");
                assert_eq!(call["function"]["name"], "lookup");
                assert_eq!(
                    serde_json::from_str::<Value>(call["function"]["arguments"].as_str().unwrap()).unwrap(),
                    json!({"path":"a"})
                );
                let result = messages.iter().find(|m| m["role"] == "tool").unwrap();
                assert_eq!(result["tool_call_id"], "call-1");
                assert!(contains(result, "result 🙂"));
            }
            _ => {
                let input = out["input"].as_array().unwrap();
                let call = input.iter().find(|i| i["type"] == "function_call").unwrap();
                assert_eq!(call["call_id"], "call-1");
                assert_eq!(call["name"], "lookup");
                assert_eq!(
                    serde_json::from_str::<Value>(call["arguments"].as_str().unwrap()).unwrap(),
                    json!({"path":"a"})
                );
                let result = input.iter().find(|i| i["type"] == "function_call_output").unwrap();
                assert_eq!(result["call_id"], "call-1");
                assert_eq!(result["output"], "result 🙂");
            }
        }
    }
}

#[test]
fn images_keep_their_url_or_base64_content_beside_user_text() {
    let url = "https://example.test/picture.png";
    let data = "data:image/png;base64,aGVsbG8=";
    for (client, upstream) in pairs() {
        for image in [url, data] {
            let part = match client {
                Format::Claude if image == data => {
                    json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGVsbG8="}})
                }
                Format::Claude => json!({"type":"image","source":{"type":"url","url":image}}),
                Format::OpenAI => json!({"type":"image_url","image_url":{"url":image,"detail":"high"}}),
                _ => json!({"type":"input_image","image_url":image,"detail":"high"}),
            };
            let text_type = if client == Format::OpenAIResponse {
                "input_text"
            } else {
                "text"
            };
            let msg = json!({"role":"user","content":[{"type":text_type,"text":"describe this"},part]});
            let body = if client == Format::OpenAIResponse {
                json!({"input":[msg]})
            } else {
                json!({"messages":[msg]})
            };
            let out = convert(client, upstream, &body);
            assert!(contains(&out, "describe this"), "{client:?}->{upstream:?}: {out}");
            assert!(
                contains(&out, if image == data { "aGVsbG8=" } else { url }),
                "{client:?}->{upstream:?}: {out}"
            );
            if upstream == Format::Claude {
                let block = out["messages"][0]["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|b| b["type"] == "image")
                    .unwrap();
                assert_eq!(block["source"]["type"], if image == data { "base64" } else { "url" });
                if image == data {
                    assert_eq!(block["source"]["media_type"], "image/png");
                }
            }
        }
    }
}

#[test]
fn attachment_conversion_keeps_bytes_metadata_and_neighboring_text() {
    let responses = json!({"input":[{"role":"user","content":[
        {"type":"input_text","text":"attachments"},
        {"type":"input_file","file_id":"file-1","filename":"note.pdf","file_data":"data:application/pdf;base64,cGRm"},
        {"type":"input_audio","data":"c291bmQ=","format":"wav"},
        {"type":"input_video","video_url":"https://example.test/movie.mp4","processing":{"fps":2}}
    ]}]});
    let out = convert(Format::OpenAIResponse, Format::OpenAI, &responses);
    assert_eq!(
        out["messages"][0]["content"],
        json!([
            {"type":"text","text":"attachments"},
            {"type":"file","file":{"file_id":"file-1","filename":"note.pdf","file_data":"data:application/pdf;base64,cGRm"}},
            {"type":"input_audio","input_audio":{"data":"c291bmQ=","format":"wav"}},
            {"type":"video_url","video_url":{"url":"https://example.test/movie.mp4","processing":{"fps":2}}}
        ])
    );
    let chat = json!({"messages":[{"role":"user","content":[
        {"type":"text","text":"attachments"},
        {"type":"file","file":{"file_id":"file-1","filename":"note.pdf","file_data":"data:application/pdf;base64,cGRm"}},
        {"type":"input_audio","input_audio":{"data":"c291bmQ=","format":"wav"}}
    ]}]});
    let out = convert(Format::OpenAI, Format::Codex, &chat);
    let content = &out["input"][0]["content"];
    assert_eq!(content[0], json!({"type":"input_text","text":"attachments"}));
    assert_eq!(
        content[1],
        json!({"type":"input_file","file_id":"file-1","filename":"note.pdf","file_data":"data:application/pdf;base64,cGRm"})
    );
    assert_eq!(
        content[2],
        json!({"type":"input_audio","data":"c291bmQ=","format":"wav"})
    );
    let claude = json!({"messages":[{"role":"user","content":[{"type":"text","text":"pdf"},
        {"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"cGRm"}}
    ]}]});
    let out = convert(Format::Claude, Format::Codex, &claude);
    assert_eq!(
        out["input"][0]["content"][1],
        json!({"type":"input_file","file_data":"data:application/pdf;base64,cGRm","filename":"document.pdf"})
    );
    let response = json!({"input":[{"role":"user","content":[{"type":"input_text","text":"pdf"},
        {"type":"input_file","filename":"doc.pdf","file_data":"data:application/pdf;base64,cGRm"}
    ]}]});
    let out = convert(Format::OpenAIResponse, Format::Claude, &response);
    assert_eq!(
        out["messages"][0]["content"][1],
        json!({"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"cGRm"}})
    );
}

#[test]
fn unsupported_attachments_refuse_each_emptied_user_turn_but_keep_text_neighbors() {
    let ctx = RequestCtx {
        model: "chosen",
        stream: false,
        tool_search: false,
        system_turns: false,
    };
    for (upstream, part) in [
        (
            Format::OpenAI,
            json!({"type":"input_file","file_url":"https://example.test/a.pdf"}),
        ),
        (Format::Claude, json!({"type":"input_file","file_id":"remote-file"})),
        (
            Format::Claude,
            json!({"type":"input_audio","data":"c291bmQ=","format":"wav"}),
        ),
    ] {
        for turn in [0, 1] {
            let mut messages = vec![
                json!({"role":"user","content":"earlier"}),
                json!({"role":"user","content":"later"}),
            ];
            messages[turn]["content"] = json!([part.clone()]);
            let body = json!({"input":messages});
            assert!(
                translate_request(
                    Format::OpenAIResponse,
                    upstream,
                    &ctx,
                    &serde_json::to_vec(&body).unwrap()
                )
                .is_err(),
                "{upstream:?}: {part}"
            );
            let mut body = body;
            body["input"][turn]["content"]
                .as_array_mut()
                .unwrap()
                .push(json!({"type":"input_text","text":"keep this turn"}));
            let out = convert(Format::OpenAIResponse, upstream, &body);
            assert!(contains(&out, "keep this turn"));
        }
    }
}

#[test]
fn namespace_declarations_choices_and_history_share_the_same_identity() {
    for upstream in [Format::OpenAI, Format::Claude] {
        let body = json!({"tools":[{"type":"namespace","name":"files","tools":[
            {"type":"function","name":"lookup","description":"namespace lookup","parameters":schema()}
        ]}],"tool_choice":{"type":"function","name":"lookup","namespace":"files"},"input":[
            {"role":"user","content":"find"},
            {"type":"function_call","call_id":"c","name":"lookup","namespace":"files","arguments":r#"{"path":"a"}"#},
            {"type":"function_call_output","call_id":"c","output":"located"}
        ]});
        let out = convert(Format::OpenAIResponse, upstream, &body);
        let name = if upstream == Format::OpenAI {
            &out["tools"][0]["function"]["name"]
        } else {
            &out["tools"][0]["name"]
        };
        let declared_name = name.as_str().unwrap();
        assert!(declared_name.contains("files") && declared_name.contains("lookup"));
        assert!(declared_name.len() <= 64);
        assert!(
            declared_name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        );
        let choice = if upstream == Format::OpenAI {
            &out["tool_choice"]["function"]["name"]
        } else {
            &out["tool_choice"]["name"]
        };
        assert_eq!(choice, name);
        if upstream == Format::OpenAI {
            let call = &out["messages"][1]["tool_calls"][0];
            assert_eq!(call["function"]["name"], *name);
            assert_eq!(call["id"], "c");
            assert_eq!(out["messages"][2]["tool_call_id"], "c");
        } else {
            let blocks: Vec<_> = out["messages"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|m| m["content"].as_array().into_iter().flatten())
                .collect();
            let call = blocks.iter().find(|b| b["type"] == "tool_use").unwrap();
            assert_eq!(call["name"], *name);
            assert_eq!(call["id"], "c");
            let result = blocks.iter().find(|b| b["type"] == "tool_result").unwrap();
            assert_eq!(result["tool_use_id"], "c");
        }
    }
}

#[test]
fn parallel_history_pairs_results_by_identity_even_when_results_arrive_in_reverse_order() {
    for (client, upstream) in pairs() {
        let mut body = conversation(client);
        match client {
            Format::Claude => {
                body["messages"] = json!([
                    {"role":"user","content":"two lookups"},
                    {"role":"assistant","content":[{"type":"tool_use","id":"c1","name":"lookup","input":{"path":"a"}},{"type":"tool_use","id":"c2","name":"lookup","input":{"path":"b"}}]},
                    {"role":"user","content":[{"type":"tool_result","tool_use_id":"c2","content":"result-b"},{"type":"tool_result","tool_use_id":"c1","content":"result-a"}]}
                ]);
            }
            Format::OpenAI => {
                body["messages"] = json!([
                    {"role":"user","content":"two lookups"},
                    {"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"lookup","arguments":r#"{"path":"a"}"#}},{"id":"c2","type":"function","function":{"name":"lookup","arguments":r#"{"path":"b"}"#}}]},
                    {"role":"tool","tool_call_id":"c2","content":"result-b"},{"role":"tool","tool_call_id":"c1","content":"result-a"}
                ]);
            }
            _ => {
                body["input"] = json!([
                    {"role":"user","content":"two lookups"},
                    {"type":"function_call","call_id":"c1","name":"lookup","arguments":r#"{"path":"a"}"#},
                    {"type":"function_call","call_id":"c2","name":"lookup","arguments":r#"{"path":"b"}"#},
                    {"type":"function_call_output","call_id":"c2","output":"result-b"},
                    {"type":"function_call_output","call_id":"c1","output":"result-a"}
                ]);
            }
        }
        let out = convert(client, upstream, &body);
        let results: Vec<_> = match upstream {
            Format::Claude => out["messages"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|m| m["content"].as_array().into_iter().flatten())
                .filter(|b| b["type"] == "tool_result")
                .map(|r| (&r["tool_use_id"], r))
                .collect(),
            Format::OpenAI => out["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["role"] == "tool")
                .map(|r| (&r["tool_call_id"], r))
                .collect(),
            _ => out["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|i| i["type"] == "function_call_output")
                .map(|r| (&r["call_id"], r))
                .collect(),
        };
        assert_eq!(results.len(), 2, "{client:?}->{upstream:?}");
        for (id, text) in [("c1", "result-a"), ("c2", "result-b")] {
            let found: Vec<_> = results.iter().filter(|(rid, _)| rid.as_str() == Some(id)).collect();
            assert_eq!(found.len(), 1);
            assert!(contains(found[0].1, text), "{client:?}->{upstream:?}: {out}");
        }
    }
}

#[test]
fn structured_output_keeps_schema_constraints_and_the_client_format_label() {
    let input_schema = json!({"type":"object","properties":{"contract_marker":{"type":"string","enum":["yes","no"]}},"required":["contract_marker"],"additionalProperties":false});
    for (client, upstream) in [
        (Format::Claude, Format::Codex),
        (Format::OpenAI, Format::Codex),
        (Format::OpenAI, Format::Claude),
        (Format::OpenAIResponse, Format::Claude),
        (Format::OpenAIResponse, Format::OpenAI),
    ] {
        let mut body = conversation(client);
        match client {
            Format::Claude => body["output_config"] = json!({"format":{"type":"json_schema","schema":input_schema}}),
            Format::OpenAI => {
                body["response_format"] = json!({"type":"json_schema","json_schema":{"name":"answer_format","strict":true,"schema":input_schema}})
            }
            _ => {
                body["text"] =
                    json!({"format":{"type":"json_schema","name":"answer_format","strict":true,"schema":input_schema}})
            }
        }
        let out = convert(client, upstream, &body);
        if upstream == Format::Claude {
            // Messages uses an explicit schema instruction rather than forwarding a
            // Responses-only format object to the provider.
            assert!(contains(&out, "contract_marker"));
            assert!(contains(&out, "yes"));
            assert!(contains(&out, "no"));
            assert!(contains(&out, "additionalProperties"));
        } else {
            let schema = if upstream == Format::OpenAI {
                &out["response_format"]["json_schema"]["schema"]
            } else {
                &out["text"]["format"]["schema"]
            };
            assert_eq!(*schema, input_schema, "{client:?}->{upstream:?}: {out}");
        }
    }
}

#[test]
fn namespaced_custom_tools_keep_string_input_and_result_pairing_in_both_directions() {
    use cliproxy_internal::translation::{ResponseCtx, pair};
    for upstream in [Format::OpenAI, Format::Claude] {
        let original = json!({"model":"chosen","tools":[{"type":"namespace","name":"shell","tools":[{"type":"custom","name":"execute","description":"run a command"}]}],"input":[
            {"role":"user","content":"run"},
            {"type":"custom_tool_call","call_id":"history-1","name":"execute","namespace":"shell","input":"echo 🙂 <>&"},
            {"type":"custom_tool_call_output","call_id":"history-1","output":"done"}
        ]});
        let translated = convert(Format::OpenAIResponse, upstream, &original);
        let tool = &translated["tools"][0];
        let (name, schema) = if upstream == Format::Claude {
            (&tool["name"], &tool["input_schema"])
        } else {
            (&tool["function"]["name"], &tool["function"]["parameters"])
        };
        assert_eq!(schema["properties"]["input"]["type"], "string");
        assert_eq!(schema["required"], json!(["input"]));
        if upstream == Format::Claude {
            let blocks: Vec<_> = translated["messages"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|m| m["content"].as_array().into_iter().flatten())
                .collect();
            let call = blocks.iter().find(|b| b["type"] == "tool_use").unwrap();
            assert_eq!(call["name"], *name);
            assert_eq!(call["id"], "history-1");
            assert_eq!(call["input"], json!({"input":"echo 🙂 <>&"}));
            assert!(
                blocks
                    .iter()
                    .any(|b| b["type"] == "tool_result" && b["tool_use_id"] == "history-1" && contains(b, "done"))
            );
        } else {
            let call = &translated["messages"][1]["tool_calls"][0];
            assert_eq!(call["id"], "history-1");
            assert_eq!(call["function"]["name"], *name);
            assert_eq!(
                serde_json::from_str::<Value>(call["function"]["arguments"].as_str().unwrap()).unwrap(),
                json!({"input":"echo 🙂 <>&"})
            );
            assert_eq!(translated["messages"][2]["tool_call_id"], "history-1");
        }
        let answer = if upstream == Format::Claude {
            json!({"id":"m","type":"message","role":"assistant","model":"chosen","content":[{"type":"tool_use","id":"new-call","name":name,"input":{"input":"next command 🙂"}}],"stop_reason":"tool_use","usage":{"input_tokens":1,"output_tokens":1}})
        } else {
            json!({"id":"m","model":"chosen","choices":[{"index":0,"message":{"role":"assistant","tool_calls":[{"id":"new-call","type":"function","function":{"name":name,"arguments":r#"{"input":"next command 🙂"}"#}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1,"completion_tokens":1}})
        };
        let original_raw = serde_json::to_vec(&original).unwrap();
        let translated_raw = serde_json::to_vec(&translated).unwrap();
        let ctx = ResponseCtx {
            model: "chosen",
            original_request: &original_raw,
            translated_request: &translated_raw,
            tool_search: false,
        };
        let answer: Value = serde_json::from_slice(
            &(pair(Format::OpenAIResponse, upstream).unwrap().non_stream)(&ctx, &serde_json::to_vec(&answer).unwrap())
                .unwrap(),
        )
        .unwrap();
        let call = answer["output"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["type"] == "custom_tool_call")
            .unwrap();
        assert_eq!(call["call_id"], "new-call");
        assert_eq!(call["name"], "execute");
        assert_eq!(call["namespace"], "shell");
        assert_eq!(call["input"], "next command 🙂");
    }
}

#[test]
fn tool_choice_tables_keep_auto_required_named_and_disabled_intent() {
    for (client, upstream) in pairs()
        .into_iter()
        .filter(|(c, u)| !(*c == Format::OpenAIResponse && *u == Format::Codex))
    {
        for intent in ["auto", "required", "named", "none"] {
            if client == Format::Claude && intent == "none" {
                continue;
            }
            let choice = match (client, intent) {
                (Format::Claude, "auto") => json!({"type":"auto"}),
                (Format::Claude, "required") => json!({"type":"any"}),
                (Format::Claude, _) => json!({"type":"tool","name":"lookup"}),
                (Format::OpenAI, "named") => json!({"type":"function","function":{"name":"lookup"}}),
                (Format::OpenAIResponse, "named") => json!({"type":"function","name":"lookup"}),
                (_, other) => json!(other),
            };
            let mut body = conversation(client);
            body["tool_choice"] = choice;
            let out = convert(client, upstream, &body);
            if upstream == Format::Claude {
                match intent {
                    "none" => {
                        if out
                            .get("tools")
                            .is_some_and(|t| t.as_array().is_some_and(|a| !a.is_empty()))
                        {
                            assert_eq!(out["tool_choice"]["type"], "none");
                        }
                    }
                    "auto" => assert_eq!(out["tool_choice"]["type"], "auto"),
                    "required" => assert_eq!(out["tool_choice"]["type"], "any"),
                    _ => {
                        assert_eq!(out["tool_choice"]["type"], "tool");
                        assert_eq!(out["tool_choice"]["name"], "lookup");
                    }
                }
            } else {
                match intent {
                    "named" => {
                        assert_eq!(out["tool_choice"]["type"], "function");
                        let name = if upstream == Format::OpenAI {
                            &out["tool_choice"]["function"]["name"]
                        } else {
                            &out["tool_choice"]["name"]
                        };
                        assert_eq!(name, "lookup");
                    }
                    other => assert_eq!(out["tool_choice"], other, "{client:?}->{upstream:?} {intent}: {out}"),
                }
            }
        }
    }
    let mut body = conversation(Format::OpenAI);
    body["tools"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type":"function","function":{"name":"other","parameters":{"type":"object"}}}));
    body["tool_choice"] = json!({"type":"allowed_tools","allowed_tools":{"mode":"auto","tools":[{"type":"function","function":{"name":"lookup"}}]}});
    let out = convert(Format::OpenAI, Format::Claude, &body);
    assert_eq!(out["tools"].as_array().unwrap().len(), 1);
    assert_eq!(out["tools"][0]["name"], "lookup");
}

#[test]
fn colliding_long_namespace_identities_survive_request_choice_and_both_response_modes() {
    use cliproxy_internal::translation::{ResponseCtx, pair};
    for upstream in [Format::OpenAI, Format::Claude] {
        let namespaces = ["a".repeat(70), format!("{}b", "a".repeat(69)), String::new()];
        let declarations: Vec<_> = namespaces
            .iter()
            .enumerate()
            .map(|(i, namespace)| {
                let tool =
                    json!({"type":"function","name":"lookup","description":format!("owner-{i}"),"parameters":schema()});
                if namespace.is_empty() {
                    tool
                } else {
                    json!({"type":"namespace","name":namespace,"tools":[tool]})
                }
            })
            .collect();
        let mut history = vec![json!({"role":"user","content":"look up all"})];
        for (i, namespace) in namespaces.iter().enumerate() {
            let mut call = json!({"type":"function_call","call_id":format!("c{i}"),"name":"lookup","arguments":json!({"path":format!("p{i}")}).to_string()});
            if !namespace.is_empty() {
                call["namespace"] = json!(namespace)
            }
            history.push(call);
            history.push(json!({"type":"function_call_output","call_id":format!("c{i}"),"output":format!("r{i}")}));
        }
        let original = json!({"model":"chosen","stream":true,"tools":declarations,"input":history});
        let translated = convert(Format::OpenAIResponse, upstream, &original);
        let tools = translated["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3);
        let names: Vec<_> = (0..3)
            .map(|i| {
                let tool = tools
                    .iter()
                    .find(|t| {
                        if upstream == Format::Claude {
                            t["description"] == format!("owner-{i}")
                        } else {
                            t["function"]["description"] == format!("owner-{i}")
                        }
                    })
                    .unwrap();
                if upstream == Format::Claude {
                    tool["name"].as_str().unwrap().to_owned()
                } else {
                    tool["function"]["name"].as_str().unwrap().to_owned()
                }
            })
            .collect();
        assert_eq!(names.iter().collect::<std::collections::HashSet<_>>().len(), 3);
        assert!(names.iter().all(|n| n.len() <= 64));
        let mut history_names = std::collections::HashMap::new();
        if upstream == Format::Claude {
            for message in translated["messages"].as_array().unwrap() {
                for block in message["content"].as_array().into_iter().flatten() {
                    if block["type"] == "tool_use" {
                        history_names.insert(
                            block["id"].as_str().unwrap().to_owned(),
                            block["name"].as_str().unwrap().to_owned(),
                        );
                    }
                }
            }
        } else {
            for message in translated["messages"].as_array().unwrap() {
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    history_names.insert(
                        call["id"].as_str().unwrap().to_owned(),
                        call["function"]["name"].as_str().unwrap().to_owned(),
                    );
                }
            }
        }
        for (i, name) in names.iter().enumerate() {
            assert_eq!(history_names[&format!("c{i}")], *name);
            let mut chosen = original.clone();
            chosen["tool_choice"] = json!({"type":"function","name":"lookup","namespace":namespaces[i]});
            let chosen = convert(Format::OpenAIResponse, upstream, &chosen);
            let actual = if upstream == Format::Claude {
                &chosen["tool_choice"]["name"]
            } else {
                &chosen["tool_choice"]["function"]["name"]
            };
            assert_eq!(actual, name);
        }
        let original_raw = serde_json::to_vec(&original).unwrap();
        let translated_raw = serde_json::to_vec(&translated).unwrap();
        let ctx = ResponseCtx {
            model: "chosen",
            original_request: &original_raw,
            translated_request: &translated_raw,
            tool_search: false,
        };
        let calls:Vec<_>=names.iter().enumerate().map(|(i,n)|if upstream==Format::Claude {json!({"type":"tool_use","id":format!("new-{i}"),"name":n,"input":{"path":format!("p{i}")}})} else {json!({"id":format!("new-{i}"),"type":"function","function":{"name":n,"arguments":json!({"path":format!("p{i}")}).to_string()}})}).collect();
        let buffered = if upstream == Format::Claude {
            json!({"id":"m","type":"message","role":"assistant","model":"chosen","content":calls,"stop_reason":"tool_use","usage":{"input_tokens":1,"output_tokens":1}})
        } else {
            json!({"id":"m","model":"chosen","choices":[{"index":0,"message":{"role":"assistant","tool_calls":calls},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1,"completion_tokens":1}})
        };
        let buffered: Value = serde_json::from_slice(
            &(pair(Format::OpenAIResponse, upstream).unwrap().non_stream)(
                &ctx,
                &serde_json::to_vec(&buffered).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        let mut stream = (pair(Format::OpenAIResponse, upstream).unwrap().stream)(&ctx);
        let mut frames = vec![];
        let mut send = |event: Value| frames.extend(stream.event(format!("data: {event}\n\n").as_bytes()).unwrap());
        if upstream == Format::Claude {
            send(
                json!({"type":"message_start","message":{"id":"m","model":"chosen","usage":{"input_tokens":1,"output_tokens":0}}}),
            );
        }
        for (i, name) in names.iter().enumerate() {
            let args = json!({"path":format!("p{i}")}).to_string();
            if upstream == Format::Claude {
                send(
                    json!({"type":"content_block_start","index":i,"content_block":{"type":"tool_use","id":format!("new-{i}"),"name":name,"input":{}}}),
                );
                send(
                    json!({"type":"content_block_delta","index":i,"delta":{"type":"input_json_delta","partial_json":args}}),
                );
                send(json!({"type":"content_block_stop","index":i}));
            } else {
                send(
                    json!({"id":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":i,"id":format!("new-{i}"),"function":{"name":name,"arguments":args}}]},"finish_reason":null}]}),
                );
            }
        }
        if upstream == Format::Claude {
            send(json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":1}}));
            send(json!({"type":"message_stop"}));
        } else {
            send(json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}));
        }
        if upstream == Format::OpenAI {
            frames.extend(stream.event(b"data: [DONE]\n\n").unwrap());
        }
        frames.extend(stream.finish().unwrap());
        let streamed: Value = frames
            .iter()
            .flat_map(|f| f.split(|&b| b == b'\n'))
            .filter_map(|l| l.strip_prefix(b"data: "))
            .filter_map(|d| serde_json::from_slice::<Value>(d).ok())
            .find(|e| e["type"] == "response.completed")
            .unwrap()["response"]
            .clone();
        for answer in [&buffered, &streamed] {
            let output = answer["output"].as_array().unwrap();
            for (i, namespace) in namespaces.iter().enumerate() {
                let call = output.iter().find(|c| c["call_id"] == format!("new-{i}")).unwrap();
                assert_eq!(call["name"], "lookup");
                if !namespace.is_empty() {
                    assert_eq!(call["namespace"], *namespace);
                } else {
                    assert!(call.get("namespace").is_none() || call["namespace"] == "");
                }
                assert_eq!(
                    serde_json::from_str::<Value>(call["arguments"].as_str().unwrap()).unwrap(),
                    json!({"path":format!("p{i}")})
                );
            }
        }
        let mut ambiguous = original.clone();
        ambiguous["tools"].as_array_mut().unwrap().pop();
        ambiguous["input"] = json!([{"role":"user","content":"ambiguous"},{"type":"function_call","call_id":"bare","name":"lookup","arguments":"{}"}]);
        let ambiguous = convert(Format::OpenAIResponse, upstream, &ambiguous);
        let bare_name = if upstream == Format::OpenAI {
            &ambiguous["messages"][1]["tool_calls"][0]["function"]["name"]
        } else {
            &ambiguous["messages"][1]["content"][0]["name"]
        };
        assert!(
            names[..2].iter().all(|n| bare_name.as_str() != Some(n.as_str())),
            "ambiguous local name captured a foreign namespace: {ambiguous}"
        );
    }
}

#[test]
fn codex_reasoning_carriers_replay_opaquely_in_order_and_foreign_thinking_is_refused() {
    use base64::Engine as _;
    // A synthetic Fernet-shaped transport token, never a provider-acceptance claim.
    // CONTRACT.md Translated route requires an opaque encrypted-content round trip.
    let mut token = vec![0_u8; 73];
    token[0] = 0x80;
    token[24] = 3;
    token[40] = 9;
    let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(token);
    let ctx = RequestCtx {
        model: "gpt-5",
        stream: true,
        tool_search: false,
        system_turns: false,
    };
    let request = json!({"tools":[{"name":"lookup","input_schema":schema()}],"messages":[
        {"role":"user","content":"start"},
        {"role":"assistant","content":[{"type":"text","text":"before reasoning"},{"type":"thinking","thinking":"private summary","signature":signature},{"type":"tool_use","id":"c","name":"lookup","input":{"path":"a"}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"c","content":"result"},{"type":"text","text":"continue"}]}
    ]});
    let raw = serde_json::to_vec(&request).unwrap();
    let out: Value = serde_json::from_slice(
        &cliproxy_internal::translation::codex_request(Format::Claude, &ctx, &raw)
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let items = out["input"].as_array().unwrap();
    let reasoning = items.iter().position(|i| i["type"] == "reasoning").unwrap();
    let call = items.iter().position(|i| i["type"] == "function_call").unwrap();
    let result = items.iter().position(|i| i["type"] == "function_call_output").unwrap();
    assert!(items[..reasoning].iter().any(|i| contains(i, "before reasoning")));
    assert!(reasoning < call && call < result);
    assert_eq!(items[reasoning]["encrypted_content"], signature);
    assert!(!contains(&out, "private summary"));
    assert_eq!(items[call]["call_id"], "c");
    assert_eq!(items[result]["call_id"], "c");
    for foreign in ["", "not-a-codex-carrier", "sealed.v1.foreign-provider"] {
        let mut invalid = request.clone();
        invalid["messages"][1]["content"][1]["signature"] = json!(foreign);
        let raw = serde_json::to_vec(&invalid).unwrap();
        let error = cliproxy_internal::translation::codex_request(Format::Claude, &ctx, &raw)
            .unwrap()
            .unwrap_err();
        assert!(error.0.contains("thinking"));
    }
    // A separately configured compatibility adapter can retain unsigned history;
    // this does not authorize those bytes on the strict Codex route above.
    let mut compatible = request;
    compatible["messages"][1]["content"][1]["signature"] = json!("compat-only-opaque");
    let compatible: Value = serde_json::from_slice(
        &cliproxy_internal::translation::claude_to_codex_with_compat(&ctx, &serde_json::to_vec(&compatible).unwrap())
            .unwrap(),
    )
    .unwrap();
    let reasoning = compatible["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "reasoning")
        .unwrap();
    assert_eq!(reasoning["encrypted_content"], "compat-only-opaque");
}

#[test]
fn top_level_function_definitions_win_over_additional_duplicates_without_hiding_distinct_tools() {
    for upstream in [Format::OpenAI, Format::Claude] {
        let body = json!({"tools":[{"type":"function","name":"lookup","description":"top-level owner","parameters":schema()}],"input":[
            {"type":"additional_tools","tools":[
                {"type":"function","name":"lookup","description":"additional duplicate","parameters":{"type":"object","properties":{"wrong":{"type":"boolean"}}}},
                {"type":"function","name":"extra","description":"additional-only","parameters":{"type":"object","properties":{"extra_value":{"type":"integer"}}}}
            ]},
            {"role":"user","content":"question"},
            {"type":"function_call","call_id":"a","name":"lookup","arguments":r#"{"path":"a"}"#},
            {"type":"function_call_output","call_id":"a","output":"first result"},
            {"type":"function_call","call_id":"b","name":"extra","arguments":r#"{"extra_value":2}"#},
            {"type":"function_call_output","call_id":"b","output":"second result"}
        ]});
        let out = convert(Format::OpenAIResponse, upstream, &body);
        let declarations: Vec<_> = out["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| if upstream == Format::Claude { t } else { &t["function"] })
            .collect();
        assert_eq!(declarations.len(), 2);
        let chosen = declarations.iter().find(|t| t["name"] == "lookup").unwrap();
        assert_eq!(chosen["description"], "top-level owner");
        let schema = if upstream == Format::Claude {
            &chosen["input_schema"]
        } else {
            &chosen["parameters"]
        };
        assert_eq!(schema["properties"], self::schema()["properties"]);
        let extra = declarations.iter().find(|t| t["name"] == "extra").unwrap();
        assert_eq!(extra["description"], "additional-only");
        assert!(contains(&out, "first result") && contains(&out, "second result"));
        if upstream == Format::OpenAI {
            let calls: Vec<_> = out["messages"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|m| m["tool_calls"].as_array().into_iter().flatten())
                .collect();
            assert!(
                calls
                    .iter()
                    .any(|c| c["id"] == "a" && c["function"]["name"] == "lookup")
            );
            assert!(calls.iter().any(|c| c["id"] == "b" && c["function"]["name"] == "extra"));
        } else {
            let calls: Vec<_> = out["messages"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|m| m["content"].as_array().into_iter().flatten())
                .collect();
            assert!(
                calls
                    .iter()
                    .any(|c| c["type"] == "tool_use" && c["id"] == "a" && c["name"] == "lookup")
            );
            assert!(
                calls
                    .iter()
                    .any(|c| c["type"] == "tool_use" && c["id"] == "b" && c["name"] == "extra")
            );
        }
    }
}
