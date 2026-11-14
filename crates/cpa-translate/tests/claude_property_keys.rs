//! Tool property keys Claude rejects on the translated routes into Claude (#77).
//!
//! Claude refuses a request whose tool `input_schema` declares a property key outside
//! `^[a-zA-Z0-9_.-]{1,64}$` (`400 tools.N.custom.input_schema.properties: Property keys
//! should match pattern ...`). Codex sends GitLab MCP tools with keys such as
//! `not[labels]` on every turn, so one such tool fails the whole conversation. The
//! Responses and Chat Completions translators rename each rejected key in the schema
//! they send and in replayed tool calls, and restore the client's keys in the tool calls
//! Claude returns, buffered and streamed.
use cpa_core::format::Format;
use cpa_translate::{RequestCtx, ResponseCtx, pair, translate_request};
use serde_json::{Value, json};

const CTX: RequestCtx<'static> = RequestCtx {
    model: "claude-opus-5-5",
    stream: true,
    tool_search: false,
};

fn accepted(key: &str) -> bool {
    (1..=64).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// Every key of every `properties` object in `schema`, in any position.
fn property_keys(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if key == "properties"
                    && let Some(props) = child.as_object()
                {
                    out.extend(props.keys().cloned());
                }
                property_keys(child, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|i| property_keys(i, out)),
        _ => {}
    }
}

/// The GitLab-style schema from router-for-me/CLIProxyAPI#7389, with nesting.
fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "not[labels]": {"type": "string", "description": "Exclude labels"},
            "labels": {"type": "string"},
            "filter": {
                "type": "object",
                "properties": {"author username": {"type": "string"}, "state": {"type": "string"}},
                "required": ["author username"]
            },
            "pages": {"type": "array", "items": {"type": "object", "properties": {"page[size]": {"type": "integer"}}}}
        },
        "required": ["not[labels]"]
    })
}

fn responses_request() -> Vec<u8> {
    json!({
        "model": "claude-opus-5-5",
        "stream": true,
        "tools": [
            {"type": "function", "name": "list_merge_requests", "parameters": schema()},
            {"type": "function", "name": "plain", "parameters": {"type": "object", "properties": {"q": {"type": "string"}}}}
        ],
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
            {"type": "function_call", "call_id": "call_1", "name": "list_merge_requests",
             "arguments": "{\"not[labels]\":\"bug\",\"labels\":\"x\",\"filter\":{\"author username\":\"zak\"},\"pages\":[{\"page[size]\":5}]}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "[]"}
        ]
    })
    .to_string()
    .into_bytes()
}

fn chat_request() -> Vec<u8> {
    json!({
        "model": "claude-opus-5-5",
        "stream": true,
        "tools": [{"type": "function", "function": {"name": "list_merge_requests", "parameters": schema()}}],
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": null, "tool_calls": [{"id": "call_1", "type": "function",
             "function": {"name": "list_merge_requests", "arguments": "{\"not[labels]\":\"bug\",\"filter\":{\"author username\":\"zak\"}}"}}]},
            {"role": "tool", "tool_call_id": "call_1", "content": "[]"}
        ]
    })
    .to_string()
    .into_bytes()
}

fn translate(client: Format, body: &[u8]) -> Value {
    let out = translate_request(client, Format::Claude, &CTX, body).unwrap();
    serde_json::from_slice(&out).unwrap()
}

fn claude_tool<'a>(out: &'a Value, name: &str) -> &'a Value {
    out["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == name)
        .unwrap()
}

fn tool_use_input(out: &Value) -> &Value {
    out["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["content"].as_array().into_iter().flatten())
        .find(|b| b["type"] == "tool_use")
        .map(|b| &b["input"])
        .unwrap()
}

fn assert_renamed_schema(out: &Value) {
    let schema = &claude_tool(out, "list_merge_requests")["input_schema"];
    let mut keys = vec![];
    property_keys(schema, &mut keys);
    assert!(keys.iter().all(|k| accepted(k)), "{keys:?} in {schema}");
    assert_eq!(schema["properties"]["not_labels_"]["description"], "Exclude labels");
    assert_eq!(schema["properties"]["labels"], json!({"type": "string"}));
    assert_eq!(schema["required"], json!(["not_labels_"]));
    let filter = &schema["properties"]["filter"];
    assert_eq!(filter["properties"]["author_username"], json!({"type": "string"}));
    assert_eq!(filter["required"], json!(["author_username"]));
    assert_eq!(
        schema["properties"]["pages"]["items"]["properties"]["page_size_"],
        json!({"type": "integer"})
    );
}

#[test]
fn responses_request_renames_rejected_keys_in_schema_and_history() {
    let out = translate(Format::OpenAIResponse, &responses_request());
    assert_renamed_schema(&out);
    // A tool Claude accepts keeps its schema.
    assert_eq!(
        claude_tool(&out, "plain")["input_schema"],
        json!({"type": "object", "properties": {"q": {"type": "string"}}})
    );
    assert_eq!(
        tool_use_input(&out),
        &json!({"not_labels_": "bug", "labels": "x", "filter": {"author_username": "zak"}, "pages": [{"page_size_": 5}]})
    );
}

#[test]
fn chat_request_renames_rejected_keys_in_schema_and_history() {
    let out = translate(Format::OpenAI, &chat_request());
    assert_renamed_schema(&out);
    assert_eq!(
        tool_use_input(&out),
        &json!({"not_labels_": "bug", "filter": {"author_username": "zak"}})
    );
}

#[test]
fn renamed_keys_avoid_every_declared_key() {
    let body = json!({
        "model": "m",
        "tools": [{"type": "function", "name": "t", "parameters": {"type": "object", "properties": {
            "a[b]": {"type": "string"},
            "a_b_": {"type": "string"},
            "nested": {"type": "object", "properties": {"a_b__2": {"type": "string"}, "a(b)": {"type": "string"}}}
        }}}],
        "input": "hi"
    })
    .to_string();
    let out = translate(Format::OpenAIResponse, body.as_bytes());
    let schema = &claude_tool(&out, "t")["input_schema"];
    let mut keys = vec![];
    property_keys(schema, &mut keys);
    keys.sort();
    // a[b] and a(b) both sanitize to a_b_, which is declared, as is a_b__2.
    assert_eq!(keys, ["a_b_", "a_b__2", "a_b__3", "a_b__4", "nested"], "{schema}");
    assert!(keys.iter().all(|k| accepted(k)));

    // The client sees exactly its own keys back.
    let claude = claude_answer(
        json!({"a_b__3": 1, "a_b_": 2, "nested": {"a_b__2": 3, "a_b__4": 4}}),
        "t",
    );
    let answer = non_stream(Format::OpenAIResponse, body.as_bytes(), &claude);
    let args = parse_args(&responses_function_call(&answer)["arguments"]);
    assert_eq!(args, json!({"a[b]": 1, "a_b_": 2, "nested": {"a_b__2": 3, "a(b)": 4}}));
}

#[test]
fn long_and_non_ascii_keys_become_valid() {
    let long = "x".repeat(70);
    let body = json!({
        "model": "m",
        "tools": [{"type": "function", "name": "t", "parameters": {"type": "object", "properties": {
            long.clone(): {"type": "string"}, "": {"type": "string"}, "größe": {"type": "string"}
        }}}],
        "input": "hi"
    })
    .to_string();
    let out = translate(Format::OpenAIResponse, body.as_bytes());
    let mut keys = vec![];
    property_keys(&claude_tool(&out, "t")["input_schema"], &mut keys);
    assert_eq!(keys.len(), 3);
    assert!(keys.iter().all(|k| accepted(k)), "{keys:?}");
}

// --- responses ---

fn response_ctx(request: &[u8]) -> ResponseCtx<'_> {
    ResponseCtx {
        model: "claude-opus-5-5",
        original_request: request,
        translated_request: b"{}",
        tool_search: false,
    }
}

/// A Claude answer that calls `name` with `input`, streamed in fragments that split the
/// renamed keys.
fn claude_events(input: &Value, name: &str) -> Vec<Value> {
    claude_text_events(&input.to_string(), name)
}

/// [`claude_events`] for arguments spelled as `text`.
fn claude_text_events(text: &str, name: &str) -> Vec<Value> {
    let (a, b) = text.split_at(text.len() / 2);
    let (a1, a2) = a.split_at(a.len() / 2);
    let mut events = vec![
        json!({"type":"message_start","message":{"id":"msg","type":"message","role":"assistant","model":"m","content":[],"stop_reason":null,"usage":{"input_tokens":5,"output_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":name,"input":{}}}),
    ];
    for part in [a1, a2, b] {
        events.push(
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":part}}),
        );
    }
    events.extend([
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":10}}),
        json!({"type":"message_stop"}),
    ]);
    events
}

fn sse(events: &[Value]) -> Vec<Vec<u8>> {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()).into_bytes())
        .collect()
}

fn claude_answer(input: Value, name: &str) -> Vec<u8> {
    sse(&claude_events(&input, name)).concat()
}

fn stream(client: Format, request: &[u8], events: &[Value]) -> Vec<Value> {
    let pair = pair(client, Format::Claude).unwrap();
    let mut s = (pair.stream)(&response_ctx(request));
    let mut chunks = vec![];
    for event in sse(events) {
        chunks.extend(s.event(&event).unwrap());
    }
    chunks.extend(s.finish().unwrap());
    let text: String = chunks.iter().map(|c| String::from_utf8_lossy(c).into_owned()).collect();
    text.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter(|d| *d != "[DONE]")
        .map(|d| serde_json::from_str(d).unwrap())
        .collect()
}

fn non_stream(client: Format, request: &[u8], body: &[u8]) -> Value {
    let pair = pair(client, Format::Claude).unwrap();
    serde_json::from_slice(&(pair.non_stream)(&response_ctx(request), body).unwrap()).unwrap()
}

fn responses_function_call(body: &Value) -> &Value {
    body["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "function_call")
        .unwrap()
}

fn renamed_input() -> Value {
    json!({"not_labels_": "bug", "filter": {"author_username": "zak", "state": "opened"}, "pages": [{"page_size_": 5}], "note": "not_labels_"})
}

fn restored_input() -> Value {
    json!({"not[labels]": "bug", "filter": {"author username": "zak", "state": "opened"}, "pages": [{"page[size]": 5}], "note": "not_labels_"})
}

fn parse_args(raw: &Value) -> Value {
    serde_json::from_str(raw.as_str().unwrap()).unwrap()
}

#[test]
fn responses_buffered_call_restores_client_keys() {
    let body = non_stream(
        Format::OpenAIResponse,
        &responses_request(),
        &claude_answer(renamed_input(), "list_merge_requests"),
    );
    let item = responses_function_call(&body);
    assert_eq!(item["name"], "list_merge_requests");
    assert_eq!(parse_args(&item["arguments"]), restored_input(), "{body}");
}

#[test]
fn responses_stream_restores_client_keys() {
    let events = stream(
        Format::OpenAIResponse,
        &responses_request(),
        &claude_events(&renamed_input(), "list_merge_requests"),
    );
    let deltas: String = events
        .iter()
        .filter(|e| e["type"] == "response.function_call_arguments.delta")
        .map(|e| e["delta"].as_str().unwrap())
        .collect();
    assert_eq!(
        serde_json::from_str::<Value>(&deltas).unwrap(),
        restored_input(),
        "{events:?}"
    );
    let done = events
        .iter()
        .find(|e| e["type"] == "response.function_call_arguments.done")
        .unwrap();
    assert_eq!(parse_args(&done["arguments"]), restored_input());
    let item = events
        .iter()
        .find(|e| e["type"] == "response.output_item.done")
        .unwrap();
    assert_eq!(parse_args(&item["item"]["arguments"]), restored_input());
    let completed = events.iter().find(|e| e["type"] == "response.completed").unwrap();
    assert_eq!(
        parse_args(&responses_function_call(&completed["response"])["arguments"]),
        restored_input()
    );
}

#[test]
fn responses_stream_keeps_incremental_deltas_for_accepted_tools() {
    let input = json!({"q": "abc"});
    let events = stream(
        Format::OpenAIResponse,
        &responses_request(),
        &claude_events(&input, "plain"),
    );
    let deltas: Vec<&str> = events
        .iter()
        .filter(|e| e["type"] == "response.function_call_arguments.delta")
        .map(|e| e["delta"].as_str().unwrap())
        .collect();
    assert_eq!(deltas.len(), 3, "{deltas:?}");
    assert_eq!(deltas.concat(), input.to_string());
}

fn chat_call(chunks: &[Value]) -> (String, String) {
    let mut name = String::new();
    let mut args = String::new();
    for chunk in chunks {
        for call in chunk["choices"][0]["delta"]["tool_calls"]
            .as_array()
            .into_iter()
            .flatten()
        {
            name += call["function"]["name"].as_str().unwrap_or_default();
            args += call["function"]["arguments"].as_str().unwrap_or_default();
        }
    }
    (name, args)
}

#[test]
fn chat_stream_and_buffered_calls_restore_client_keys() {
    let chunks = stream(
        Format::OpenAI,
        &chat_request(),
        &claude_events(&renamed_input(), "list_merge_requests"),
    );
    let (name, args) = chat_call(&chunks);
    assert_eq!(name, "list_merge_requests");
    assert_eq!(serde_json::from_str::<Value>(&args).unwrap(), restored_input());

    let body = non_stream(
        Format::OpenAI,
        &chat_request(),
        &claude_answer(renamed_input(), "list_merge_requests"),
    );
    let call = &body["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(parse_args(&call["function"]["arguments"]), restored_input(), "{body}");
}

/// Codex declares MCP tools as namespace children and replays their calls with the
/// namespace.
#[test]
fn responses_namespaced_mcp_tool_round_trips() {
    let request = json!({
        "model": "claude-opus-5-5",
        "tools": [{"type": "namespace", "name": "mcp__gitlab__", "tools": [
            {"type": "function", "name": "list_merge_requests", "parameters": schema()}
        ]}],
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
            {"type": "function_call", "call_id": "call_1", "namespace": "mcp__gitlab__", "name": "list_merge_requests",
             "arguments": "{\"not[labels]\":\"bug\"}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "[]"}
        ]
    })
    .to_string()
    .into_bytes();
    let out = translate(Format::OpenAIResponse, &request);
    let tool = &out["tools"][0];
    let claude_name = tool["name"].as_str().unwrap().to_owned();
    assert_eq!(tool["input_schema"]["required"], json!(["not_labels_"]), "{tool}");
    assert_eq!(tool_use_input(&out), &json!({"not_labels_": "bug"}));

    let body = non_stream(
        Format::OpenAIResponse,
        &request,
        &claude_answer(renamed_input(), &claude_name),
    );
    let item = responses_function_call(&body);
    assert_eq!(item["namespace"], "mcp__gitlab__", "{item}");
    assert_eq!(item["name"], "list_merge_requests");
    assert_eq!(parse_args(&item["arguments"]), restored_input());
}

/// Valid arguments with leading whitespace are restored on every path.
#[test]
fn whitespace_before_arguments_does_not_skip_restoration() {
    let text = format!(" \n{}", renamed_input());
    let events = claude_text_events(&text, "list_merge_requests");
    let body = sse(&events).concat();

    let answer = non_stream(Format::OpenAIResponse, &responses_request(), &body);
    assert_eq!(
        parse_args(&responses_function_call(&answer)["arguments"]),
        restored_input()
    );
    let streamed = stream(Format::OpenAIResponse, &responses_request(), &events);
    let done = streamed
        .iter()
        .find(|e| e["type"] == "response.function_call_arguments.done")
        .unwrap();
    assert_eq!(parse_args(&done["arguments"]), restored_input());

    let answer = non_stream(Format::OpenAI, &chat_request(), &body);
    let call = &answer["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(parse_args(&call["function"]["arguments"]), restored_input());
    let (_, args) = chat_call(&stream(Format::OpenAI, &chat_request(), &events));
    assert_eq!(serde_json::from_str::<Value>(&args).unwrap(), restored_input());
}
