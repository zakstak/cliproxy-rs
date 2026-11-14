//! #72: a chain of paused `web_search_call` items with one ID and a single large query
//! translates into one `server_tool_use` block that carries the query once, whatever the
//! length of the chain.
use cpa_core::format::Format;
use cpa_translate::{RequestCtx, translate_request};
use serde_json::{Value, json};

/// A user turn, one paused search carrying `query`, then `repeats` paused continuations
/// of the same search without a query.
fn chain(query: &str, repeats: usize) -> Vec<u8> {
    let mut input = vec![
        json!({"role":"user","content":"search"}),
        json!({"type":"web_search_call","id":"ws_srvtoolu_big","status":"in_progress","action":{"type":"search","query":query}}),
    ];
    for _ in 0..repeats {
        input.push(
            json!({"type":"web_search_call","id":"ws_srvtoolu_big","status":"in_progress","action":{"type":"search"}}),
        );
    }
    json!({"model": "claude-test", "input": input}).to_string().into_bytes()
}

fn translate(body: &[u8]) -> Value {
    let ctx = RequestCtx {
        model: "claude-test",
        stream: true,
        tool_search: false,
    };
    let out = translate_request(Format::OpenAIResponse, Format::Claude, &ctx, body).unwrap();
    serde_json::from_slice(&out).unwrap()
}

#[test]
fn a_repeated_paused_search_keeps_one_block_with_its_query() {
    let query = "q".repeat(256 * 1024);
    let mut outputs = Vec::new();
    for repeats in [0, 16, 128] {
        let out = translate(&chain(&query, repeats));
        let blocks = out["messages"][1]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1, "one server_tool_use for a chain of {repeats}");
        assert_eq!(blocks[0]["type"], "server_tool_use");
        assert_eq!(blocks[0]["id"], "srvtoolu_big");
        assert_eq!(blocks[0]["input"]["query"].as_str().unwrap(), query);
        outputs.push(out);
    }
    // The continuations add nothing: every chain length translates to the same request.
    assert!(outputs.windows(2).all(|pair| pair[0] == pair[1]));
}
