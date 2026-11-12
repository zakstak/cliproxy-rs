//! Bytes allocated while preparing an xAI request whose tool history re-declares a
//! growing namespace (issue #86 review): each `additional_tools` item adds one child to
//! namespace `app`, so the request folds and the namespace's dispatcher lists every
//! child. The work must grow with the history, not with its square: Go emits a full
//! copy of the merged dispatcher at every occurrence.
//!
//! Its own test binary, with a single test, so its counting allocator sees only this
//! request (and the failed loopback connection after it). The upstream is a closed
//! loopback port: preparation runs fully, nothing is sent anywhere.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

use cpa_core::config::Config;
use cpa_core::exec::{Caller, ExecRequest, Operation};
use cpa_core::format::Format;
use serde_json::{Value, json};

static ALLOCATED: AtomicU64 = AtomicU64::new(0);

/// The system allocator, counting the bytes of every allocation and growth.
struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        ALLOCATED.fetch_add(size.saturating_sub(layout.size()) as u64, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn request(n: usize) -> ExecRequest {
    let history: Vec<Value> = (0..n)
        .map(|i| {
            json!({"type": "additional_tools", "tools": [{"type": "namespace", "name": "app", "tools": [
                {"type": "function", "name": format!("tool_{i}"), "description": "d".repeat(256), "parameters": {"type": "object"}}
            ]}]})
        })
        .collect();
    let mut input = history;
    input.push(json!({"role": "user", "content": "hi"}));
    ExecRequest {
        operation: Operation::Generate,
        source_format: Format::OpenAIResponse,
        response_format: Format::OpenAIResponse,
        requested_model: "grok-4.7".into(),
        model: "grok-4.7".into(),
        original_body: Default::default(),
        body: json!({"model": "grok-4.7", "input": input}).to_string().into(),
        stream: false,
        alt: None,
        session: None,
        execution_session: None,
        derived_session: None,
        resolved_model: None,
        usage: Default::default(),
        request_path: "/v1/responses".into(),
        headers: Default::default(),
        caller: Caller {
            principal: "fake-client".into(),
            source: "authorization",
            provider: String::new(),
        },
    }
}

fn credential() -> cpa_core::credential::Credential {
    // A private path; the credential is built in memory and nothing reads or writes it.
    let dir = std::env::temp_dir().join("xai-tool-history-alloc-auth");
    let mut metadata = serde_json::Map::new();
    metadata.insert("type".into(), "xai".into());
    let mut c = cpa_core::credential::Credential::from_file(&dir, &dir.join("xai.json"), metadata).unwrap();
    c.attributes.insert("api_key".into(), "sk-fake-xai".into());
    // A closed loopback port: the send fails after the request is prepared.
    c.attributes.insert("base_url".into(), "http://127.0.0.1:1/v1".into());
    c
}

/// Bytes allocated by one request of `n` history items, and its body size.
async fn measure(n: usize) -> (u64, u64) {
    let executor = cpa_exec::xai::XaiExecutor::default();
    let (cred, cfg, req) = (credential(), Config::default(), request(n));
    let input = req.body.len() as u64;
    let before = ALLOCATED.load(Ordering::Relaxed);
    let result = executor.execute(&cred, req, &cfg, false).await;
    let allocated = ALLOCATED.load(Ordering::Relaxed) - before;
    assert!(result.is_err(), "the closed port must refuse the request");
    (allocated, input)
}

#[test]
fn preparing_a_growing_tool_history_allocates_linearly() {
    // One current-thread runtime: the request runs on this thread.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // Warm process-wide state (clients, statics) before measuring.
    runtime.block_on(measure(250));
    let (small, small_input) = runtime.block_on(measure(250));
    let (large, large_input) = runtime.block_on(measure(500));
    eprintln!("n=250 input={small_input} allocated={small}; n=500 input={large_input} allocated={large}");
    // Twice the history: about twice the bytes when linear, four times when quadratic.
    assert!(
        large * 10 <= small * 27,
        "allocation grew {:.2}x for 2x the history (n=250: {small}, n=500: {large})",
        large as f64 / small as f64
    );
    // And a constant multiple of the request, not one that grows with it.
    assert!(
        large <= large_input * 250,
        "n=500 allocated {large} bytes for a {large_input}-byte request ({}x)",
        large / large_input
    );
}
