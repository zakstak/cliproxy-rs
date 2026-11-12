//! Anthropic server-side fallback on the Chat Completions and Responses routes (#71).
//!
//! Event shapes follow anthropic-sdk-python 5139cf2e: the recorded stream
//! `tests/lib/streaming/fixtures/fallback_response.txt` (a `fallback` block with `from`,
//! `to` and `trigger` between the declining and the serving model) and
//! `types/beta/beta_fallback_message_iteration_usage.py` (a `fallback_message` entry in
//! `usage.iterations` names the serving model). Streamed and buffered answers must
//! agree on the serving model and the ordered hops.
use cpa_core::format::Format;
use cpa_translate::{ResponseCtx, pair};
use serde_json::{Value, json};

const REQUESTED: &str = "claude-opus-4-7";

fn ctx(request: &[u8]) -> ResponseCtx<'_> {
    ResponseCtx {
        model: REQUESTED,
        original_request: request,
        translated_request: b"{}",
        tool_search: false,
    }
}

fn request() -> Vec<u8> {
    json!({"model": REQUESTED, "input": "hi"}).to_string().into_bytes()
}

fn start(model: &str) -> Value {
    json!({"type":"message_start","message":{"id":"msg_fb","type":"message","role":"assistant","content":[],"model":model,"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":25,"output_tokens":1}}})
}

fn hop(index: i64, from: &str, to: &str) -> [Value; 2] {
    [
        json!({"type":"content_block_start","index":index,"content_block":{"type":"fallback","from":{"model":from},"to":{"model":to},"trigger":{"type":"refusal","category":null}}}),
        json!({"type":"content_block_stop","index":index}),
    ]
}

fn text(index: i64, t: &str) -> [Value; 3] {
    [
        json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":t}}),
        json!({"type":"content_block_stop","index":index}),
    ]
}

fn end(usage: Value) -> [Value; 2] {
    [
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":usage}),
        json!({"type":"message_stop"}),
    ]
}

/// The SDK's recorded single-hop stream.
fn single_hop() -> Vec<Value> {
    let mut e = vec![start(REQUESTED)];
    e.extend(hop(0, REQUESTED, "claude-sonnet-4-5"));
    e.extend(text(1, "Hello there!"));
    e.extend(end(json!({"output_tokens":8})));
    e
}

fn sse(events: &[Value]) -> Vec<Vec<u8>> {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()).into_bytes())
        .collect()
}

fn stream(client: Format, events: &[Value]) -> Vec<(String, Value)> {
    let request = request();
    let mut s = (pair(client, Format::Claude).unwrap().stream)(&ctx(&request));
    let mut chunks = vec![];
    for event in sse(events) {
        chunks.extend(s.event(&event).unwrap());
    }
    chunks.extend(s.finish().unwrap());
    let mut out = vec![];
    for chunk in chunks {
        let mut name = String::new();
        for line in String::from_utf8(chunk.to_vec()).unwrap().lines() {
            if let Some(n) = line.strip_prefix("event: ") {
                name = n.to_owned();
            } else if let Some(d) = line.strip_prefix("data: ")
                && d != "[DONE]"
            {
                out.push((name.clone(), serde_json::from_str(d).unwrap()));
            }
        }
    }
    out
}

fn buffered(client: Format, body: &[u8]) -> Value {
    let request = request();
    serde_json::from_slice(&(pair(client, Format::Claude).unwrap().non_stream)(&ctx(&request), body).unwrap()).unwrap()
}

fn buffered_sse(client: Format, events: &[Value]) -> Value {
    buffered(client, &sse(events).concat())
}

fn hop_json(from: &str, to: &str) -> Value {
    json!({"from":{"model":from},"to":{"model":to},"trigger":{"type":"refusal","category":null}})
}

fn with(mut v: Value, key: &str, n: i64) -> Value {
    v[key] = json!(n);
    v
}

#[test]
fn single_hop_reports_the_serving_model_and_the_hop() {
    let events = single_hop();
    // Chat stream: chunks before the hop keep the requested model, the hop chunk
    // carries it, and later chunks name the serving model.
    let chunks = stream(Format::OpenAI, &events);
    let models: Vec<&str> = chunks.iter().map(|(_, c)| c["model"].as_str().unwrap()).collect();
    assert_eq!(models.first(), Some(&REQUESTED));
    assert_eq!(models.last(), Some(&"claude-sonnet-4-5"));
    let hops: Vec<&Value> = chunks
        .iter()
        .filter_map(|(_, c)| c["choices"][0]["delta"].get("fallback"))
        .collect();
    assert_eq!(hops, [&hop_json(REQUESTED, "claude-sonnet-4-5")]);
    let at = chunks
        .iter()
        .position(|(_, c)| c["choices"][0]["delta"].get("fallback").is_some())
        .unwrap();
    assert_eq!(chunks[at].1["model"], "claude-sonnet-4-5");
    assert_eq!(
        chunks[at].1["choices"][0]["delta"]["fallbacks"],
        json!([hop_json(REQUESTED, "claude-sonnet-4-5")])
    );
    let text: String = chunks
        .iter()
        .filter_map(|(_, c)| c["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(text, "Hello there!");

    // Chat buffered.
    let body = buffered_sse(Format::OpenAI, &events);
    assert_eq!(body["model"], "claude-sonnet-4-5");
    let want = with(hop_json(REQUESTED, "claude-sonnet-4-5"), "content_offset", 0);
    assert_eq!(body["choices"][0]["message"]["fallback"], want);
    assert_eq!(body["choices"][0]["message"]["fallbacks"], json!([want]));
    assert_eq!(body["choices"][0]["message"]["content"], "Hello there!");

    // Responses stream: a response.in_progress announces the hop before the output
    // that follows it, and the terminal response names the serving model.
    let out = stream(Format::OpenAIResponse, &events);
    let progress: Vec<&Value> = out
        .iter()
        .filter(|(n, e)| n == "response.in_progress" && e["response"].get("fallback").is_some())
        .map(|(_, e)| e)
        .collect();
    assert_eq!(progress.len(), 1, "{out:?}");
    let want = with(hop_json(REQUESTED, "claude-sonnet-4-5"), "output_index", 0);
    assert_eq!(progress[0]["response"]["model"], "claude-sonnet-4-5");
    assert_eq!(progress[0]["response"]["fallback"], want);
    let hop_at = out
        .iter()
        .position(|(n, e)| n == "response.in_progress" && e["response"].get("fallback").is_some())
        .unwrap();
    let text_at = out.iter().position(|(n, _)| n == "response.output_text.delta").unwrap();
    assert!(hop_at < text_at, "the hop precedes the serving model's output");
    let (name, terminal) = out.last().unwrap();
    assert_eq!(name, "response.completed");
    assert_eq!(terminal["response"]["model"], "claude-sonnet-4-5");
    assert_eq!(terminal["response"]["fallbacks"], json!([want]));
    // Responses buffered.
    let body = buffered_sse(Format::OpenAIResponse, &events);
    assert_eq!(body["model"], "claude-sonnet-4-5");
    assert_eq!(body["fallback"], want);
    assert_eq!(body["fallbacks"], json!([want]));
}

/// Each hop keeps its order and its boundary: the content offset (UTF-8 bytes) for
/// Chat, the output index for Responses.
#[test]
fn multi_hop_keeps_order_and_boundaries() {
    let mut events = vec![start("claude-opus-5-5")];
    events.extend(text(0, "héllo "));
    events.extend(hop(1, "claude-opus-5-5", "claude-opus-4-8"));
    events.extend(text(2, "more "));
    events.extend(hop(3, "claude-opus-4-8", "claude-sonnet-4-6"));
    events.extend(text(4, "done"));
    events.extend(end(json!({"output_tokens":9})));
    let first = hop_json("claude-opus-5-5", "claude-opus-4-8");
    let second = hop_json("claude-opus-4-8", "claude-sonnet-4-6");

    let body = buffered_sse(Format::OpenAI, &events);
    assert_eq!(body["model"], "claude-sonnet-4-6");
    assert_eq!(
        body["choices"][0]["message"]["fallbacks"],
        json!([
            with(first.clone(), "content_offset", 7),
            with(second.clone(), "content_offset", 12)
        ])
    );
    assert_eq!(
        body["choices"][0]["message"]["fallback"],
        with(second.clone(), "content_offset", 12)
    );

    let chunks = stream(Format::OpenAI, &events);
    let last_hop = chunks
        .iter()
        .rev()
        .find_map(|(_, c)| c["choices"][0]["delta"].get("fallbacks"))
        .unwrap();
    assert_eq!(last_hop, &json!([first.clone(), second.clone()]));

    let body = buffered_sse(Format::OpenAIResponse, &events);
    assert_eq!(body["model"], "claude-sonnet-4-6");
    assert_eq!(
        body["fallbacks"],
        json!([
            with(first.clone(), "output_index", 1),
            with(second.clone(), "output_index", 2)
        ])
    );
    let out = stream(Format::OpenAIResponse, &events);
    let terminal = &out.last().unwrap().1["response"];
    assert_eq!(terminal["model"], "claude-sonnet-4-6");
    assert_eq!(terminal["fallbacks"], body["fallbacks"]);
}

/// A turn a fallback model served from the start carries no block, only a
/// `fallback_message` iteration; that names the serving model and the hop.
#[test]
fn fallback_message_iteration_without_block_names_the_serving_model() {
    let iterations = json!({"output_tokens":4,"iterations":[
        {"type":"message","model":REQUESTED,"input_tokens":25,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0},
        {"type":"fallback_message","model":"claude-opus-4-8","input_tokens":25,"output_tokens":4,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}
    ]});
    let mut events = vec![start(REQUESTED)];
    events.extend(text(0, "ok"));
    events.extend(end(iterations));
    let want = json!({"from":{"model":REQUESTED},"to":{"model":"claude-opus-4-8"}});

    let body = buffered_sse(Format::OpenAI, &events);
    assert_eq!(body["model"], "claude-opus-4-8");
    assert_eq!(
        body["choices"][0]["message"]["fallback"],
        with(want.clone(), "content_offset", 0)
    );
    let chunks = stream(Format::OpenAI, &events);
    assert_eq!(
        chunks.last().unwrap().1["model"],
        "claude-opus-4-8",
        "trailing usage chunk"
    );
    let carrying = chunks
        .iter()
        .find(|(_, c)| c["choices"][0]["delta"].get("fallback").is_some())
        .unwrap();
    assert_eq!(carrying.1["choices"][0]["delta"]["fallback"], want);
    assert_eq!(carrying.1["usage"]["completion_tokens"], 4, "on the usage chunk");

    let body = buffered_sse(Format::OpenAIResponse, &events);
    assert_eq!(body["model"], "claude-opus-4-8");
    assert_eq!(body["fallbacks"], json!([want.clone()]));
    let out = stream(Format::OpenAIResponse, &events);
    let terminal = &out.last().unwrap().1["response"];
    assert_eq!(terminal["model"], "claude-opus-4-8");
    assert_eq!(terminal["fallback"], want);
}

/// With a block, the iterations name the serving model but add no second hop.
#[test]
fn block_and_iteration_report_one_hop() {
    let iterations = json!({"output_tokens":8,"iterations":[
        {"type":"message","model":REQUESTED,"input_tokens":25,"output_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0},
        {"type":"fallback_message","model":"claude-sonnet-4-5-20250929","input_tokens":25,"output_tokens":7,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}
    ]});
    let mut events = vec![start(REQUESTED)];
    events.extend(hop(0, REQUESTED, "claude-sonnet-4-5"));
    events.extend(text(1, "Hello"));
    events.extend(end(iterations));
    for client in [Format::OpenAI, Format::OpenAIResponse] {
        let body = buffered_sse(client, &events);
        assert_eq!(body["model"], "claude-sonnet-4-5-20250929", "{client:?}");
        let hops = if client == Format::OpenAI {
            &body["choices"][0]["message"]["fallbacks"]
        } else {
            &body["fallbacks"]
        };
        assert_eq!(hops.as_array().unwrap().len(), 1, "{client:?}: {body}");
    }
}

/// Without a documented fallback signal nothing changes, even when the upstream names
/// a dated snapshot or another model than the one requested: Go's message_start
/// inference is not ported.
#[test]
fn no_signal_no_fallback_fields() {
    for upstream_model in [REQUESTED, "claude-opus-4-7-20260101", "claude-haiku-4-5"] {
        let mut events = vec![start(upstream_model)];
        events.extend(text(0, "plain"));
        events.extend(end(json!({"output_tokens":1})));
        let chunks = stream(Format::OpenAI, &events);
        assert!(chunks.iter().all(|(_, c)| c["model"] == REQUESTED), "{chunks:?}");
        assert!(
            chunks
                .iter()
                .all(|(_, c)| c["choices"][0]["delta"].get("fallback").is_none())
        );
        let body = buffered_sse(Format::OpenAI, &events);
        assert_eq!(
            body["model"], upstream_model,
            "buffered Chat keeps the upstream's model"
        );
        assert!(body["choices"][0]["message"].get("fallback").is_none());
        let out = stream(Format::OpenAIResponse, &events);
        let terminal = &out.last().unwrap().1["response"];
        assert!(terminal.get("fallback").is_none() && terminal.get("fallbacks").is_none());
        assert!(out.iter().filter(|(n, _)| n == "response.in_progress").count() == 1);
        let body = buffered_sse(Format::OpenAIResponse, &events);
        assert!(body.get("fallback").is_none());
    }
}

/// A buffered native message: Anthropic relabels its `model` to the serving one and
/// keeps the block in `content`; the translation agrees with the stream.
#[test]
fn native_message_body_agrees_with_the_stream() {
    let native = json!({"id":"msg_n","type":"message","role":"assistant","model":"claude-sonnet-4-5",
        "content":[
            {"type":"fallback","from":{"model":REQUESTED},"to":{"model":"claude-sonnet-4-5"},"trigger":{"type":"refusal","category":"cyber","extra":1}},
            {"type":"text","text":"Hello there!"}
        ],
        "stop_reason":"end_turn","stop_sequence":null,
        "usage":{"input_tokens":25,"output_tokens":8,"iterations":[
            {"type":"message","model":REQUESTED,"input_tokens":25,"output_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0},
            {"type":"fallback_message","model":"claude-sonnet-4-5","input_tokens":25,"output_tokens":7,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}]}});
    let trigger = json!({"type":"refusal","category":"cyber","extra":1});
    let body = buffered(Format::OpenAI, native.to_string().as_bytes());
    assert_eq!(body["model"], "claude-sonnet-4-5");
    assert_eq!(
        body["choices"][0]["message"]["fallback"]["trigger"], trigger,
        "unknown fields kept"
    );
    assert_eq!(body["choices"][0]["message"]["fallbacks"].as_array().unwrap().len(), 1);
    let body = buffered(Format::OpenAIResponse, native.to_string().as_bytes());
    assert_eq!(body["model"], "claude-sonnet-4-5");
    assert_eq!(body["fallback"]["trigger"], trigger);
    assert_eq!(body["fallback"]["output_index"], 0);
}

fn it(kind: &str, model: &str) -> Value {
    json!({"type":kind,"model":model,"input_tokens":1,"output_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0})
}

/// A message without fallback blocks whose usage arrives in one `message_delta` per
/// `snapshots` entry, each carrying those iterations.
fn iteration_events(snapshots: &[Value]) -> Vec<Value> {
    let mut e = vec![start(REQUESTED)];
    e.extend(text(0, "ok"));
    for (i, iterations) in snapshots.iter().enumerate() {
        let mut delta = json!({"type":"message_delta","delta":{"stop_reason":null},"usage":{"output_tokens":2,"iterations":iterations}});
        if i + 1 == snapshots.len() {
            delta["delta"]["stop_reason"] = json!("end_turn");
        }
        e.push(delta);
    }
    e.push(json!({"type":"message_stop"}));
    e
}

/// Hops as (from, to).
type Hops = Vec<(String, String)>;

/// (path, serving model, hops) on each of the four translated paths.
fn reported(events: &[Value]) -> Vec<(String, String, Hops)> {
    let pairs = |hops: &Value| -> Hops {
        hops.as_array()
            .map(|a| a.iter().map(hop_models).collect())
            .unwrap_or_default()
    };
    let mut out = vec![];
    let body = buffered_sse(Format::OpenAI, events);
    out.push((
        "chat buffered".to_owned(),
        body["model"].as_str().unwrap().to_owned(),
        pairs(&body["choices"][0]["message"]["fallbacks"]),
    ));
    let chunks = stream(Format::OpenAI, events);
    let last_hops = chunks
        .iter()
        .rev()
        .find_map(|(_, c)| c["choices"][0]["delta"].get("fallbacks").cloned())
        .unwrap_or(Value::Null);
    out.push((
        "chat stream".to_owned(),
        chunks.last().unwrap().1["model"].as_str().unwrap().to_owned(),
        pairs(&last_hops),
    ));
    let body = buffered_sse(Format::OpenAIResponse, events);
    out.push((
        "responses buffered".to_owned(),
        body["model"].as_str().unwrap().to_owned(),
        pairs(&body["fallbacks"]),
    ));
    let terminal = stream(Format::OpenAIResponse, events).last().unwrap().1["response"].clone();
    out.push((
        "responses stream".to_owned(),
        terminal["model"].as_str().unwrap().to_owned(),
        pairs(&terminal["fallbacks"]),
    ));
    out
}

fn hop_models(hop: &Value) -> (String, String) {
    (
        hop["from"]["model"].as_str().unwrap().to_owned(),
        hop["to"]["model"].as_str().unwrap().to_owned(),
    )
}

fn hops(list: &[(&str, &str)]) -> Hops {
    list.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
}

/// Without blocks, the hops run from the requested model through each other model that
/// ran to the serving one. The serving model's earlier tool-loop iterations, repeated
/// iterations of one model and the name the requested model ran under are not hops.
#[test]
fn iterations_without_blocks_keep_the_requested_origin() {
    let served = "claude-opus-4-8";
    for (label, iterations, want) in [
        (
            "sticky with tool loop",
            json!([
                it("message", served),
                it("message", served),
                it("fallback_message", served)
            ]),
            hops(&[(REQUESTED, served)]),
        ),
        (
            "requested ran as a snapshot, then the serving tool loop",
            json!([
                it("message", "claude-opus-4-7-20260101"),
                it("message", served),
                it("fallback_message", served)
            ]),
            hops(&[(REQUESTED, served)]),
        ),
        (
            "requested ran under another name",
            json!([it("message", "claude-opus-4-7-latest"), it("fallback_message", served)]),
            hops(&[(REQUESTED, served)]),
        ),
        (
            "two transitions with repeats",
            json!([
                it("message", REQUESTED),
                it("message", REQUESTED),
                it("message", "claude-opus-5"),
                it("message", "claude-opus-5-20260301"),
                it("message", "claude-sonnet-4-6"),
                it("fallback_message", "claude-sonnet-4-6")
            ]),
            hops(&[(REQUESTED, "claude-opus-5"), ("claude-opus-5", "claude-sonnet-4-6")]),
        ),
    ] {
        let want_model = want.last().unwrap().1.clone();
        for (path, model, got) in reported(&iteration_events(std::slice::from_ref(&iterations))) {
            assert_eq!(model, want_model, "{label}: {path}");
            assert_eq!(got, want, "{label}: {path}");
        }
    }
}

/// Repeated usage snapshots neither duplicate nor reverse the hops: a longer one
/// extends them, the same or a shorter one changes nothing.
#[test]
fn repeated_iteration_snapshots_do_not_duplicate_or_reverse_hops() {
    let one = json!([it("message", REQUESTED), it("fallback_message", "claude-opus-5")]);
    let two = json!([
        it("message", REQUESTED),
        it("message", "claude-opus-5"),
        it("fallback_message", "claude-sonnet-4-6")
    ]);
    let both = hops(&[(REQUESTED, "claude-opus-5"), ("claude-opus-5", "claude-sonnet-4-6")]);
    for (label, snapshots, want) in [
        (
            "same twice",
            vec![one.clone(), one.clone()],
            hops(&[(REQUESTED, "claude-opus-5")]),
        ),
        ("growing", vec![one.clone(), two.clone()], both.clone()),
        ("shrinking", vec![two.clone(), one.clone()], both.clone()),
    ] {
        let events = iteration_events(&snapshots);
        for (path, _, got) in reported(&events) {
            assert_eq!(got, want, "{label}: {path}");
        }
        // The stream announces each new hop set once.
        let announced = stream(Format::OpenAI, &events)
            .iter()
            .filter(|(_, c)| c["choices"][0]["delta"].get("fallback").is_some())
            .count();
        assert_eq!(
            announced,
            if label == "same twice" {
                1
            } else {
                1 + usize::from(label == "growing")
            }
        );
    }
}

/// A block stays authoritative over iterations that would imply other hops.
#[test]
fn blocks_override_iteration_hops() {
    let mut events = vec![start(REQUESTED)];
    events.extend(hop(0, REQUESTED, "claude-opus-4-8"));
    events.extend(text(1, "ok"));
    events.extend(end(json!({"output_tokens":2,"iterations":[
        it("message", REQUESTED), it("message", "claude-opus-5"), it("message", "claude-opus-4-8"), it("fallback_message", "claude-opus-4-8")
    ]})));
    for (path, model, got) in reported(&events) {
        assert_eq!(model, "claude-opus-4-8", "{path}");
        assert_eq!(got, hops(&[(REQUESTED, "claude-opus-4-8")]), "{path}");
    }
}
