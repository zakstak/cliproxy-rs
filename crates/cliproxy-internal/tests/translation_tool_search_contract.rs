//! Client-executed Responses tool discovery mapped to Claude tool declarations and
//! tool_use/tool_result pairs. These expectations come from the two schemas and the
//! adapter's selected discovery policy, not from a second implementation's output.
use cliproxy_internal::format::Format;
use cliproxy_internal::translation::{RequestCtx, ResponseCtx, pair, translate_request};
use serde_json::{Value, json};

fn convert(mut request: Value, enabled: bool) -> Value {
    request["model"] = json!("claude-sonnet-4-5");
    let ctx = RequestCtx {
        registry: None,
        model: "claude-sonnet-4-5",
        stream: false,
        tool_search: enabled,
        system_turns: false,
    };
    serde_json::from_slice(
        &translate_request(
            Format::OpenAIResponse,
            Format::Claude,
            &ctx,
            &serde_json::to_vec(&request).unwrap(),
        )
        .unwrap(),
    )
    .unwrap()
}
fn discovery(tools: Value) -> Value {
    json!({"tools":[{"type":"tool_search","execution":"client"}],"input":[
        {"role":"user","content":"Find a tool"},
        {"type":"tool_search_call","call_id":"search_1","execution":"client","arguments":{"query":"files","limit":3}},
        {"type":"tool_search_output","call_id":"search_1","tools":tools},
        {"role":"user","content":"Continue"}
    ]})
}
fn tool(name: &str) -> Value {
    json!({"type":"function","name":name,"parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}})
}
fn declarations(response: &Value) -> Vec<&Value> {
    response["tools"].as_array().into_iter().flatten().collect()
}
fn blocks(response: &Value) -> Vec<&Value> {
    response["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["content"].as_array().into_iter().flatten())
        .collect()
}

#[test]
fn client_search_schema_requires_a_string_query_even_with_a_custom_schema() {
    for parameters in [
        json!(null),
        json!({"type":"object","properties":{"query":{"type":"number"},"extra":{"type":"boolean"}},"required":["extra"]}),
    ] {
        let out = convert(
            json!({"tools":[{"type":"tool_search","execution":"client","parameters":parameters}],"input":"question"}),
            true,
        );
        let search = declarations(&out)
            .into_iter()
            .find(|t| t["name"] == "tool_search")
            .unwrap();
        assert_eq!(search["input_schema"]["type"], "object");
        assert_eq!(search["input_schema"]["properties"]["query"]["type"], "string");
        assert!(
            search["input_schema"]["required"]
                .as_array()
                .unwrap()
                .contains(&json!("query"))
        );
        assert!(search.get("execution").is_none());
    }
}

#[test]
fn hosted_search_is_never_exposed_as_a_claude_function() {
    for enabled in [false, true] {
        let out = convert(
            json!({"tools":[{"type":"tool_search","execution":"server"}],"input":"question"}),
            enabled,
        );
        assert!(declarations(&out).is_empty());
    }
}

#[test]
fn search_switch_changes_only_search_support_for_ordinary_tools() {
    let request = json!({"tools":[tool("ordinary")],"input":"question"});
    assert_eq!(convert(request.clone(), false), convert(request, true));
    let request = json!({"tools":[{"type":"tool_search","execution":"client"},tool("ordinary")],"input":"question"});
    let off = convert(request.clone(), false);
    let on = convert(request, true);
    assert_eq!(off["messages"], on["messages"]);
    let ordinary = |out: &Value| {
        declarations(out)
            .into_iter()
            .find(|t| t["name"] == "ordinary")
            .unwrap()
            .clone()
    };
    assert_eq!(ordinary(&off), ordinary(&on));
    assert!(!declarations(&off).iter().any(|t| t["name"] == "tool_search"));
}

#[test]
fn discovered_tools_have_matching_calls_results_and_deferred_references() {
    let out = convert(discovery(json!([tool("read_file"), tool("read_file")])), true);
    let declarations = declarations(&out);
    let read: Vec<&&Value> = declarations.iter().filter(|t| t["name"] == "read_file").collect();
    assert_eq!(read.len(), 1);
    assert_eq!(read[0]["defer_loading"], true);
    assert_eq!(read[0]["input_schema"]["required"], json!(["path"]));
    let content = blocks(&out);
    let call = content.iter().find(|p| p["type"] == "tool_use").unwrap();
    assert_eq!(call["name"], "tool_search");
    assert_eq!(call["input"], json!({"query":"files","limit":3}));
    let result = content.iter().find(|p| p["type"] == "tool_result").unwrap();
    assert_eq!(result["tool_use_id"], call["id"]);
    assert_eq!(
        result["content"],
        json!([{"type":"tool_reference","tool_name":"read_file"}])
    );
}

#[test]
fn directly_declared_tools_win_and_are_not_referenced_as_discoveries() {
    let mut request = discovery(json!([tool("read_file"), tool("later")]));
    let mut direct = tool("read_file");
    direct["description"] = json!("direct wins");
    request["tools"].as_array_mut().unwrap().push(direct);
    let out = convert(request, true);
    let read = declarations(&out)
        .into_iter()
        .find(|t| t["name"] == "read_file")
        .unwrap();
    assert_eq!(read["description"], "direct wins");
    assert!(read.get("defer_loading").is_none());
    let content = blocks(&out);
    let result = content.iter().find(|p| p["type"] == "tool_result").unwrap();
    assert_eq!(
        result["content"],
        json!([{"type":"tool_reference","tool_name":"later"}])
    );
}

#[test]
fn tools_wait_for_discovery_and_empty_results_remain_readable() {
    let mut deferred = tool("read_file");
    deferred["defer_loading"] = json!(true);
    let request = json!({"tools":[{"type":"tool_search","execution":"client"},deferred],"input":"question"});
    let out = convert(request, true);
    assert!(!declarations(&out).iter().any(|t| t["name"] == "read_file"));
    let out = convert(discovery(json!([])), true);
    let content = blocks(&out);
    let result = content.iter().find(|p| p["type"] == "tool_result").unwrap();
    assert!(result["content"].as_str().unwrap().contains("No matching tools"));
}

#[test]
fn discoveries_append_after_a_stable_prefix_and_cannot_carry_cache_hints() {
    let first = convert(discovery(json!([tool("first")])), true);
    let mut discovered = tool("second");
    discovered["cache_control"] = json!({"type":"ephemeral","ttl":"1h"});
    let second = convert(discovery(json!([tool("first"), discovered])), true);
    assert_eq!(
        second["tools"].as_array().unwrap()[..first["tools"].as_array().unwrap().len()],
        first["tools"].as_array().unwrap()[..]
    );
    let second = declarations(&second)
        .into_iter()
        .find(|t| t["name"] == "second")
        .unwrap();
    assert_eq!(second["defer_loading"], true);
    assert!(second.get("cache_control").is_none());
}

#[test]
fn search_results_return_client_search_calls_in_streamed_and_buffered_responses() {
    let original = serde_json::to_vec(&discovery(json!([tool("read_file")]))).unwrap();
    let translated = serde_json::to_vec(&convert(discovery(json!([tool("read_file")])), true)).unwrap();
    let ctx = ResponseCtx {
        model: "claude-sonnet-4-5",
        original_request: &original,
        translated_request: &translated,
        tool_search: true,
    };
    for (arguments, stop, status) in [
        (r#"{"query":"files","limit":3}"#, "tool_use", "completed"),
        (r#"{"query":"files"#, "max_tokens", "incomplete"),
    ] {
        let events = [
            json!({"type":"message_start","message":{"id":"message-1","model":"claude-sonnet-4-5","usage":{"input_tokens":3,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"new_search","name":"tool_search","input":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":arguments}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":stop},"usage":{"output_tokens":5}}),
            json!({"type":"message_stop"}),
        ];
        let wire: Vec<Vec<u8>> = events
            .iter()
            .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()).into_bytes())
            .collect();
        let pair = pair(Format::OpenAIResponse, Format::Claude).unwrap();
        let buffered: Value = serde_json::from_slice(&(pair.non_stream)(&ctx, &wire.concat()).unwrap()).unwrap();
        let mut stream = (pair.stream)(&ctx);
        let mut frames = vec![];
        for event in &wire {
            frames.extend(stream.event(event).unwrap());
        }
        frames.extend(stream.finish().unwrap());
        let output: String = frames.iter().map(|f| String::from_utf8_lossy(f)).collect();
        let completed: Value = output
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|d| serde_json::from_str::<Value>(d).ok())
            .find(|e| e["type"] == "response.completed" || e["type"] == "response.incomplete")
            .unwrap();
        for response in [&buffered, &completed["response"]] {
            let call = response["output"]
                .as_array()
                .unwrap()
                .iter()
                .find(|i| i["type"] == "tool_search_call")
                .unwrap();
            assert_eq!(call["execution"], "client");
            assert_eq!(call["status"], status);
            assert!(call["call_id"].as_str().unwrap().contains("new_search"));
            if status == "completed" {
                assert_eq!(call["arguments"], json!({"query":"files","limit":3}));
            }
        }
    }
}

#[test]
fn namespaced_discoveries_keep_qualified_history_and_reference_the_declared_name() {
    let namespace = json!({"type":"namespace","name":"files","tools":[tool("read")]});
    let mut request = discovery(json!([namespace]));
    let history = request["input"].as_array_mut().unwrap();
    history.insert(3,json!({"type":"function_call","namespace":"files","name":"read","call_id":"read_1","arguments":r#"{"path":"a.txt"}"#}));
    history.insert(
        4,
        json!({"type":"function_call_output","call_id":"read_1","output":"contents"}),
    );
    let out = convert(request, true);
    let declared = declarations(&out)
        .into_iter()
        .find(|t| t["name"] == "files__read")
        .unwrap();
    assert_eq!(declared["input_schema"]["required"], json!(["path"]));
    let content = blocks(&out);
    let call = content
        .iter()
        .find(|p| p["type"] == "tool_use" && p["name"] == "files__read")
        .unwrap();
    assert_eq!(call["input"], json!({"path":"a.txt"}));
    let result = content
        .iter()
        .find(|p| p["type"] == "tool_result" && p["tool_use_id"] == call["id"])
        .unwrap();
    assert_eq!(result["content"], "contents");
    let search = content
        .iter()
        .find(|p| p["type"] == "tool_result" && p["tool_use_id"] == "search_1")
        .unwrap();
    assert_eq!(
        search["content"],
        json!([{"type":"tool_reference","tool_name":"files__read"}])
    );
}

#[test]
fn a_client_declaration_owns_the_anchor_name_and_is_never_replaced_by_a_placeholder() {
    let mut request = discovery(json!([tool("later")]));
    let mut anchor = tool("tool_search_deferred_anchor");
    anchor["description"] = json!("client-owned");
    request["tools"].as_array_mut().unwrap().push(anchor);
    let out = convert(request, true);
    let anchor: Vec<&Value> = declarations(&out)
        .into_iter()
        .filter(|t| t["name"] == "tool_search_deferred_anchor")
        .collect();
    assert_eq!(anchor.len(), 1);
    assert_eq!(anchor[0]["description"], "client-owned");
    assert!(anchor[0].get("defer_loading").is_none());
}

#[test]
fn a_same_named_function_wins_by_declaration_precedence_and_stays_a_function() {
    let mut deferred = tool("available");
    deferred["defer_loading"] = json!(true);
    let request =
        json!({"tools":[tool("tool_search"),{"type":"tool_search","execution":"client"},deferred],"input":"question"});
    let out = convert(request.clone(), true);
    let search = declarations(&out)
        .into_iter()
        .find(|t| t["name"] == "tool_search")
        .unwrap();
    assert_eq!(search["input_schema"]["properties"], json!({"path":{"type":"string"}}));
    assert!(declarations(&out).iter().any(|t| t["name"] == "available"));
    assert!(
        !declarations(&out)
            .iter()
            .any(|t| t["name"] == "tool_search_deferred_anchor")
    );
    let original = serde_json::to_vec(&request).unwrap();
    let translated = serde_json::to_vec(&out).unwrap();
    let ctx = ResponseCtx {
        model: "claude-sonnet-4-5",
        original_request: &original,
        translated_request: &translated,
        tool_search: true,
    };
    let events = [
        json!({"type":"message_start","message":{"id":"m","model":"claude-sonnet-4-5","usage":{"input_tokens":1,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"ordinary","name":"tool_search","input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":r#"{"path":"a"}"#}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":1}}),
        json!({"type":"message_stop"}),
    ];
    let wire: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
    let response: Value = serde_json::from_slice(
        &(pair(Format::OpenAIResponse, Format::Claude).unwrap().non_stream)(&ctx, wire.as_bytes()).unwrap(),
    )
    .unwrap();
    assert_eq!(response["output"][0]["type"], "function_call");
    assert_eq!(response["output"][0]["name"], "tool_search");
}

#[test]
fn sanitized_function_names_cannot_capture_the_search_identity() {
    let mut request = discovery(json!([tool("tool search")]));
    let history = request["input"].as_array_mut().unwrap();
    history.insert(3,json!({"type":"function_call","name":"tool search","call_id":"ordinary_call","arguments":r#"{"path":"a.txt"}"#}));
    history.insert(
        4,
        json!({"type":"function_call_output","call_id":"ordinary_call","output":"contents"}),
    );
    let out = convert(request, true);
    let ordinary = declarations(&out)
        .into_iter()
        .find(|t| t["input_schema"]["properties"].get("path").is_some())
        .unwrap();
    let ordinary_name = ordinary["name"].as_str().unwrap();
    assert_ne!(ordinary_name, "tool_search");
    assert!(ordinary_name.len() <= 64);
    let names: std::collections::HashSet<&str> =
        declarations(&out).iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names.len(), declarations(&out).len());
    let content = blocks(&out);
    let call = content
        .iter()
        .find(|p| p["type"] == "tool_use" && p["id"] == "ordinary_call")
        .unwrap();
    assert_eq!(call["name"], ordinary_name);
    let search = content
        .iter()
        .find(|p| p["type"] == "tool_use" && p["id"] == "search_1")
        .unwrap();
    assert_eq!(search["name"], "tool_search");
    let result = content
        .iter()
        .find(|p| p["type"] == "tool_result" && p["tool_use_id"] == "search_1")
        .unwrap();
    assert_eq!(
        result["content"],
        json!([{"type":"tool_reference","tool_name":ordinary_name}])
    );
}
