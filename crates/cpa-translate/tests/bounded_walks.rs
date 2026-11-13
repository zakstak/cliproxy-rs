//! Issue #146: translators walk nested JSON on the heap, to at most
//! `cpa_common::json::WALK_DEPTH_LIMIT` levels, on the caller's thread.
//!
//! Every registered pair translates requests, buffered responses and streams that nest
//! deeply, both structurally and as JSON text inside strings, on one thread with a small
//! fixed stack. The same stack serves a shallow and a 20,000-level input, so the native
//! stack a translation needs does not grow with the input's depth. Long streams of code
//! text, whose braces made the removed depth estimate start an OS thread per event, run
//! on that thread too.

use cpa_common::json as gj;
use cpa_core::format::Format;
use cpa_translate::{RequestCtx, ResponseCtx, pair};
use serde_json::json;

/// Small next to a 2 MiB async worker, and fixed for every depth below.
const STACK: usize = 1 << 20;

fn on_small_stack(f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap();
}

/// `{"a":{"a":...1...}}`, `depth` objects deep.
fn deep_object(depth: usize) -> String {
    format!("{}1{}", r#"{"a":"#.repeat(depth), "}".repeat(depth))
}

/// `[[...]]`, `depth` arrays deep.
fn deep_array(depth: usize) -> String {
    format!("{}{}", "[".repeat(depth), "]".repeat(depth))
}

/// A JSON Schema whose object properties nest `depth` levels, with dialect keywords and
/// `true` subschemas at every level for the schema normalizers.
fn deep_schema(depth: usize) -> String {
    let open =
        r#"{"type":"object","$schema":"x","additionalProperties":true,"items":true,"required":[],"properties":{"p":"#;
    format!("{}{}{}", open.repeat(depth), r#"{"type":"string"}"#, "}}".repeat(depth))
}

/// Responses namespaces nested `depth` levels, an apply_patch custom tool innermost.
fn deep_namespaces(depth: usize) -> String {
    let open = r#"{"type":"namespace","name":"ns","tools":["#;
    format!(
        "{}{}{}",
        open.repeat(depth),
        r#"{"type":"custom","name":"apply_patch","format":{"type":"grammar"}}"#,
        "]}".repeat(depth)
    )
}

/// A tool choice whose `tools` nest `depth` levels.
fn deep_choice(depth: usize) -> String {
    let open = r#"{"type":"allowed_tools","tools":["#;
    format!(
        "{}{}{}",
        open.repeat(depth),
        r#"{"type":"custom","name":"apply_patch"}"#,
        "]}".repeat(depth)
    )
}

/// Reasoning details nested in arrays `depth` levels, text innermost.
fn deep_reasoning(depth: usize) -> String {
    format!("{}{}{}", "[".repeat(depth), r#"{"text":"why"}"#, "]".repeat(depth))
}

fn requests(depth: usize) -> Vec<(Format, String)> {
    let object = deep_object(depth);
    let array = deep_array(depth);
    let schema = deep_schema(depth);
    // The same JSON as text: tool arguments and results a translator may parse.
    let object_text = json!(object).to_string();
    let array_text = json!(array).to_string();
    let claude = format!(
        r#"{{"model":"m","max_tokens":8,"tools":[{{"name":"t","input_schema":{schema}}},{{"name":"u","input_schema":{{"type":"object","properties":{{"x":{array}}}}}}}],"messages":[{{"role":"user","content":"hi"}},{{"role":"assistant","content":[{{"type":"tool_use","id":"toolu_1","name":"t","input":{{"x":{object}}}}}]}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_1","content":{object_text}}},{{"type":"tool_result","tool_use_id":"toolu_1","content":[{{"type":"text","text":{array_text}}}]}}]}}]}}"#
    );
    let openai = format!(
        r#"{{"model":"m","tools":[{{"type":"function","function":{{"name":"t","parameters":{schema}}}}}],"response_format":{{"type":"json_schema","json_schema":{{"name":"s","schema":{schema}}}}},"messages":[{{"role":"user","content":"hi"}},{{"role":"assistant","content":null,"reasoning_details":{reasoning},"tool_calls":[{{"id":"call_1","type":"function","function":{{"name":"t","arguments":{object_text}}}}}]}},{{"role":"tool","tool_call_id":"call_1","content":{array_text}}}]}}"#,
        reasoning = deep_reasoning(depth),
    );
    let responses = format!(
        r#"{{"model":"m","tools":[{namespaces},{{"type":"function","name":"t","parameters":{schema}}}],"tool_choice":{choice},"text":{{"format":{{"type":"json_schema","name":"s","schema":{schema}}}}},"input":[{{"type":"message","role":"user","content":"hi"}},{{"type":"function_call","call_id":"call_1","name":"t","arguments":{object_text}}},{{"type":"function_call_output","call_id":"call_1","output":{array_text}}},{{"type":"additional_tools","tools":[{namespaces}]}}]}}"#,
        namespaces = deep_namespaces(depth),
        choice = deep_choice(depth),
    );
    vec![
        (Format::Claude, claude),
        (Format::OpenAI, openai),
        (Format::OpenAIResponse, responses.clone()),
        (Format::Codex, responses),
    ]
}

/// Buffered upstream responses, by upstream format.
fn responses(depth: usize) -> Vec<(Format, String)> {
    let object = deep_object(depth);
    let object_text = json!(object).to_string();
    let reasoning = deep_reasoning(depth);
    let claude = format!(
        r#"{{"id":"msg_1","type":"message","role":"assistant","model":"m","content":[{{"type":"text","text":"x"}},{{"type":"tool_use","id":"toolu_1","name":"t","input":{{"x":{object}}}}}],"stop_reason":"tool_use","usage":{{"input_tokens":1,"output_tokens":1}}}}"#
    );
    let openai = format!(
        r#"{{"id":"c1","object":"chat.completion","model":"m","choices":[{{"index":0,"message":{{"role":"assistant","content":"x","reasoning_details":{reasoning},"tool_calls":[{{"id":"call_1","type":"function","function":{{"name":"t","arguments":{object_text}}}}}]}},"finish_reason":"tool_calls"}}],"usage":{{"prompt_tokens":1,"completion_tokens":1}}}}"#
    );
    let responses = format!(
        r#"{{"type":"response.completed","response":{{"id":"r1","object":"response","model":"m","status":"completed","output":[{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"x"}}]}},{{"type":"function_call","call_id":"call_1","name":"t","arguments":{object_text}}},{{"type":"custom_tool_call","call_id":"call_2","name":"apply_patch","input":{object_text}}}],"usage":{{"input_tokens":1,"output_tokens":1}}}}}}"#
    );
    vec![
        (Format::Claude, claude),
        (Format::OpenAI, openai),
        (Format::OpenAIResponse, responses.clone()),
        (Format::Codex, responses),
    ]
}

/// Upstream stream events, by upstream format: deep tool input sent in pieces and code
/// text with many braces.
fn stream_events(upstream: Format, depth: usize, code_events: usize) -> Vec<Vec<u8>> {
    let ev = |v: serde_json::Value| format!("data: {v}\n\n").into_bytes();
    let object = deep_object(depth);
    let pieces: Vec<String> = object
        .as_bytes()
        .chunks(4_096)
        .map(|c| String::from_utf8(c.to_vec()).unwrap())
        .collect();
    let code = "fn sample() { if x { [y] } } ".repeat(10);
    let mut out = vec![];
    match upstream {
        Format::Claude => {
            out.push(ev(
                json!({"type":"message_start","message":{"id":"m1","model":"m","usage":{"input_tokens":1}}}),
            ));
            out.push(ev(
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            ));
            for _ in 0..code_events {
                out.push(ev(
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":code}}),
                ));
            }
            out.push(ev(json!({"type":"content_block_stop","index":0})));
            out.push(ev(json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"t","input":{}}})));
            for piece in &pieces {
                out.push(ev(json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":piece}})));
            }
            out.push(ev(json!({"type":"content_block_stop","index":1})));
            out.push(ev(
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":1}}),
            ));
            out.push(ev(json!({"type":"message_stop"})));
        }
        Format::OpenAI => {
            let chunk = |delta: serde_json::Value, finish: serde_json::Value| {
                ev(
                    json!({"id":"c1","object":"chat.completion.chunk","model":"m","choices":[{"index":0,"delta":delta,"finish_reason":finish}]}),
                )
            };
            out.push(chunk(json!({"role":"assistant"}), json!(null)));
            out.push(chunk(
                json!({"reasoning_details": serde_json::from_str::<serde_json::Value>(&deep_reasoning(100)).unwrap()}),
                json!(null),
            ));
            for _ in 0..code_events {
                out.push(chunk(json!({"content":code}), json!(null)));
            }
            out.push(chunk(json!({"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"t","arguments":""}}]}), json!(null)));
            for piece in &pieces {
                out.push(chunk(
                    json!({"tool_calls":[{"index":0,"function":{"arguments":piece}}]}),
                    json!(null),
                ));
            }
            out.push(chunk(json!({}), json!("tool_calls")));
            out.push(b"data: [DONE]\n\n".to_vec());
        }
        Format::OpenAIResponse | Format::Codex => {
            out.push(ev(json!({"type":"response.created","response":{"id":"r1","model":"m","status":"in_progress","output":[]}})));
            out.push(ev(json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1","role":"assistant","content":[]}})));
            for _ in 0..code_events {
                out.push(ev(json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"msg_1","delta":code})));
            }
            out.push(ev(json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"t","arguments":""}})));
            for piece in &pieces {
                out.push(ev(json!({"type":"response.function_call_arguments.delta","output_index":1,"item_id":"fc_1","delta":piece})));
            }
            out.push(ev(json!({"type":"response.function_call_arguments.done","output_index":1,"item_id":"fc_1","arguments":object})));
            out.push(ev(json!({"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"t","arguments":object}})));
            out.push(ev(json!({"type":"response.completed","response":{"id":"r1","model":"m","status":"completed","output":[{"type":"function_call","call_id":"call_1","name":"t","arguments":object}],"usage":{"input_tokens":1,"output_tokens":1}}})));
        }
    }
    out
}

fn pairs() -> Vec<(Format, Format)> {
    Format::ALL
        .iter()
        .flat_map(|&c| Format::ALL.iter().map(move |&u| (c, u)))
        .filter(|&(c, u)| pair(c, u).is_some())
        .collect()
}

/// Translates everything at `depth` through every pair. Returns each transform's
/// outcome: whether it succeeded, and for streams how many events failed and how many
/// frames came out. Successful request outputs must be JSON.
fn translate_all(depth: usize, code_events: usize) -> Vec<(String, bool, usize, bool)> {
    let mut outcomes = vec![];
    let requests = requests(depth);
    let responses = responses(depth);
    for (client, upstream) in pairs() {
        let p = pair(client, upstream).unwrap();
        let body = &requests.iter().find(|(f, _)| *f == client).unwrap().1;
        for stream in [false, true] {
            let ctx = RequestCtx {
                model: "m",
                stream,
                tool_search: false,
            };
            let out = cpa_translate::translate_request(client, upstream, &ctx, body.as_bytes());
            if let Ok(out) = &out {
                assert!(
                    gj::valid(out),
                    "{client:?} -> {upstream:?} request output is not JSON at depth {depth}"
                );
            }
            outcomes.push((
                format!("{client:?}->{upstream:?} request stream={stream}"),
                out.is_ok(),
                0,
                true,
            ));
        }
        let codex_ctx = RequestCtx {
            model: "m",
            stream: true,
            tool_search: false,
        };
        if let Some(out) = cpa_translate::codex_request(client, &codex_ctx, body.as_bytes()) {
            if let Ok(out) = &out {
                assert!(gj::valid(out), "{client:?} codex request output is not JSON");
            }
            outcomes.push((format!("{client:?} codex request"), out.is_ok(), 0, true));
        }
        let rctx = ResponseCtx {
            model: "m",
            original_request: body.as_bytes(),
            translated_request: body.as_bytes(),
            tool_search: false,
        };
        let response = &responses.iter().find(|(f, _)| *f == upstream).unwrap().1;
        let out = (p.non_stream)(&rctx, response.as_bytes());
        outcomes.push((format!("{client:?}->{upstream:?} non-stream"), out.is_ok(), 0, true));
        let mut stream = (p.stream)(&rctx);
        let (mut failed, mut frames) = (0, 0);
        for event in stream_events(upstream, depth, code_events) {
            match stream.event(&event) {
                Ok(out) => frames += out.len(),
                Err(_) => failed += 1,
            }
        }
        frames += stream.finalize_tool_input().len();
        let finished = stream.finish().map(|out| frames += out.len()).is_ok();
        outcomes.push((format!("{client:?}->{upstream:?} stream"), finished, failed, frames > 0));
    }
    outcomes
}

#[test]
fn deep_and_embedded_json_translates_on_a_fixed_small_stack() {
    on_small_stack(|| {
        let shallow = translate_all(4, 2);
        let deep = translate_all(20_000, 2);
        // Depth changes no transform's outcome: the same ones succeed, no stream event
        // fails, and every stream writes frames.
        assert_eq!(shallow, deep);
        assert!(deep.len() >= 20, "pairs lost: {}", deep.len());
        for (name, ok, failed, framed) in &deep {
            assert_eq!(*failed, 0, "{name}");
            assert!(*framed, "{name}");
            // The strict Codex route refuses parts of this synthetic Chat body at any
            // depth; the comparison above covers it.
            if !name.ends_with("codex request") && (name.contains("request") || name.contains("stream")) {
                assert!(*ok, "{name}");
            }
        }
    });
}

/// The removed depth estimate counted every `{` and `[` in strings cumulatively and
/// started an OS thread for each event after 256 of them. These streams carry tens of
/// thousands of braces in code text and translate on one small fixed stack;
/// [`translators_start_no_threads`] keeps the thread workaround out.
#[test]
fn long_code_streams_translate_on_a_fixed_small_stack() {
    on_small_stack(|| {
        for (client, upstream) in pairs() {
            let p = pair(client, upstream).unwrap();
            let rctx = ResponseCtx {
                model: "m",
                original_request: br#"{"model":"m","input":"q"}"#,
                translated_request: b"{}",
                tool_search: false,
            };
            let mut stream = (p.stream)(&rctx);
            for event in stream_events(upstream, 4, 2_000) {
                stream.event(&event).unwrap();
            }
            stream.finish().unwrap();
        }
    });
}

/// No translator code starts a thread: the per-event `deep_stack` workaround is gone and
/// must not come back.
#[test]
fn translators_start_no_threads() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
    let mut checked = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "rs") {
            let text = std::fs::read_to_string(&path).unwrap();
            for needle in [
                "thread::scope",
                "thread::spawn",
                "thread::Builder",
                "spawn_scoped",
                "stack_size(",
            ] {
                assert!(!text.contains(needle), "{} uses {needle}", path.display());
            }
            checked += 1;
        }
    }
    assert!(checked > 20, "{checked}");
}

/// Below the walk limit a Claude tool schema is normalized for Chat Completions as in Go;
/// a subschema beyond it keeps the bytes the client sent (docs/DIFFERENCES-FROM-GO.md
/// "Bounded JSON walks").
#[test]
fn schema_normalization_stops_at_the_walk_limit() {
    let leaf = r#"{"type":"object","z" : 1}"#;
    let nest = |levels: usize| format!("{}{leaf}{}", r#"{"items":"#.repeat(levels), "}".repeat(levels));
    let translate = |schema: String| {
        let body = format!(
            r#"{{"model":"m","max_tokens":8,"tools":[{{"name":"t","input_schema":{schema}}}],"messages":[{{"role":"user","content":"hi"}}]}}"#
        );
        let ctx = RequestCtx {
            model: "m",
            stream: false,
            tool_search: false,
        };
        let out = cpa_translate::translate_request(Format::Claude, Format::OpenAI, &ctx, body.as_bytes()).unwrap();
        gj::get(&out, "tools.0.function.parameters").raw().to_vec()
    };
    let shallow = String::from_utf8(translate(nest(3))).unwrap();
    assert!(
        shallow.contains(r#"{"properties":{},"type":"object","z":1}"#),
        "{shallow}"
    );
    let deep = String::from_utf8(translate(nest(gj::WALK_DEPTH_LIMIT + 5))).unwrap();
    assert!(deep.contains(leaf), "deep leaf kept as sent");
    assert!(gj::valid(deep.as_bytes()));
}
