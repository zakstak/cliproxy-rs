//! #72 memory bound: a chain of paused `web_search_call` items with one ID and a single
//! large query must not copy or retain that query once per item. A counting allocator
//! measures the peak live heap of one request translation; the bound is a multiple of
//! the request size, independent of the chain length. Its own test binary keeps other
//! tests' allocations out of the count.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use cpa_core::format::Format;
use cpa_translate::{RequestCtx, translate_request};
use serde_json::{Value, json};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        unsafe { System.dealloc(p, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, layout, new_size) };
        if !q.is_null() {
            if new_size >= layout.size() {
                let live = LIVE.fetch_add(new_size - layout.size(), Ordering::Relaxed) + new_size - layout.size();
                PEAK.fetch_max(live, Ordering::Relaxed);
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

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

/// Peak live heap above the level before the call, and the translated request.
fn translate_peak(body: &[u8]) -> (usize, Vec<u8>) {
    let ctx = RequestCtx {
        model: "claude-test",
        stream: true,
        tool_search: false,
    };
    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let out = translate_request(Format::OpenAIResponse, Format::Claude, &ctx, body).unwrap();
    (PEAK.load(Ordering::Relaxed) - before, out)
}

#[test]
fn a_repeated_paused_search_does_not_multiply_its_query() {
    // 256 KiB query, 128 continuations: copying the query per continuation, as the
    // reviewed head did, retains at least 32 MiB; the bound below is ~3 MiB.
    let query = "q".repeat(256 * 1024);
    let body = chain(&query, 128);
    let (peak, out) = translate_peak(&body);
    let out: Value = serde_json::from_slice(&out).unwrap();
    let blocks = out["messages"][1]["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 1, "one server_tool_use for the chain");
    assert_eq!(blocks[0]["type"], "server_tool_use");
    assert_eq!(blocks[0]["input"]["query"].as_str().unwrap().len(), query.len());
    let bound = 12 * body.len();
    eprintln!("body {} bytes, peak live heap {peak} bytes, bound {bound}", body.len());
    assert!(
        peak < bound,
        "peak live heap {peak} bytes for a {}-byte body",
        body.len()
    );

    // The peak does not grow with the chain: 8x the continuations stays within the same
    // bound as the short chain plus the extra items' own size.
    let short = chain(&query, 16);
    let (short_peak, _) = translate_peak(&short);
    let (long_peak, _) = translate_peak(&chain(&query, 128));
    let growth = long_peak.saturating_sub(short_peak);
    assert!(
        growth < 16 * (body.len() - short.len()),
        "peak grew by {growth} bytes for {} more bytes of continuations",
        body.len() - short.len()
    );
}
