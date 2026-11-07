//! Memory gate: the heap a large Claude or Codex request costs on its way through the
//! proxy.
//!
//! A counting global allocator (this test binary only; release builds use the system
//! allocator untouched) records live bytes and allocation calls while one streamed
//! request of about 306 KB, then one of about 1.9 MB, goes from a client through the
//! router to a local fake upstream and back: `/v1/messages` through the Claude executor
//! to a fake Anthropic upstream, then `/v1/responses` through the Codex executor (an API
//! key whose base-url is the fake upstream). The Codex route is measured at about 100 KB,
//! 306 KB, 1.9 MB and 7.6 MB, without and with a model alias (the native route's one body
//! edit), and translated from Claude Messages and from Gemini with large tool schemas
//! (#29), so the ratios show how preparation scales with the body. `/v1/messages/count_tokens` with the same
//! Claude bodies is counted too: an OAuth credential counts locally with the shared
//! o200k_base encoder, which the warm-up builds. The same Claude bodies then go to a
//! Claude OAuth login through the server's request capture, from a client that is not
//! Claude Code, so they are cloaked, their tools renamed, the CLI identity applied and the
//! body signed: one at a time, then eight at once with the fake upstream holding back its
//! answers, for the heap the eight keep while they wait and the peak their preparation
//! adds. A captured request is measured only once every earlier capture is finalized, so
//! the upstream bodies they kept are released before it starts. The request body is built
//! before counting starts and handed to the router directly. The fake upstream runs on its
//! own thread and runtime, which the allocator does not count: hyper sizes its read buffers
//! from how much the previous read returned, so the upstream's allocations depend on
//! scheduling, not on the proxy. Each size is sent three times and gated on the smallest
//! peak and call count. The ceilings are the `alloc.*` lines of `bench/budgets.txt` (a
//! value without a line is reported, not gated); the measured values are printed so a
//! change can update them with a dated note. Every route runs in one test, one after the
//! other, because the counters are process-wide.

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::response::IntoResponse;
use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use cpa_server::request_logging::RequestLog;
use futures_util::StreamExt;
use tower::ServiceExt;

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static CALLS: AtomicUsize = AtomicUsize::new(0);
static COUNTING: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Set on the fake upstream's thread. `const`, so reading it never allocates.
    static UNCOUNTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn counted() -> bool {
    !UNCOUNTED.try_with(std::cell::Cell::get).unwrap_or(false)
}

// SAFETY: every call is forwarded to the system allocator unchanged; the counters are
// plain atomics and never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() && counted() {
            grow(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        if counted() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() && counted() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            grow(new_size);
        }
        p
    }
}

fn grow(size: usize) {
    let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
    if COUNTING.load(Ordering::Relaxed) {
        CALLS.fetch_add(1, Ordering::Relaxed);
        PEAK.fetch_max(live, Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The fake upstream drains the request without keeping it and streams a short reply:
/// Responses events on the Codex path (`/responses`), Claude events otherwise.
async fn upstream(req: Request) -> axum::response::Response {
    let codex = req.uri().path().ends_with("/responses");
    let mut body = req.into_body().into_data_stream();
    while let Some(chunk) = body.next().await {
        drop(chunk.unwrap());
    }
    if codex {
        let sse = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"in_progress\",\"model\":\"gpt-5.5\",\"output\":[]}}\n\n\
                   event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"in_progress\",\"role\":\"assistant\",\"content\":[]}}\n\n\
                   event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"delta\":\"ok\"}\n\n\
                   event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"ok\",\"annotations\":[]}]}}\n\n\
                   event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"gpt-5.5\",\"output\":[{\"id\":\"msg_1\",\"type\":\"message\",\"status\":\"completed\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"ok\",\"annotations\":[]}]}],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n";
        return ([("content-type", "text/event-stream")], sse).into_response();
    }
    let sse = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-4-5\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n\
               event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
               event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n\
               event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
               event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n\
               event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    if HOLD.load(Ordering::Relaxed) {
        // The request is read; answer once the test says so, as a real upstream answers
        // a long prompt only after it has read it.
        WAITING.fetch_add(1, Ordering::Relaxed);
        RELEASE.acquire().await.unwrap().forget();
    }
    ([("content-type", "text/event-stream")], sse).into_response()
}

/// While set, the fake upstream counts each Claude request it has read in `WAITING` and
/// answers it only when `RELEASE` gives it a permit.
static HOLD: AtomicBool = AtomicBool::new(false);
static WAITING: AtomicUsize = AtomicUsize::new(0);
static RELEASE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(0);

/// Keeps a handle to each request's capture. A captured request is finalized in a task of
/// its own after its response's last byte, which frees the upstream body kept for an
/// error log; [`settle`] waits for that, so it is not freed inside the next measurement.
/// Adds no allocation to a request: the list's room is reserved up front, and the inner
/// service's future is returned as it is.
#[derive(Clone)]
struct Captures<S>(S, Arc<Mutex<Vec<RequestLog>>>);

impl<S: tower::Service<Request>> tower::Service<Request> for Captures<S> {
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), S::Error>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, request: Request) -> S::Future {
        if let Some(log) = request.extensions().get::<RequestLog>() {
            self.1.lock().unwrap().push(log.clone());
        }
        self.0.call(request)
    }
}

/// Waits until the capture of every request sent so far is finalized.
async fn settle(captures: &Mutex<Vec<RequestLog>>) {
    loop {
        let Some(log) = captures.lock().unwrap().pop() else {
            return;
        };
        log.finished().await;
    }
}

/// A coding-agent conversation of about `target` bytes, shaped like `bench/messages`:
/// an 18 KB system prompt, 24 tools, then assistant `tool_use` and user `tool_result`
/// turns.
fn conversation(target: usize) -> Bytes {
    use serde_json::json;
    let filler = |n: usize| "Explain the change in this diff and list any risks. ".repeat(n / 52 + 1);
    let tools: Vec<_> = (0..24)
        .map(|i| {
            json!({
                "name": format!("tool_{i}"),
                "description": filler(400),
                "input_schema": {"type": "object", "properties": {"path": {"type": "string"}, "text": {"type": "string"}}},
            })
        })
        .collect();
    let mut messages = vec![json!({"role": "user", "content": filler(2000)})];
    let mut size = 30_000;
    let mut turn = 0;
    while size < target {
        let id = format!("toolu_{turn:06}");
        messages.push(json!({"role": "assistant", "content": [
            {"type": "text", "text": filler(1000)},
            {"type": "tool_use", "id": id, "name": "tool_1", "input": {"path": "src/main.rs"}},
        ]}));
        messages.push(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": id, "content": filler(20_000)},
        ]}));
        size += 22_000;
        turn += 1;
    }
    let body = json!({
        "model": "claude-opus-5-5",
        "max_tokens": 1024,
        "stream": true,
        "system": [{"type": "text", "text": filler(18_000)}],
        "tools": tools,
        "messages": messages,
    });
    Bytes::from(serde_json::to_vec(&body).unwrap())
}

/// [`conversation`] for the cloaked OAuth login (`cloak/` model prefix).
fn cloak_conversation(target: usize) -> Bytes {
    let body = String::from_utf8(conversation(target).to_vec()).unwrap();
    Bytes::from(body.replacen(r#""model":"claude-opus-5-5""#, r#""model":"cloak/claude-opus-5-5""#, 1))
}

/// The same conversation for Codex, in the Responses format: 18 KB of instructions, 24
/// function tools, then reasoning items with encrypted content, function calls and
/// their large outputs.
fn responses_conversation(target: usize) -> Bytes {
    use serde_json::json;
    let filler = |n: usize| "Explain the change in this diff and list any risks. ".repeat(n / 52 + 1);
    let tools: Vec<_> = (0..24)
        .map(|i| {
            json!({
                "type": "function",
                "name": format!("tool_{i}"),
                "description": filler(400),
                "parameters": {"type": "object", "properties": {"path": {"type": "string"}, "text": {"type": "string"}}},
            })
        })
        .collect();
    let mut input =
        vec![json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": filler(2000)}]})];
    let mut size = 30_000;
    let mut turn = 0;
    while size < target {
        let call = format!("call_{turn:06}");
        input.push(json!({"type": "reasoning", "id": format!("rs_{turn:06}"), "summary": [{"type": "summary_text", "text": filler(600)}], "encrypted_content": "gAAAAABo".repeat(150)}));
        input.push(json!({"type": "function_call", "id": format!("fc_{turn:06}"), "call_id": call, "name": "tool_1", "arguments": "{\"path\":\"src/main.rs\"}"}));
        input.push(json!({"type": "function_call_output", "call_id": call, "output": filler(20_000)}));
        size += 22_000;
        turn += 1;
    }
    let body = json!({
        "model": "gpt-5.5",
        "instructions": filler(18_000),
        "input": input,
        "tools": tools,
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "reasoning": {"effort": "medium", "summary": "auto"},
        "store": false,
        "stream": true,
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": "alloc-budget",
    });
    Bytes::from(serde_json::to_vec(&body).unwrap())
}

/// [`responses_conversation`] for a model alias, so the Codex route routes `model` (#23:
/// the one body edit a native request gets).
fn responses_alias_conversation(target: usize) -> Bytes {
    let body = String::from_utf8(responses_conversation(target).to_vec()).unwrap();
    Bytes::from(body.replacen(r#""model":"gpt-5.5""#, r#""model":"codex-alias""#, 1))
}

/// [`conversation`] (Claude Messages) for the Codex model: the translated route (#27)
/// converts it to Responses.
fn claude_to_codex_conversation(target: usize) -> Bytes {
    let body = String::from_utf8(conversation(target).to_vec()).unwrap();
    Bytes::from(body.replacen(r#""model":"claude-opus-5-5""#, r#""model":"gpt-5.5""#, 1))
}

/// A Gemini request for the Codex model, its size from large tool schemas: 48 function
/// declarations whose `parameters` schemas hold 40 properties each, with Gemini's
/// upper-case `type` names, which the translated route converts at schema positions only
/// (#27), then turns of text, function calls and their large responses.
fn gemini_conversation(target: usize) -> Bytes {
    use serde_json::json;
    let filler = |n: usize| "Explain the change in this diff and list any risks. ".repeat(n / 52 + 1);
    let properties = |i: usize| {
        let mut map = serde_json::Map::new();
        for p in 0..40 {
            map.insert(
                format!("field_{i}_{p}"),
                json!({"type": "STRING", "description": filler(120), "enum": ["TYPE", "type", "OBJECT"]}),
            );
        }
        map.insert(
            "nested".into(),
            json!({"type": "ARRAY", "items": {"type": "OBJECT", "properties": {"type": {"type": "STRING"}}}}),
        );
        serde_json::Value::Object(map)
    };
    let declarations: Vec<_> = (0..48)
        .map(|i| json!({"name": format!("tool_{i}"), "description": filler(300), "parameters": {"type": "OBJECT", "properties": properties(i)}}))
        .collect();
    let mut contents = vec![json!({"role": "user", "parts": [{"text": filler(2000)}]})];
    let mut size = 360_000;
    let mut turn = 0;
    while size < target {
        contents.push(json!({"role": "model", "parts": [{"text": filler(1000)}, {"functionCall": {"name": "tool_1", "args": {"path": "src/main.rs"}}}]}));
        contents.push(json!({"role": "user", "parts": [{"functionResponse": {"name": "tool_1", "response": {"content": filler(20_000)}}}]}));
        size += 22_000;
        turn += 1;
    }
    let _ = turn;
    let body = json!({
        "systemInstruction": {"parts": [{"text": filler(18_000)}]},
        "contents": contents,
        "tools": [{"functionDeclarations": declarations}],
        "generationConfig": {"maxOutputTokens": 1024},
    });
    Bytes::from(serde_json::to_vec(&body).unwrap())
}

/// The ceiling for `key` in bench/budgets.txt: `<key> <value> <tolerance %>`, read the way
/// bench/gate.sh reads it. None when the file has no line for it: the value is reported,
/// not gated.
fn budget(key: &str) -> Option<usize> {
    include_str!("../../../bench/budgets.txt")
        .lines()
        .filter_map(|l| l.split('#').next())
        .find_map(|l| {
            let parts: Vec<&str> = l.split_whitespace().collect();
            (parts.first() == Some(&key)).then(|| {
                let value: f64 = parts[1].replace('_', "").parse().unwrap();
                let tolerance: f64 = parts.get(2).map_or(0.0, |t| t.parse().unwrap());
                (value * (1.0 + tolerance / 100.0)) as usize
            })
        })
}

/// [`budget`] for a key that must have a line.
fn gated(key: &str) -> usize {
    budget(key).unwrap_or_else(|| panic!("bench/budgets.txt has no {key}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_requests_stay_within_their_heap_budget() {
    // The fake upstream: its own thread and single-threaded runtime, not counted.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let upstream_url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        UNCOUNTED.with(|u| u.set(true));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(listener, axum::Router::new().fallback(upstream))
                .await
                .unwrap()
        });
    });
    let dir: PathBuf = std::env::temp_dir().join(format!("cpa-alloc-budget-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("claude.json"),
        r#"{"type":"claude","access_token":"tok-A","email":"a@example.com"}"#,
    )
    .unwrap();
    // A Claude OAuth login, reached only as `cloak/<model>`: its requests are cloaked
    // (the client is not Claude Code), signed and remapped, as for a coding agent on a
    // subscription. Only the steps for api.anthropic.com itself are skipped.
    std::fs::write(
        dir.join("claude-oauth.json"),
        r#"{"type":"claude","prefix":"cloak","access_token":"sk-ant-oat01-FAKE-alloc-budget","email":"b@example.com","expired":"2099-01-01T00:00:00Z","claude_device_ids":["0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"],"account_uuid":"6f1d2c3b-4a59-4e6f-8a7b-9c0d1e2f3a4b"}"#,
    )
    .unwrap();
    let mut config = Config::parse(&format!(
        "force-model-prefix: true\ncodex-api-key:\n  - api-key: sk-FAKE-codex\n    base-url: {upstream_url}\n    models:\n      - name: gpt-5.5\n      - name: gpt-5.5-routed\n        alias: codex-alias\n"
    ))
    .unwrap();
    config.api_keys = vec!["client-key-1".into()];
    config.auth_dir = dir.clone();
    let mut creds: Vec<_> = cpa_core::config::credentials::from_auth_dir(&config)
        .into_iter()
        .map(cpa_server::testing::local)
        .collect();
    creds.extend(cpa_core::config::credentials::from_config(&config));
    let executors = Executors {
        claude: ClaudeExecutor::new(&upstream_url).unwrap(),
        codex: Default::default(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(config, creds, executors));
    cpa_server::install_registry(&rt);
    // The cloaked login's requests also go through request capture, as in the server:
    // with request logs off it still keeps each upstream request body for a possible
    // error log (Go's deferred API request), until the response has streamed.
    let management = cpa_server::management::Management::with_options(
        rt.clone(),
        dir.join("config.yaml"),
        cpa_server::management::Options {
            log_dir: Some(dir.join("logs")),
            management_password: Some(String::new()),
            auth_dir: Some(dir.clone()),
            ..Default::default()
        },
    );
    // Room for every cloaked request this test sends: 2 warm-ups, 6 samples, 8 at once.
    let captures = Arc::new(Mutex::new(Vec::with_capacity(16)));
    let logged = cpa_server::request_logging::router(
        &management,
        axum::Router::new().fallback_service(Captures(cpa_server::router(rt.clone()), captures.clone())),
    );
    let app = cpa_server::router(rt);

    // `route` is "claude" or "cloak" (`/v1/messages`), "count" (`/v1/messages/count_tokens`)
    // or "codex" (`/v1/responses`). The request, and what its response must contain.
    let request = |route: &str, body: Bytes| {
        let (path, done): (_, &'static [u8]) = match route {
            "claude" | "cloak" | "codex-claude" => ("/v1/messages", b"message_stop"),
            "count" => ("/v1/messages/count_tokens", b"\"input_tokens\""),
            "codex-gemini" => (
                "/v1beta/models/gpt-5.5:streamGenerateContent?alt=sse",
                b"totalTokenCount",
            ),
            _ => ("/v1/responses", b"response.completed"),
        };
        let request = axum::http::Request::post(path)
            .header("x-api-key", "client-key-1")
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        (request, done)
    };
    let check = |status: axum::http::StatusCode, text: &[u8], done: &[u8]| {
        assert_eq!(status, 200, "{text:?}");
        assert!(text.windows(done.len()).any(|w| w == done), "{text:?}");
    };
    let send = |route: &'static str, body: Bytes| {
        let app = if route == "cloak" { logged.clone() } else { app.clone() };
        let (request, done) = request(route, body);
        async move {
            let response = app.oneshot(request).await.unwrap();
            let status = response.status();
            let text = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
            check(status, &text, done);
        }
    };

    let mut over = Vec::new();
    for (route, name, target) in [
        ("claude", "306k", 306_000),
        ("claude", "1900k", 1_900_000),
        ("count", "count.306k", 306_000),
        ("count", "count.1900k", 1_900_000),
        ("codex", "codex.100k", 100_000),
        ("codex", "codex.306k", 306_000),
        ("codex", "codex.1900k", 1_900_000),
        ("codex", "codex.7600k", 7_600_000),
        ("codex-alias", "codex.alias.100k", 100_000),
        ("codex-alias", "codex.alias.1900k", 1_900_000),
        ("codex-alias", "codex.alias.7600k", 7_600_000),
        ("codex-claude", "codex.claude.100k", 100_000),
        ("codex-claude", "codex.claude.1900k", 1_900_000),
        ("codex-claude", "codex.claude.7600k", 7_600_000),
        ("codex-gemini", "codex.gemini.400k", 400_000),
        ("codex-gemini", "codex.gemini.1900k", 1_900_000),
        ("codex-gemini", "codex.gemini.7600k", 7_600_000),
        ("cloak", "cloak.306k", 306_000),
        ("cloak", "cloak.1900k", 1_900_000),
    ] {
        let body = match route {
            "codex" => responses_conversation(target),
            "codex-alias" => responses_alias_conversation(target),
            "codex-claude" => claude_to_codex_conversation(target),
            "codex-gemini" => gemini_conversation(target),
            "cloak" => cloak_conversation(target),
            _ => conversation(target),
        };
        // Warm-up: connection pool, registries and lazy statics.
        send(route, body.clone()).await;
        settle(&captures).await;
        let (mut peak, mut calls) = (usize::MAX, usize::MAX);
        for _ in 0..3 {
            let base = LIVE.load(Ordering::Relaxed);
            PEAK.store(base, Ordering::Relaxed);
            CALLS.store(0, Ordering::Relaxed);
            COUNTING.store(true, Ordering::Relaxed);
            send(route, body.clone()).await;
            COUNTING.store(false, Ordering::Relaxed);
            peak = peak.min(PEAK.load(Ordering::Relaxed).saturating_sub(base));
            calls = calls.min(CALLS.load(Ordering::Relaxed));
            settle(&captures).await;
        }
        let (peak_max, calls_max) = (
            gated(&format!("alloc.{name}.peak_bytes")),
            gated(&format!("alloc.{name}.allocations")),
        );
        println!(
            "{name}: body {} B, peak live {peak} B ({:.2}x body, ceiling {peak_max}), {calls} allocations (ceiling {calls_max})",
            body.len(),
            peak as f64 / body.len() as f64,
        );
        if peak > peak_max {
            over.push(format!("alloc.{name}.peak_bytes {peak} > {peak_max}"));
        }
        if calls > calls_max {
            over.push(format!("alloc.{name}.allocations {calls} > {calls_max}"));
        }
    }

    // CONCURRENT cloaked 1.9 MB requests at once, each waiting for the fake upstream's
    // answer, as a coding agent's requests wait seconds for the first token of a long
    // prompt. `held` is the heap the eight keep between them while they wait, every one
    // prepared and sent; `peak` adds what their preparations overlapped, which depends on
    // scheduling.
    const CONCURRENT: usize = 8;
    let body = cloak_conversation(1_900_000);
    HOLD.store(true, Ordering::Relaxed);
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
    let tasks: Vec<_> = (0..CONCURRENT)
        .map(|_| {
            let app = logged.clone();
            let (request, done) = request("cloak", body.clone());
            tokio::spawn(async move {
                let response = app.oneshot(request).await.unwrap();
                let status = response.status();
                let text = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
                check(status, &text, done);
            })
        })
        .collect();
    // A request that fails before it reaches the upstream ends its task early: stop
    // waiting then, release the others and let its panic report why.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while WAITING.load(Ordering::Relaxed) < CONCURRENT
        && !tasks.iter().any(|task| task.is_finished())
        && std::time::Instant::now() < deadline
    {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let waiting = WAITING.load(Ordering::Relaxed);
    let held = LIVE.load(Ordering::Relaxed).saturating_sub(base);
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base);
    COUNTING.store(false, Ordering::Relaxed);
    RELEASE.add_permits(CONCURRENT);
    let aborts: Vec<_> = tasks.iter().map(|task| task.abort_handle()).collect();
    match tokio::time::timeout(
        std::time::Duration::from_secs(120),
        futures_util::future::join_all(tasks),
    )
    .await
    {
        Ok(results) => results.into_iter().for_each(|result| result.unwrap()),
        Err(_) => {
            aborts.iter().for_each(tokio::task::AbortHandle::abort);
            panic!("requests still running 120 s after the fake upstream answered");
        }
    }
    HOLD.store(false, Ordering::Relaxed);
    settle(&captures).await;
    assert_eq!(waiting, CONCURRENT, "requests that reached the fake upstream");
    // `held` must have a line; `peak` is reported until bench/budgets.txt gives it one.
    let key = |name: &str| format!("alloc.cloak.1900k.c{CONCURRENT}.{name}_bytes");
    for (name, value, ceiling) in [
        ("held", held, Some(gated(&key("held")))),
        ("peak", peak, budget(&key("peak"))),
    ] {
        let ratio = value as f64 / body.len() as f64;
        let Some(max) = ceiling else {
            println!("cloak.1900k x{CONCURRENT} waiting: {name} {value} B ({ratio:.1}x one body, reported, not gated)");
            continue;
        };
        println!("cloak.1900k x{CONCURRENT} waiting: {name} {value} B ({ratio:.1}x one body, ceiling {max})");
        if value > max {
            over.push(format!("{} {value} > {max}", key(name)));
        }
    }
    let _ = std::fs::remove_dir_all(dir);
    assert!(over.is_empty(), "over budget (bench/budgets.txt): {over:?}");
}
