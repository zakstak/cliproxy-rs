//! Response steering on the executor, ported case for case from Go's
//! internal/runtime/executor/codex_websockets_duplex*_test.go. Each upstream follows the Go
//! test's scripted conversation and the assertions are Go's. Cases that need the
//! conductor (failover, account state) live in cpa-server's `tests/duplex_dispatch.rs`.
//!
//! Go hands the executor an input channel; here the client's frames go through the
//! session's shared queue. Go's unbuffered sends become a send plus [`delivered`], which
//! waits until the duplex took the frame.

use super::*;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::stream::BoxStream;

const STEERING: &str = "codex:\n  response-steering: true\n";
const MODEL: &str = "gpt-6-astra";

/// One upstream conversation, as the Go test's handler runs it. Failures are collected
/// (Go's `t.Error` in the handler) and checked by [`Scripted::finish`].
struct Peer {
    socket: AxSocket,
    errors: Arc<Mutex<Vec<String>>>,
}

impl Peer {
    fn error(&self, message: String) {
        self.errors.lock().unwrap().push(message);
    }

    /// `c.ReadMessage()` with the Go test's read deadline.
    async fn read(&mut self) -> String {
        loop {
            match tokio::time::timeout(Duration::from_secs(10), self.socket.recv()).await {
                Ok(Some(Ok(AxMessage::Text(text)))) => return text.as_str().to_owned(),
                Ok(Some(Ok(AxMessage::Ping(_) | AxMessage::Pong(_)))) => continue,
                other => {
                    self.error(format!("upstream read: {other:?}"));
                    return String::new();
                }
            }
        }
    }

    async fn write(&mut self, payload: &str) {
        if self.socket.send(AxMessage::Text(payload.into())).await.is_err() {
            self.error(format!("upstream write: {payload}"));
        }
    }

    /// `_, _, _ = c.ReadMessage()`: hold the socket until the proxy sends or closes.
    async fn idle(&mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(10), self.socket.recv()).await;
    }
}

/// A loopback upstream running `script` for every dial.
struct Scripted {
    url: String,
    errors: Arc<Mutex<Vec<String>>>,
    dials: Arc<AtomicUsize>,
    done: Arc<AtomicUsize>,
}

impl Scripted {
    async fn start<F, Fut>(script: F) -> Self
    where
        F: Fn(Peer) -> Fut + Clone + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self::start_with_handshake(&[], script).await
    }

    /// [`Scripted::start`] with extra headers on every upgrade response.
    async fn start_with_handshake<F, Fut>(handshake: &'static [(&'static str, &'static str)], script: F) -> Self
    where
        F: Fn(Peer) -> Fut + Clone + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let errors = Arc::new(Mutex::new(Vec::new()));
        let (dials, done) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let state = (script, errors.clone(), dials.clone(), done.clone());
        let app = axum::Router::new().fallback(move |ws: WebSocketUpgrade| {
            let (script, errors, dials, done) = state.clone();
            async move {
                dials.fetch_add(1, Ordering::SeqCst);
                let mut response = ws.on_upgrade(move |socket| async move {
                    script(Peer { socket, errors }).await;
                    done.fetch_add(1, Ordering::SeqCst);
                });
                for (name, value) in handshake {
                    response.headers_mut().insert(
                        axum::http::HeaderName::from_static(name),
                        axum::http::HeaderValue::from_static(value),
                    );
                }
                response
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self {
            url,
            errors,
            dials,
            done,
        }
    }

    /// Every conversation ended (the proxy closed its socket) and none failed.
    async fn finish(&self) {
        let started = Instant::now();
        while self.done.load(Ordering::SeqCst) < self.dials.load(Ordering::SeqCst) {
            assert!(started.elapsed() < Duration::from_secs(3), "upstream cleanup stalled");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let errors = self.errors.lock().unwrap().clone();
        assert!(errors.is_empty(), "upstream: {errors:?}");
    }
}

/// A steering executor with the session's client queue (`WithWebsocketInput`).
struct Client {
    executor: CodexExecutor,
    input: tokio::sync::mpsc::Sender<Bytes>,
    frames: ClientFrames,
    session: String,
}

impl Client {
    fn new(session: &str) -> Self {
        let executor = CodexExecutor::new().unwrap();
        let (input, rx) = tokio::sync::mpsc::channel(1);
        let frames: ClientFrames = Arc::new(tokio::sync::Mutex::new(rx));
        executor.attach_steering(session, SteeringInput::new(frames.clone(), |_| true));
        Self {
            executor,
            input,
            frames,
            session: session.to_owned(),
        }
    }

    /// `exec.ExecuteStream` with Go's `opts.SourceFormat` (the response format follows it).
    async fn start(&self, url: &str, body: &str, source: Format) -> BoxStream<'static, Result<Bytes, ExecError>> {
        self.start_with(url, body, source, Default::default()).await
    }

    async fn start_with(
        &self,
        url: &str,
        body: &str,
        source: Format,
        usage: cpa_core::exec::UsageSink,
    ) -> BoxStream<'static, Result<Bytes, ExecError>> {
        self.start_config(url, body, source, usage, STEERING).await
    }

    async fn start_config(
        &self,
        url: &str,
        body: &str,
        source: Format,
        usage: cpa_core::exec::UsageSink,
        config: &str,
    ) -> BoxStream<'static, Result<Bytes, ExecError>> {
        let mut req = request(body);
        req.usage = usage;
        req.model = MODEL.into();
        req.requested_model = MODEL.into();
        req.source_format = source;
        req.response_format = source;
        req.execution_session = Some(self.session.clone());
        let session = ExecSession {
            id: self.session.clone(),
            continuation: false,
            lease: None,
        };
        let cfg = Config::parse(config).unwrap();
        let response = self
            .executor
            .execute_in_session(&credential(url), req, &cfg, &session)
            .await
            .unwrap();
        let ResponseBody::Stream(stream) = response.body else {
            panic!("websocket turns stream");
        };
        stream
    }

    /// `input <- payload` on Go's unbuffered channel: returns once the duplex took it.
    async fn send(&self, payload: &str) {
        self.input.send(Bytes::from(payload.to_owned())).await.unwrap();
        delivered(&self.input).await;
    }
}

async fn delivered(input: &tokio::sync::mpsc::Sender<Bytes>) {
    let started = Instant::now();
    while input.capacity() < input.max_capacity() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the duplex never read the frame"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

async fn next(stream: &mut BoxStream<'static, Result<Bytes, ExecError>>) -> Option<Result<String, ExecError>> {
    tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("stream stalled")
        .map(|item| item.map(|b| String::from_utf8_lossy(&b).into_owned()))
}

fn get(payload: &str, path: &str) -> String {
    crate::rawjson::get(payload, path).str().to_owned()
}

// ------------------------------------------------------------------ codex_websockets_duplex_test.go

/// `TestCodexDuplexSteeringLifecycle`: the upstream cannot finish the first response
/// until it reads the steer, so a serial request/response proxy deadlocks.
#[tokio::test]
async fn steering_lifecycle() {
    for boundary in ["response.incomplete", "response.completed"] {
        const STEER: &str = r#"{"type":"response.steer","previous_response_id":"r1","input":[{"role":"user","content":[{"type":"input_text","text":"STEER_OK"}]}]}"#;
        const ACCEPTED: &str =
            r#"{"type":"response.steer.accepted","sequence_number":2,"steer":{"id":"s1","previous_response_id":"r1"}}"#;
        const PENDING: &str = r#"{"type":"response.steer.pending","sequence_number":8,"steer":{"id":"s2","previous_response_id":"r2"},"reason":"waiting_for_required_input","required_input":[{"type":"function_call_output","call_id":"c1","name":"lookup"}]}"#;
        const FAILED: &str = r#"{"type":"response.steer.failed","sequence_number":14,"steer":{"id":"s3","previous_response_id":"missing","input":"recover me"},"error":{"code":"response_not_found","message":"missing response"}}"#;
        let up = Scripted::start(move |mut c: Peer| async move {
            let first = c.read().await;
            if get(&first, "type") != "response.create" {
                c.error(format!("initial create: {first}"));
            }
            c.write(r#"{"type":"response.created","response":{"id":"r1","output":[]}}"#).await;
            let got = c.read().await;
            if got != STEER {
                c.error(format!("steer transformed: {got}"));
            }
            c.write(ACCEPTED).await;
            c.write(&format!(
                r#"{{"type":"{boundary}","response":{{"id":"r1","output":[],"incomplete_details":{{"reason":"steered"}}}}}}"#
            ))
            .await;
            c.write(r#"{"type":"response.created","response":{"id":"r2","output":[]}}"#).await;
            let got = c.read().await;
            if get(&got, "type") != "response.steer" {
                c.error(format!("second control: {got}"));
            }
            c.write(r#"{"type":"response.steer.accepted","steer":{"id":"s2","previous_response_id":"r2"}}"#)
                .await;
            c.write(r#"{"type":"response.completed","response":{"id":"r2","output":[{"type":"function_call","call_id":"c1","name":"lookup","arguments":"{}"}]}}"#).await;
            c.write(PENDING).await;
            let got = c.read().await;
            if get(&got, "type") != "response.create"
                || get(&got, "previous_response_id") != "r2"
                || get(&got, "input.0.call_id") != "c1"
            {
                c.error(format!("tool continuation: {got}"));
            }
            if get(&got, "instructions") != "New settings" {
                c.error(format!("explicit settings not applied: {got}"));
            }
            c.write(r#"{"type":"response.created","response":{"id":"r3","output":[]}}"#).await;
            c.write(r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"STEER_OK"}]}}"#).await;
            c.write(r#"{"type":"response.completed","response":{"id":"r3","output":[]}}"#).await;
            // Idle steering must also reach the same live upstream connection.
            let got = c.read().await;
            if get(&got, "previous_response_id") != "missing" {
                c.error(format!("idle control missing: {got}"));
            }
            c.write(FAILED).await;
            c.idle().await;
        })
        .await;
        let client = Client::new(&format!("duplex-test-{boundary}"));
        let mut stream = client
            .start(
                &up.url,
                r#"{"model":"gpt-6-astra","input":[],"instructions":"Initial settings"}"#,
                Format::Codex,
            )
            .await;
        let mut seen = std::collections::HashSet::new();
        while !seen.contains("failed") {
            let chunk = next(&mut stream)
                .await
                .expect("stream ended before all responses")
                .unwrap();
            let (event, id) = (get(&chunk, "type"), get(&chunk, "response.id"));
            match (event.as_str(), id.as_str()) {
                ("response.created", "r1") => client.send(STEER).await,
                ("response.steer.accepted", _) if get(&chunk, "steer.id") == "s1" => {
                    assert_eq!(chunk, ACCEPTED, "accepted event changed");
                    seen.insert("accepted");
                }
                (e, "r1") if e == boundary => {
                    seen.insert("boundary");
                }
                ("response.created", "r2") => {
                    client
                        .send(r#"{"type":"response.steer","previous_response_id":"r2","input":"Use tool result"}"#)
                        .await;
                }
                ("response.steer.pending", _) => {
                    assert_eq!(chunk, PENDING, "pending event changed");
                    seen.insert("pending");
                    client.send(r#"{"type":"response.create","previous_response_id":"r2","instructions":"New settings","input":[{"type":"function_call_output","call_id":"c1","output":"ok"}]}"#).await;
                }
                ("response.output_item.done", _) => {
                    assert_eq!(get(&chunk, "item.content.0.text"), "STEER_OK", "successor output");
                    seen.insert("item");
                }
                ("response.completed", "r3") => {
                    // As the upstream sent it: the proxy repairs no output (#24, #28).
                    assert_eq!(
                        chunk,
                        r#"{"type":"response.completed","response":{"id":"r3","output":[]}}"#
                    );
                    assert!(seen.contains("item"), "successor output arrives before its completion");
                    seen.insert("successor");
                    client
                        .send(r#"{"type":"response.steer","previous_response_id":"missing","input":"recover me"}"#)
                        .await;
                }
                ("response.steer.failed", _) => {
                    assert_eq!(chunk, FAILED, "failed input changed");
                    seen.insert("failed");
                }
                _ => {}
            }
        }
        drop(stream);
        up.finish().await;
        assert_eq!(up.dials.load(Ordering::SeqCst), 1, "connections");
        for key in ["accepted", "boundary", "pending", "successor", "failed"] {
            assert!(seen.contains(key), "{boundary}: missing {key}");
        }
    }
}

/// Go's `TestCodexDuplexAppendInheritsContextAndInstructions` built a create from a
/// `response.append` (the parent, model and instructions filled in). No first-party or
/// documented source defines the frame, so a steering connection refuses it with a local
/// error event and stays open; nothing reaches the upstream (#28).
#[tokio::test]
async fn append_is_refused_on_the_open_connection() {
    let up = Scripted::start(|mut c: Peer| async move {
        c.read().await;
        c.write(r#"{"type":"response.created","response":{"id":"resp-1","output":[]}}"#)
            .await;
        c.write(r#"{"type":"response.completed","response":{"id":"resp-1","output":[]}}"#)
            .await;
        let next = c.read().await;
        if get(&next, "type") != "response.create" || get(&next, "previous_response_id") != "resp-1" {
            c.error(format!("only the client's create follows the refused append: {next}"));
        }
        c.write(r#"{"type":"response.created","response":{"id":"resp-2","output":[]}}"#)
            .await;
        c.write(r#"{"type":"response.completed","response":{"id":"resp-2","output":[]}}"#)
            .await;
        c.idle().await;
    })
    .await;
    let client = Client::new("append-test");
    let mut stream = client
        .start(
            &up.url,
            r#"{"model":"gpt-6-astra","input":[{"role":"user","content":"turn 1"}],"instructions":"Initial system instructions"}"#,
            Format::Codex,
        )
        .await;
    let mut refused = false;
    while let Some(chunk) = next(&mut stream).await {
        let chunk = chunk.unwrap();
        let (event, id) = (get(&chunk, "type"), get(&chunk, "response.id"));
        if event == "response.completed" && id == "resp-1" {
            client
                .send(r#"{"type":"response.append","input":[{"role":"user","content":"turn 2"}]}"#)
                .await;
        }
        if event == "error" {
            assert_eq!(get(&chunk, "status"), "400", "{chunk}");
            assert_eq!(
                get(&chunk, "error.message"),
                crate::codex::native::APPEND_REFUSED,
                "{chunk}"
            );
            refused = true;
            client
                .send(r#"{"type":"response.create","model":"gpt-6-astra","previous_response_id":"resp-1","input":[{"role":"user","content":"turn 2"}]}"#)
                .await;
        }
        if event == "response.completed" && id == "resp-2" {
            break;
        }
    }
    assert!(refused, "the append was refused");
    drop(stream);
    up.finish().await;
}

/// `TestCodexDuplexStandaloneCreateDoesNotInheritParentID`.
#[tokio::test]
async fn standalone_create_does_not_inherit_parent_id() {
    let up = Scripted::start(|mut c: Peer| async move {
        c.read().await;
        c.write(r#"{"type":"response.created","response":{"id":"resp-1","output":[]}}"#)
            .await;
        c.write(r#"{"type":"response.completed","response":{"id":"resp-1","output":[]}}"#)
            .await;
        let second = c.read().await;
        if get(&second, "type") != "response.create" {
            c.error(format!("second type: {second}"));
        }
        if crate::rawjson::get(&second, "previous_response_id").exists() {
            c.error(format!(
                "standalone create must not gain previous_response_id: {second}"
            ));
        }
        c.write(r#"{"type":"response.created","response":{"id":"resp-2","output":[]}}"#)
            .await;
        c.write(r#"{"type":"response.completed","response":{"id":"resp-2","output":[]}}"#)
            .await;
        c.idle().await;
    })
    .await;
    let client = Client::new("standalone-test");
    let mut stream = client
        .start(
            &up.url,
            r#"{"model":"gpt-6-astra","input":[{"role":"user","content":"turn 1"}]}"#,
            Format::Codex,
        )
        .await;
    while let Some(chunk) = next(&mut stream).await {
        let chunk = chunk.unwrap();
        let (event, id) = (get(&chunk, "type"), get(&chunk, "response.id"));
        if event == "response.completed" && id == "resp-1" {
            client
                .send(r#"{"type":"response.create","model":"gpt-6-astra","input":[{"role":"user","content":"turn 2 standalone"}]}"#)
                .await;
        }
        if event == "response.completed" && id == "resp-2" {
            break;
        }
    }
    drop(stream);
    up.finish().await;
}

/// `TestCodexDuplexQueuedCreateDoesNotBlockSubsequentSteer`.
#[tokio::test]
async fn queued_create_does_not_block_subsequent_steer() {
    let up = Scripted::start(|mut c: Peer| async move {
        let first = c.read().await;
        if get(&first, "type") != "response.create" {
            c.error(format!("expected response.create: {first}"));
        }
        c.write(r#"{"type":"response.created","response":{"id":"r1","output":[]}}"#).await;
        let steer1 = c.read().await;
        if get(&steer1, "type") != "response.steer" {
            c.error(format!("expected response.steer: {steer1}"));
        }
        c.write(r#"{"type":"response.steer.accepted","steer":{"id":"s1","previous_response_id":"r1"}}"#)
            .await;
        c.write(r#"{"type":"response.incomplete","response":{"id":"r1","output":[],"incomplete_details":{"reason":"steered"}}}"#).await;
        // The automatic successor holds back the next create.
        c.write(r#"{"type":"response.created","response":{"id":"auto-1","previous_response_id":"r1","output":[]}}"#)
            .await;
        // The steer the client sent after that create must arrive first.
        let steer = c.read().await;
        if get(&steer, "type") != "response.steer" {
            c.error(format!("expected response.steer to arrive before blocked create, got: {steer}"));
            return;
        }
        c.write(r#"{"type":"response.steer.accepted","steer":{"id":"s2","previous_response_id":"auto-1"}}"#)
            .await;
        c.write(r#"{"type":"response.completed","response":{"id":"auto-1","output":[]}}"#).await;
        c.write(r#"{"type":"response.created","response":{"id":"auto-2","previous_response_id":"auto-1","output":[]}}"#)
            .await;
        c.write(r#"{"type":"response.completed","response":{"id":"auto-2","output":[]}}"#).await;
        let queued = c.read().await;
        if get(&queued, "type") != "response.create" {
            c.error(format!("queued create: {queued}"));
        }
        c.write(r#"{"type":"response.created","response":{"id":"r2","output":[]}}"#).await;
        c.write(r#"{"type":"response.completed","response":{"id":"r2","output":[]}}"#).await;
        c.idle().await;
    })
    .await;
    let client = Client::new("steer-block-test");
    let mut stream = client
        .start(
            &up.url,
            r#"{"model":"gpt-6-astra","input":[{"role":"user","content":"start"}]}"#,
            Format::Codex,
        )
        .await;
    while let Some(chunk) = next(&mut stream).await {
        let chunk = chunk.unwrap();
        let (event, id) = (get(&chunk, "type"), get(&chunk, "response.id"));
        if event == "response.created" && id == "r1" {
            client
                .send(r#"{"type":"response.steer","previous_response_id":"r1","input":"steer 1"}"#)
                .await;
        }
        if event == "response.created" && id == "auto-1" {
            client.send(r#"{"type":"response.create","model":"gpt-6-astra","previous_response_id":"auto-1","input":[{"role":"user","content":"queued next"}]}"#).await;
            client
                .send(r#"{"type":"response.steer","previous_response_id":"auto-1","input":"steer 2 in flight"}"#)
                .await;
        }
        if event == "response.completed" && id == "r2" {
            break;
        }
    }
    drop(stream);
    up.finish().await;
}

// ------------------------------------------------- codex_websockets_duplex_successor_metadata_test.go

/// `TestCodexDuplexAutomaticSuccessorMetadata`: a create queued behind steering must not
/// supply an automatic successor's settings while it waits to be sent.
#[tokio::test]
async fn automatic_successor_metadata() {
    for scenario in [
        "completed",
        "incomplete",
        "early_tool_result",
        "failed_steering",
        "multiple_steers",
        "create_before_steer",
    ] {
        let queued = Arc::new(tokio::sync::Notify::new());
        let before_steer = Arc::new(tokio::sync::Notify::new());
        let (q, b) = (queued.clone(), before_steer.clone());
        let up = Scripted::start(move |mut c: Peer| {
            let (queued, before_steer) = (q.clone(), b.clone());
            async move {
                let response = |kind: &str, id: &str, key: &str| {
                    let parent = if id == "automatic" { "first" } else { "" };
                    format!(
                        r#"{{"type":"{kind}","response":{{"id":"{id}","previous_response_id":"{parent}","prompt_cache_key":"{key}","output":[],"incomplete_details":{{"reason":"steered"}}}}}}"#
                    )
                };
                let first = c.read().await;
                let first_key = get(&first, "prompt_cache_key");
                c.write(&response("response.created", "first", &first_key)).await;
                if scenario == "create_before_steer" {
                    let middle = c.read().await;
                    if get(&middle, "type") != "response.create" {
                        c.error(format!("expected middle create: {middle}"));
                        return;
                    }
                    if tokio::time::timeout(Duration::from_secs(3), before_steer.notified())
                        .await
                        .is_err()
                    {
                        c.error("steering was not queued".into());
                        return;
                    }
                    let middle_key = get(&middle, "prompt_cache_key");
                    c.write(&response("response.completed", "first", &first_key)).await;
                    c.write(&response("response.created", "middle", &middle_key)).await;
                    c.write(&response("response.completed", "middle", &middle_key)).await;
                }
                let p = c.read().await;
                if get(&p, "type") != "response.steer" {
                    c.error(format!("expected steering: {p}"));
                    return;
                }
                c.write(r#"{"type":"response.steer.accepted","steer":{"id":"s1","previous_response_id":"first"}}"#)
                    .await;
                if scenario == "multiple_steers" {
                    let p = c.read().await;
                    if get(&p, "type") != "response.steer" {
                        c.error(format!("expected second steering: {p}"));
                        return;
                    }
                    c.write(r#"{"type":"response.steer.accepted","steer":{"id":"s2","previous_response_id":"first"}}"#)
                        .await;
                    c.write(r#"{"type":"response.steer.failed","steer":{"id":"s2","previous_response_id":"first","input":"second"},"error":{"code":"invalid_input","message":"second failed"}}"#).await;
                }
                if tokio::time::timeout(Duration::from_secs(3), queued.notified())
                    .await
                    .is_err()
                {
                    c.error("create was not queued".into());
                    return;
                }
                let boundary = if scenario == "incomplete" {
                    "response.incomplete"
                } else {
                    "response.completed"
                };
                if scenario != "create_before_steer" {
                    c.write(&response(boundary, "first", &first_key)).await;
                }
                match scenario {
                    "early_tool_result" => c.write(r#"{"type":"response.steer.pending","steer":{"id":"s1","previous_response_id":"first"},"reason":"waiting_for_required_input","required_input":[{"type":"function_call_output","call_id":"c1"}]}"#).await,
                    "failed_steering" => c.write(r#"{"type":"response.steer.failed","steer":{"id":"s1","previous_response_id":"first","input":"first"},"error":{"code":"successor_creation_failed","message":"failed"}}"#).await,
                    _ => {
                        c.write(&response("response.created", "automatic", &first_key)).await;
                        c.write(&response("response.completed", "automatic", &first_key)).await;
                    }
                }
                let next = c.read().await;
                if get(&next, "type") != "response.create" {
                    c.error(format!("expected explicit create: {next}"));
                    return;
                }
                let next_key = get(&next, "prompt_cache_key");
                c.write(&response("response.created", "explicit", &next_key)).await;
                c.write(&response("response.completed", "explicit", &next_key)).await;
                c.idle().await;
            }
        })
        .await;
        let client = Client::new(&format!("successor-{scenario}"));
        let request = |key: &str| {
            format!(
                r#"{{"type":"response.create","model":"gpt-6-astra","prompt_cache_key":"{key}","previous_response_id":"first","input":[]}}"#
            )
        };
        // Go's executor test leaves the execution session out of `req.Metadata`, so its
        // Codex-source key survives; through the conductor Go replaces it with the session
        // UUID, as this executor does. Responses clients keep their key in both.
        let mut stream = client
            .start(&up.url, &request("first-key"), Format::OpenAIResponse)
            .await;
        let (mut queued_create, mut automatic, mut explicit) = (false, false, false);
        while let Some(chunk) = next(&mut stream).await {
            let chunk = chunk.unwrap_or_else(|e| panic!("{scenario}: {e}"));
            let (event, id) = (get(&chunk, "type"), get(&chunk, "response.id"));
            if event == "response.created" && id == "first" {
                if scenario == "create_before_steer" {
                    client.send(&request("middle-key")).await;
                }
                client
                    .send(r#"{"type":"response.steer","previous_response_id":"first","input":"first"}"#)
                    .await;
                if scenario == "create_before_steer" {
                    before_steer.notify_one();
                }
            }
            if event == "response.steer.accepted" {
                if scenario == "multiple_steers" && get(&chunk, "steer.id") == "s1" {
                    client
                        .send(r#"{"type":"response.steer","previous_response_id":"first","input":"second"}"#)
                        .await;
                } else if !queued_create {
                    queued_create = true;
                    client.send(&request("explicit-key")).await;
                    queued.notify_one();
                }
            }
            if event == "response.created" || event == "response.completed" {
                let want = match id.as_str() {
                    "explicit" => "explicit-key",
                    "middle" => "middle-key",
                    _ => "first-key",
                };
                assert_eq!(
                    get(&chunk, "response.prompt_cache_key"),
                    want,
                    "{scenario}: {event} {id} settings"
                );
                automatic |= id == "automatic" && event == "response.completed";
                if id == "explicit" && event == "response.completed" {
                    explicit = true;
                    break;
                }
            }
        }
        drop(stream);
        let want_auto = scenario != "early_tool_result" && scenario != "failed_steering";
        assert!(
            explicit && automatic == want_auto,
            "{scenario}: automatic={automatic} want={want_auto} explicit={explicit}"
        );
        up.finish().await;
    }
}

// ---------------------------------------------------------- codex_websockets_duplex_rejection_test.go

/// `TestCodexDuplexRejectedCreateMetadata`: the upstream waits for every explicit create
/// before rejecting one, so queue ownership is deterministic.
#[tokio::test]
async fn rejected_create_metadata() {
    for (name, active_failure, ambiguous) in [
        ("rejected_create", false, false),
        ("active_failure", true, false),
        ("ambiguous_failure", true, true),
    ] {
        let up = Scripted::start(move |mut c: Peer| async move {
            let event = |kind: &str, id: &str, key: &str| {
                format!(
                    r#"{{"type":"{kind}","response":{{"id":"{id}","prompt_cache_key":"{key}","output":[],"error":{{"type":"invalid_request_error","message":"rejected"}}}}}}"#
                )
            };
            let first_key = get(&c.read().await, "prompt_cache_key");
            c.write(&event("response.created", "first", &first_key)).await;
            let (mut rejected_key, mut rejected_id) = (first_key.clone(), "first");
            if !active_failure {
                c.write(&event("response.completed", "first", &first_key)).await;
                rejected_key = get(&c.read().await, "prompt_cache_key");
                rejected_id = "rejected";
            }
            let good_key = get(&c.read().await, "prompt_cache_key");
            if ambiguous {
                c.write(&event("response.failed", "", &rejected_key)).await;
                c.idle().await;
                return;
            }
            c.write(&event("response.failed", rejected_id, &rejected_key)).await;
            c.write(&event("response.created", "good", &good_key)).await;
            c.write(&event("response.completed", "good", &good_key)).await;
            c.idle().await;
        })
        .await;
        let client = Client::new(&format!("metadata-{name}"));
        let request = |key: &str| {
            format!(r#"{{"type":"response.create","model":"gpt-6-astra","prompt_cache_key":"{key}","input":[]}}"#)
        };
        // Responses source: see `automatic_successor_metadata`.
        let mut stream = client
            .start(&up.url, &request("first-key"), Format::OpenAIResponse)
            .await;
        let (mut sent, mut failed, mut completed, mut terminated) = (false, false, false, false);
        while let Some(chunk) = next(&mut stream).await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    assert!(ambiguous && error.scope == FailureScope::Request, "{name}: {error}");
                    terminated = true;
                    continue;
                }
            };
            let (kind, id) = (get(&chunk, "type"), get(&chunk, "response.id"));
            if !sent
                && id == "first"
                && ((active_failure && kind == "response.created") || (!active_failure && kind == "response.completed"))
            {
                sent = true;
                if !active_failure {
                    client.send(&request("rejected-key")).await;
                }
                client.send(&request("good-key")).await;
            }
            if kind == "response.failed" {
                failed = true;
                if ambiguous {
                    continue;
                }
                let want = if active_failure { "first-key" } else { "rejected-key" };
                assert_eq!(
                    get(&chunk, "response.prompt_cache_key"),
                    want,
                    "{name}: failure metadata"
                );
            }
            if kind == "response.completed" && id == "good" {
                completed = true;
                assert_eq!(
                    get(&chunk, "response.prompt_cache_key"),
                    "good-key",
                    "{name}: success consumed another create's metadata"
                );
                break;
            }
        }
        drop(stream);
        if ambiguous {
            assert!(
                failed && terminated && !completed,
                "{name}: failed={failed} terminated={terminated} completed={completed}"
            );
        } else {
            assert!(failed && completed, "{name}: failed={failed} completed={completed}");
        }
        up.finish().await;
    }
}

/// The four upstream rejections of `TestCodexDuplexInitialFailure`: (name, payload,
/// status, quota, headers).
const INITIAL_FAILURES: [(&str, &str, u16, bool, bool); 4] = [
    (
        "response_failed_auth",
        r#"{"type":"response.failed","response":{"error":{"type":"authentication_error","message":"expired credential"}}}"#,
        401,
        false,
        false,
    ),
    (
        "response_failed_quota",
        r#"{"type":"response.failed","response":{"error":{"type":"usage_limit_reached","message":"quota exhausted","resets_in_seconds":3600}}}"#,
        429,
        true,
        false,
    ),
    // The top-level status is authoritative even when the error type is generic.
    (
        "error_auth",
        r#"{"type":"error","status":401,"headers":{"X-Request-Id":"initial-rejection"},"error":{"type":"server_error","message":"expired credential"}}"#,
        401,
        false,
        true,
    ),
    (
        "error_quota",
        r#"{"type":"error","status_code":429,"headers":{"X-Request-Id":"initial-rejection"},"error":{"type":"usage_limit_reached","message":"quota exhausted","resets_in_seconds":3600}}"#,
        429,
        true,
        true,
    ),
];

/// `TestCodexDuplexInitialFailure` without failover: the rejection is the stream's only
/// item, an account error with its status, quota scope, retry delay and headers.
#[tokio::test]
async fn initial_failure() {
    for (name, payload, status, quota, headers) in INITIAL_FAILURES {
        let up = Scripted::start(move |mut c: Peer| async move {
            c.read().await;
            c.write(payload).await;
            // Keep the rejected socket open: the executor must close it itself.
            c.idle().await;
        })
        .await;
        let client = Client::new(&format!("initial-{name}"));
        let mut stream = client
            .start(
                &up.url,
                r#"{"model":"duplex-initial-failure-model","input":[]}"#,
                Format::Codex,
            )
            .await;
        let error = next(&mut stream)
            .await
            .expect("an error chunk")
            .expect_err("initial rejection must be an error chunk");
        assert_eq!(error.status, status, "{name}: status lost: {error}");
        assert_ne!(
            error.scope,
            FailureScope::Request,
            "{name}: upstream rejection became a connection-only error"
        );
        if quota {
            assert_eq!(
                error.scope,
                FailureScope::Credential,
                "{name}: credential quota scope lost"
            );
            assert_eq!(
                error.retry_after,
                Some(Duration::from_secs(3600)),
                "{name}: upstream retry delay lost"
            );
        }
        if headers {
            assert_eq!(
                error.headers.get("x-request-id").and_then(|v| v.to_str().ok()),
                Some("initial-rejection"),
                "{name}: upstream error headers lost"
            );
        }
        assert!(
            next(&mut stream).await.is_none(),
            "{name}: rejected stream remained open"
        );
        up.finish().await;
        assert_eq!(up.dials.load(Ordering::SeqCst), 1, "{name}: attempts");
    }
}

// ------------------------------------------------- codex_websockets_duplex_bootstrap_input_test.go

/// `TestCodexDuplexBootstrapCancellationPreservesInput`: cancelling before
/// `response.created` leaves the queued follow-up unread and closes the socket.
#[tokio::test]
async fn bootstrap_cancellation_preserves_input() {
    let initial_read = Arc::new(tokio::sync::Notify::new());
    let read = initial_read.clone();
    let up = Scripted::start(move |mut c: Peer| {
        let read = read.clone();
        async move {
            c.read().await;
            read.notify_one();
            // No response.created: cancellation must release the blocked writer.
            c.idle().await;
        }
    })
    .await;
    let client = Client::new("bootstrap-cancel");
    client
        .input
        .send(Bytes::from_static(br#"{"type":"response.steer","input":[]}"#))
        .await
        .unwrap();
    let stream = client
        .start(
            &up.url,
            r#"{"model":"bootstrap-cancel-model","input":[]}"#,
            Format::Codex,
        )
        .await;
    tokio::time::timeout(Duration::from_secs(5), initial_read.notified())
        .await
        .expect("initial request not sent");
    // Go cancels the context; here the consumer drops the stream.
    drop(stream);
    up.finish().await;
    let mut frames = tokio::time::timeout(Duration::from_secs(3), client.frames.clone().lock_owned())
        .await
        .expect("the cancelled duplex released the client queue");
    assert!(frames.try_recv().is_ok(), "follow-up consumed before response.created");
}

// ------------------------------------------------------- codex_websockets_duplex_health_test.go

/// `TestCodexDuplexConnectionTimeoutDoesNotCoolHealthyAccount`, executor half: the
/// client's input failing after a completed response ends the stream with a connection
/// error, never an account error. (Go injects a read timeout through its input; the
/// queue here only closes, which is what Go's own downstream reader does on any error.)
/// The conductor half is cpa-server's `duplex_dispatch.rs`.
#[tokio::test]
async fn input_failure_is_a_connection_error() {
    let up = Scripted::start(|mut c: Peer| async move {
        c.read().await;
        c.write(r#"{"type":"response.created","response":{"id":"r1"}}"#).await;
        c.write(r#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#)
            .await;
        c.idle().await;
    })
    .await;
    let mut client = Client::new("duplex-health-session");
    let mut stream = client
        .start(&up.url, r#"{"model":"duplex-health-model","input":[]}"#, Format::Codex)
        .await;
    let (mut completed, mut error) = (false, None);
    while let Some(chunk) = next(&mut stream).await {
        match chunk {
            Ok(chunk) if get(&chunk, "type") == "response.completed" => {
                completed = true;
                // The downstream reader is gone: the queue closes.
                let (closed, _) = tokio::sync::mpsc::channel(1);
                drop(std::mem::replace(&mut client.input, closed));
            }
            Ok(_) => {}
            Err(e) => error = Some(e),
        }
    }
    let error = error.expect("the stream ends with an error");
    assert!(completed);
    assert_eq!(error.scope, FailureScope::Request, "{error}");
    up.finish().await;
}

/// Upstream capture in the duplex (`codex_websockets_duplex.go`): each frame the writer
/// sends is `api.websocket.request` with only the URL, method, body, provider and
/// account ID; each upstream message is recorded as received.
#[tokio::test]
async fn capture_records_duplex_frames() {
    use crate::codex_testkit::{Tap, Wiretap};
    const STEER: &str = r#"{"type":"response.steer","previous_response_id":"r1","input":"more"}"#;
    const ACCEPTED: &str = r#"{"type":"response.steer.accepted","steer":{"id":"s1","previous_response_id":"r1"}}"#;
    const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#;
    let up = Scripted::start(|mut c: Peer| async move {
        c.read().await;
        c.write(r#"{"type":"response.created","response":{"id":"r1","output":[]}}"#)
            .await;
        c.read().await;
        c.write(ACCEPTED).await;
        c.write(COMPLETED).await;
        c.idle().await;
    })
    .await;
    let client = Client::new("capture-duplex");
    let tap = Arc::new(Wiretap::default());
    let usage = cpa_core::exec::UsageSink::default().with_capture(cpa_core::exec::CaptureSink::new(tap.clone()));
    let mut stream = client
        .start_with(&up.url, r#"{"model":"gpt-6-astra","input":[]}"#, Format::Codex, usage)
        .await;
    while let Some(chunk) = next(&mut stream).await {
        let chunk = chunk.unwrap();
        match get(&chunk, "type").as_str() {
            "response.created" => client.send(STEER).await,
            "response.completed" => break,
            _ => {}
        }
    }
    drop(stream);
    up.finish().await;
    let taps = tap.taps();
    let credential = credential(&up.url);
    let ws_url = format!("{}/responses", up.url.replacen("http://", "ws://", 1));
    let frame = Tap::WsRequest {
        url: ws_url,
        headers: vec![],
        body: STEER.into(),
        account: ["codex", credential.id.as_str(), "", "", ""].map(str::to_owned),
    };
    assert!(taps.contains(&frame), "{taps:#?}");
    let responses: Vec<&Tap> = taps.iter().filter(|t| matches!(t, Tap::WsResponse(_))).collect();
    assert_eq!(responses.len(), 3, "created, accepted and completed: {taps:#?}");
    assert_eq!(*responses[1], Tap::WsResponse(ACCEPTED.into()));
}

/// Go's reader waits inside `EmitWebSocketResponseEvent` while its writer goroutine keeps
/// taking client frames: a steer sent while an observer holds a delta reaches upstream,
/// and the held delta reaches the client only once its observation returns.
#[tokio::test]
async fn a_slow_observer_does_not_hold_client_input() {
    const STEER: &str = r#"{"type":"response.steer","previous_response_id":"r1","input":"steer while observed"}"#;
    const DELTA: &str = r#"{"type":"response.output_text.delta","delta":"held"}"#;
    /// Holds every delta until `release`, telling `held` when it starts.
    struct Holding {
        held: tokio::sync::Notify,
        release: tokio::sync::Semaphore,
    }
    impl cpa_core::exec::WebSocketObserver for Holding {
        fn response<'a>(
            &'a self,
            _: &'a str,
            _: &'a cpa_core::credential::Credential,
            _: &'a str,
            payload: &'a [u8],
        ) -> futures_util::future::BoxFuture<'a, ()> {
            Box::pin(async move {
                if crate::rawjson::get(&String::from_utf8_lossy(payload), "type").str() == "response.output_text.delta"
                {
                    self.held.notify_one();
                    let _ = self.release.acquire().await.unwrap();
                }
            })
        }
    }
    let (steered, delta) = (
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
    );
    let up = {
        let (steered, delta) = (steered.clone(), delta.clone());
        Scripted::start(move |mut c: Peer| {
            let (steered, delta) = (steered.clone(), delta.clone());
            async move {
                c.read().await;
                c.write(r#"{"type":"response.created","response":{"id":"r1","output":[]}}"#)
                    .await;
                delta.notified().await;
                c.write(DELTA).await;
                let got = c.read().await;
                if got != STEER {
                    c.error(format!("steer: {got}"));
                }
                steered.notify_one();
                c.write(r#"{"type":"response.steer.accepted","steer":{"id":"s1","previous_response_id":"r1"}}"#)
                    .await;
                c.write(r#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#)
                    .await;
                c.idle().await;
            }
        })
        .await
    };
    let client = Client::new("observer-duplex");
    let observer = Arc::new(Holding {
        held: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
    });
    let usage = cpa_core::exec::UsageSink::default().with_websocket(Some(observer.clone()));
    let mut stream = client
        .start_with(&up.url, r#"{"model":"gpt-6-astra","input":[]}"#, Format::Codex, usage)
        .await;
    let created = next(&mut stream).await.unwrap().unwrap();
    assert_eq!(get(&created, "type"), "response.created");
    delta.notify_one();
    tokio::time::timeout(Duration::from_secs(5), observer.held.notified())
        .await
        .expect("the delta was not observed");
    // Go's unbuffered send: the duplex takes the steer while the delta is observed.
    client.send(STEER).await;
    tokio::time::timeout(Duration::from_secs(5), steered.notified())
        .await
        .expect("the steer did not reach upstream while the delta was observed");
    let early = tokio::time::timeout(Duration::from_millis(200), stream.next()).await;
    assert!(
        early.is_err(),
        "the held delta was delivered before its observation returned: {early:?}"
    );
    observer.release.add_permits(10);
    let mut types = Vec::new();
    while let Some(chunk) = next(&mut stream).await {
        let kind = get(&chunk.unwrap(), "type");
        types.push(kind.clone());
        if kind == "response.completed" {
            break;
        }
    }
    assert_eq!(
        types,
        [
            "response.output_text.delta",
            "response.steer.accepted",
            "response.completed"
        ]
    );
    drop(stream);
    up.finish().await;
}

/// Go's writer only records its failure; the reader finishes the observer and sends the
/// message it holds, then reports the error. A writer-side failure (a create for another
/// model) raised while a delta is observed reaches the client after that delta.
#[tokio::test]
async fn a_failure_waits_for_the_observed_message() {
    const DELTA: &str = r#"{"type":"response.output_text.delta","delta":"held"}"#;
    struct Holding {
        held: tokio::sync::Notify,
        release: tokio::sync::Semaphore,
    }
    impl cpa_core::exec::WebSocketObserver for Holding {
        fn response<'a>(
            &'a self,
            _: &'a str,
            _: &'a cpa_core::credential::Credential,
            _: &'a str,
            payload: &'a [u8],
        ) -> futures_util::future::BoxFuture<'a, ()> {
            Box::pin(async move {
                if crate::rawjson::get(&String::from_utf8_lossy(payload), "type").str() == "response.output_text.delta"
                {
                    self.held.notify_one();
                    let _ = self.release.acquire().await.unwrap();
                }
            })
        }
    }
    let delta = Arc::new(tokio::sync::Notify::new());
    let up = {
        let delta = delta.clone();
        Scripted::start(move |mut c: Peer| {
            let delta = delta.clone();
            async move {
                c.read().await;
                c.write(r#"{"type":"response.created","response":{"id":"r1","output":[]}}"#)
                    .await;
                delta.notified().await;
                c.write(DELTA).await;
                c.idle().await;
            }
        })
        .await
    };
    let client = Client::new("observer-duplex-failure");
    let observer = Arc::new(Holding {
        held: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
    });
    let usage = cpa_core::exec::UsageSink::default().with_websocket(Some(observer.clone()));
    let mut stream = client
        .start_with(&up.url, r#"{"model":"gpt-6-astra","input":[]}"#, Format::Codex, usage)
        .await;
    let created = next(&mut stream).await.unwrap().unwrap();
    assert_eq!(get(&created, "type"), "response.created");
    delta.notify_one();
    tokio::time::timeout(Duration::from_secs(5), observer.held.notified())
        .await
        .expect("the delta was not observed");
    client
        .send(r#"{"type":"response.create","model":"another-model","input":[]}"#)
        .await;
    let early = tokio::time::timeout(Duration::from_millis(200), stream.next()).await;
    assert!(early.is_err(), "delivered before the observation returned: {early:?}");
    observer.release.add_permits(10);
    let held = next(&mut stream).await.expect("the held delta").unwrap();
    assert_eq!(get(&held, "type"), "response.output_text.delta");
    let error = next(&mut stream).await.expect("the deferred failure").unwrap_err();
    assert!(error.is_replay_required(), "{error}");
    assert!(next(&mut stream).await.is_none());
    drop(stream);
    up.finish().await;
}

/// A failure raised by queued-create progress while a message is observed stops client
/// input at once: no later client frame is consumed and nothing is output before the
/// observed message, which the failure then follows.
#[tokio::test]
async fn a_progress_failure_stops_client_input_during_an_observation() {
    const STEER: &str = r#"{"type":"response.steer","previous_response_id":"r1","input":"steer"}"#;
    const CREATED_R2: &str =
        r#"{"type":"response.created","response":{"id":"r2","previous_response_id":"r1","output":[]}}"#;
    struct Holding {
        held: tokio::sync::Notify,
        release: tokio::sync::Semaphore,
    }
    impl cpa_core::exec::WebSocketObserver for Holding {
        fn response<'a>(
            &'a self,
            _: &'a str,
            _: &'a cpa_core::credential::Credential,
            _: &'a str,
            payload: &'a [u8],
        ) -> futures_util::future::BoxFuture<'a, ()> {
            Box::pin(async move {
                if crate::rawjson::get(&String::from_utf8_lossy(payload), "response.id").str() == "r2" {
                    self.held.notify_one();
                    let _ = self.release.acquire().await.unwrap();
                }
            })
        }
    }
    let go = Arc::new(tokio::sync::Notify::new());
    let up = {
        let go = go.clone();
        Scripted::start(move |mut c: Peer| {
            let go = go.clone();
            async move {
                c.read().await;
                c.write(r#"{"type":"response.created","response":{"id":"r1","output":[]}}"#)
                    .await;
                let create = c.read().await;
                if get(&create, "type") != "response.create" {
                    c.error(format!("pending create: {create}"));
                }
                let steer = c.read().await;
                if steer != STEER {
                    c.error(format!("steer: {steer}"));
                }
                go.notified().await;
                c.write(r#"{"type":"response.steer.accepted","steer":{"id":"s1","previous_response_id":"r1"}}"#)
                    .await;
                c.write(CREATED_R2).await;
                c.idle().await;
            }
        })
        .await
    };
    let client = Client::new("observer-duplex-progress-failure");
    let observer = Arc::new(Holding {
        held: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
    });
    let usage = cpa_core::exec::UsageSink::default().with_websocket(Some(observer.clone()));
    let mut stream = client
        .start_with(&up.url, r#"{"model":"gpt-6-astra","input":[]}"#, Format::Codex, usage)
        .await;
    assert_eq!(
        get(&next(&mut stream).await.unwrap().unwrap(), "type"),
        "response.created"
    );
    // A create that stays pending, a steer on r1, and a create for another model queued
    // behind the steer.
    client.send(r#"{"type":"response.create","input":[]}"#).await;
    client.send(STEER).await;
    client
        .send(r#"{"type":"response.create","model":"another-model","input":[]}"#)
        .await;
    go.notify_one();
    let accepted = next(&mut stream).await.unwrap().unwrap();
    assert_eq!(get(&accepted, "type"), "response.steer.accepted");
    // r2's created event frees the queued create, whose progress fails while r2 is held.
    tokio::time::timeout(Duration::from_secs(5), observer.held.notified())
        .await
        .expect("r2 was not observed");
    client.input.send(Bytes::from_static(b"not json")).await.unwrap();
    let early = tokio::time::timeout(Duration::from_millis(300), stream.next()).await;
    assert!(early.is_err(), "output before the observation returned: {early:?}");
    assert_eq!(
        client.input.capacity(),
        0,
        "a client frame was consumed after the failure"
    );
    observer.release.add_permits(10);
    let created = next(&mut stream).await.expect("the held event").unwrap();
    assert_eq!(get(&created, "response.id"), "r2");
    let error = next(&mut stream).await.expect("the deferred failure").unwrap_err();
    assert!(error.is_replay_required(), "{error}");
    assert!(next(&mut stream).await.is_none());
    drop(stream);
    up.finish().await;
}

/// Sign in with ChatGPT on a steering socket: Go gave creates its SIWC WebSocket body
/// rules and stripped `configuration_update` items from steers. The `siwc` profile is
/// native now (#28), so the create and the steer go upstream as the client wrote them,
/// with only the create's `model` routed.
#[tokio::test]
async fn siwc_creates_and_steers_pass_unchanged() {
    const CREATE: &str = r#"{"type":"response.create","model":"gpt-6-astra","temperature":0.5,"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"type":"message","role":"user","content":"hi"}]}"#;
    const STEER: &str = r#"{"type":"response.steer","previous_response_id":"r1","temperature":1,"input":[{"type":"configuration_update","reasoning":{"effort":"high"}},{"type":"message","role":"user","content":"steer"}]}"#;
    let up = Scripted::start(|mut c: Peer| async move {
        let first = c.read().await;
        if first != CREATE {
            c.error(format!("first create changed: {first}"));
        }
        c.write(r#"{"type":"response.created","response":{"id":"r1","output":[]}}"#)
            .await;
        let steer = c.read().await;
        if steer != STEER {
            c.error(format!("steer changed: {steer}"));
        }
        c.write(r#"{"type":"response.steer.accepted","steer":{"id":"s1","previous_response_id":"r1"}}"#)
            .await;
        c.write(r#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#)
            .await;
        c.idle().await;
    })
    .await;
    let client = Client::new("siwc-steer");
    let mut credential = credential(&up.url);
    credential.metadata.insert("use_siwc".into(), true.into());
    credential
        .metadata
        .insert("siwc".into(), serde_json::json!({"access_token": "siwc-at"}));
    let mut req = request(CREATE);
    (req.model, req.requested_model, req.execution_session) =
        (MODEL.into(), MODEL.into(), Some(client.session.clone()));
    let session = ExecSession {
        id: client.session.clone(),
        continuation: false,
        lease: None,
    };
    let response = client
        .executor
        .execute_in_session(&credential, req, &Config::parse(STEERING).unwrap(), &session)
        .await
        .unwrap();
    let ResponseBody::Stream(mut stream) = response.body else {
        panic!("websocket turns stream");
    };
    while let Some(chunk) = next(&mut stream).await {
        let chunk = chunk.unwrap();
        match get(&chunk, "type").as_str() {
            "response.created" => client.send(STEER).await,
            "response.completed" => break,
            _ => {}
        }
    }
    drop(stream);
    up.finish().await;
}

/// Go's `TestCodexDuplexSteerPayloadBarrier` (868e137) ran the payload rules on a steer.
/// No Codex route applies payload rules (#28): with an override or a filter configured,
/// the steer goes upstream as the client wrote it.
#[tokio::test]
async fn steer_frames_ignore_the_payload_rules() {
    for filter in [false, true] {
        let up = Scripted::start(move |mut c: Peer| async move {
            c.read().await;
            c.write(r#"{"type":"response.created","response":{"id":"parent","output":[]}}"#)
                .await;
            let got = c.read().await;
            if got != r#"{"type":"response.steer","previous_response_id":"parent","input":"original"}"# {
                c.error(format!("filter={filter}: steer changed: {got}"));
            }
            c.idle().await;
        })
        .await;
        let rule = if filter {
            "  filter:\n    - models: [{name: \"*\", match: [{type: response.steer}]}]\n      params: [input, type]\n"
        } else {
            "  override:\n    - models: [{name: \"*\", match: [{type: response.steer}]}]\n      params: {input: configured, type: response.create}\n"
        };
        let config = format!("{STEERING}payload:\n{rule}");
        let client = Client::new(&format!("duplex-steer-rules-{filter}"));
        let mut stream = client
            .start_config(&up.url, r#"{"input":[]}"#, Format::Codex, Default::default(), &config)
            .await;
        let created = next(&mut stream).await.unwrap().unwrap();
        assert_eq!(get(&created, "type"), "response.created", "{created}");
        client
            .send(r#"{"type":"response.steer","previous_response_id":"parent","input":"original"}"#)
            .await;
        drop(stream);
        up.finish().await;
    }
}

// ------------------------------------------------------------------ interrupt acknowledgements

const DUPLEX_INTERRUPT: &str = r#"{"type":"response.interrupt","response_id":"r1","mode":"discard_partial_items"}"#;
const DUPLEX_ACCEPTED: &str = r#"{"type":"response.interrupt.accepted","response_id":"r1"}"#;

/// Current Codex may interrupt a response the duplex already finished (it had not read
/// the terminal event yet). The duplex owns the socket, so the interrupt goes upstream
/// and upstream's acknowledgement comes back on the stream.
#[tokio::test]
async fn an_interrupt_after_the_terminal_event_is_acknowledged_on_the_stream() {
    let up = Scripted::start(|mut c: Peer| async move {
        c.read().await;
        c.write(r#"{"type":"response.created","response":{"id":"r1","output":[]}}"#)
            .await;
        c.write(r#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#)
            .await;
        let got = c.read().await;
        if got != DUPLEX_INTERRUPT {
            c.error(format!("interrupt: {got}"));
        }
        c.write(DUPLEX_ACCEPTED).await;
        c.idle().await;
    })
    .await;
    let client = Client::new("duplex-late-interrupt");
    let mut stream = client.start(&up.url, r#"{"input":[]}"#, Format::Codex).await;
    for want in ["response.created", "response.completed"] {
        let event = next(&mut stream).await.unwrap().unwrap();
        assert_eq!(get(&event, "type"), want, "{event}");
    }
    let outcome = client
        .executor
        .interrupt_session(&client.session, DUPLEX_INTERRUPT.as_bytes(), |_| true)
        .await;
    assert!(outcome.is_ok(), "{outcome:?}");
    let ack = next(&mut stream).await.unwrap().unwrap();
    assert_eq!(ack, DUPLEX_ACCEPTED, "the acknowledgement is unchanged");
    drop(stream);
    up.finish().await;
}

/// An acknowledgement for a response nobody interrupted (another ID) never reaches the
/// stream; the interrupted response's own one does.
#[tokio::test]
async fn an_acknowledgement_for_another_response_never_reaches_the_stream() {
    let up = Scripted::start(|mut c: Peer| async move {
        c.read().await;
        c.write(r#"{"type":"response.created","response":{"id":"r1","output":[]}}"#).await;
        let got = c.read().await;
        if got != DUPLEX_INTERRUPT {
            c.error(format!("interrupt: {got}"));
        }
        c.write(r#"{"type":"response.interrupt.accepted","response_id":"stale"}"#).await;
        c.write(DUPLEX_ACCEPTED).await;
        c.write(r#"{"type":"response.incomplete","response":{"id":"r1","output":[],"incomplete_details":{"reason":"interrupted"}}}"#)
            .await;
        c.idle().await;
    })
    .await;
    let client = Client::new("duplex-stale-ack");
    let mut stream = client.start(&up.url, r#"{"input":[]}"#, Format::Codex).await;
    let created = next(&mut stream).await.unwrap().unwrap();
    assert_eq!(get(&created, "type"), "response.created", "{created}");
    let outcome = client
        .executor
        .interrupt_session(&client.session, DUPLEX_INTERRUPT.as_bytes(), |_| true)
        .await;
    assert!(outcome.is_ok(), "{outcome:?}");
    let ack = next(&mut stream).await.unwrap().unwrap();
    assert_eq!(ack, DUPLEX_ACCEPTED, "only r1's acknowledgement");
    let incomplete = next(&mut stream).await.unwrap().unwrap();
    assert_eq!(get(&incomplete, "type"), "response.incomplete", "{incomplete}");
    drop(stream);
    up.finish().await;
}

/// Contract probe: the later-frame reader uses the same boundary as the first turn.
/// Escaped routing strings are decoded only for inspection; unknown JSON stays raw.
#[tokio::test]
async fn later_creates_keep_unknown_bytes_after_a_boundary_refusal() {
    const CREATE: &str = r#"{ "type":"response.\u0063reate", "model":"gpt-6-\u0061stra", "previous_response_id":null, "future":{"n":1e400,"s":"\u0061"}, "input":[] }"#;
    let up = Scripted::start(|mut c: Peer| async move {
        c.read().await;
        c.write(r#"{"type":"response.created","response":{"id":"r1","output":[]}}"#)
            .await;
        c.write(r#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#)
            .await;
        let create = c.read().await;
        if create != CREATE {
            c.error(format!("later create changed: {create}"));
        }
        c.write(r#"{"type":"response.created","response":{"id":"r2","output":[]}}"#)
            .await;
        c.write(r#"{"type":"response.completed","response":{"id":"r2","output":[]}}"#)
            .await;
        c.idle().await;
    })
    .await;
    let client = Client::new("raw-later-create");
    let mut stream = client
        .start(&up.url, r#"{"model":"gpt-6-astra","input":[]}"#, Format::Codex)
        .await;
    let mut refused = false;
    let mut completed = false;
    while let Some(chunk) = next(&mut stream).await {
        let chunk = chunk.unwrap();
        match (get(&chunk, "type").as_str(), get(&chunk, "response.id").as_str()) {
            ("response.completed", "r1") => {
                client
                    .send(r#"{"type":"response.create","model":"gpt-6-astra","mod\u0065l":"duplicate"}"#)
                    .await
            }
            ("error", _) => {
                assert!(get(&chunk, "error.message").contains("repeats an object key"));
                refused = true;
                client.send(CREATE).await;
            }
            ("response.completed", "r2") => {
                completed = true;
                break;
            }
            _ => {}
        }
    }
    assert!(refused && completed);
    drop(stream);
    up.finish().await;
}

/// #24 (refs #3): on a duplex connection, an explicit later `response.create` is a client
/// request, so it gets the socket's upgrade `openai-model` alone, right before the
/// `response.created` that starts it. Turn state never repeats. An automatic successor is
/// upstream's own response, not a request, and a create rejected before it started has
/// no response: neither gets metadata. The bootstrap create's metadata is the server's
/// (from the returned handshake), so the stream adds none for it. Nothing extra goes
/// upstream.
#[tokio::test]
async fn later_creates_get_the_sockets_model_before_they_start() {
    const LATER: &str = r#"{"type":"response.metadata","headers":{"openai-model":"gpt-6-astra-served"}}"#;
    let response = |kind: &str, id: &str, parent: &str| {
        format!(r#"{{"type":"{kind}","response":{{"id":"{id}","previous_response_id":"{parent}","output":[]}}}}"#)
    };
    let failed = r#"{"type":"response.failed","response":{"id":"rejected","output":[],"error":{"type":"invalid_request_error","message":"rejected"}}}"#;
    let accepted = r#"{"type":"response.steer.accepted","steer":{"id":"s1","previous_response_id":"first"}}"#;
    let received: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = received.clone();
    let up = Scripted::start_with_handshake(
        &[
            ("x-codex-turn-state", "ts-open"),
            ("openai-model", "gpt-6-astra-served"),
        ],
        move |mut c: Peer| {
            let log = log.clone();
            async move {
                let read = async |c: &mut Peer| {
                    let frame = c.read().await;
                    log.lock().unwrap().push(frame.clone());
                    frame
                };
                read(&mut c).await;
                c.write(&response("response.created", "first", "")).await;
                let steer = read(&mut c).await;
                if get(&steer, "type") != "response.steer" {
                    c.error(format!("expected steering: {steer}"));
                    return;
                }
                c.write(accepted).await;
                c.write(&response("response.completed", "first", "")).await;
                c.write(&response("response.created", "automatic", "first")).await;
                c.write(&response("response.completed", "automatic", "first")).await;
                read(&mut c).await;
                read(&mut c).await;
                c.write(failed).await;
                c.write(&response("response.created", "good", "")).await;
                c.write(&response("response.completed", "good", "")).await;
                c.idle().await;
            }
        },
    )
    .await;
    let client = Client::new("later-create-metadata");
    let create = |key: &str| {
        format!(r#"{{"type":"response.create","model":"gpt-6-astra","prompt_cache_key":"{key}","input":[]}}"#)
    };
    let steer = r#"{"type":"response.steer","previous_response_id":"first","input":"more"}"#;
    let mut stream = client.start(&up.url, &create("first"), Format::OpenAIResponse).await;
    let mut delivered = Vec::new();
    while let Some(chunk) = next(&mut stream).await {
        let chunk = chunk.expect("no stream failure");
        let (kind, id) = (get(&chunk, "type"), get(&chunk, "response.id"));
        delivered.push(chunk);
        if kind == "response.created" && id == "first" {
            client.send(steer).await;
        }
        if kind == "response.completed" && id == "automatic" {
            client.send(&create("rejected")).await;
            client.send(&create("good")).await;
        }
        if kind == "response.completed" && id == "good" {
            break;
        }
    }
    drop(stream);
    assert_eq!(
        delivered,
        [
            response("response.created", "first", ""),
            accepted.to_owned(),
            response("response.completed", "first", ""),
            response("response.created", "automatic", "first"),
            response("response.completed", "automatic", "first"),
            failed.to_owned(),
            LATER.to_owned(),
            response("response.created", "good", ""),
            response("response.completed", "good", ""),
        ]
    );
    up.finish().await;
    assert_eq!(
        *received.lock().unwrap(),
        [create("first"), steer.to_owned(), create("rejected"), create("good")],
        "the client's frames only, as sent"
    );
}

// ------------------------------------------------------------------ usage records (#25)

/// Usage records as a steering connection makes them: every settlement in order, as the
/// record's label and status (200 for a publish). A record that settles twice shows
/// twice. The detached bootstrap record is `bootstrap`, a create's record its
/// `service_tier`, and an automatic successor's its parent's label plus `+auto`.
#[derive(Clone)]
struct Ledger {
    label: String,
    settled: Arc<Mutex<Vec<(String, u16)>>>,
}

impl Ledger {
    fn attempt() -> Self {
        Self {
            label: "attempt".into(),
            settled: Arc::default(),
        }
    }

    fn record(&self, label: String) -> Option<Arc<dyn cpa_core::exec::UsageObserver>> {
        Some(Arc::new(Self {
            label,
            settled: self.settled.clone(),
        }))
    }

    fn settle(&self, status: u16) {
        self.settled.lock().unwrap().push((self.label.clone(), status));
    }

    /// The settlements so far, sorted: which record settles first when the connection
    /// drops several is not the point.
    fn sorted(&self) -> Vec<(String, u16)> {
        let mut settled = self.settled.lock().unwrap().clone();
        settled.sort();
        settled
    }
}

impl cpa_core::exec::UsageObserver for Ledger {
    fn response_body(&self, _: Format, _: &[u8]) {}
    fn response_line(&self, _: Format, _: &[u8]) {}
    fn request(&self, _: Format, _: &[u8]) {}
    fn publish(&self) {
        self.settle(200);
    }
    fn publish_failure(&self, status: u16, _: &str) {
        self.settle(status);
    }
    fn detach(&self) -> Option<Arc<dyn cpa_core::exec::UsageObserver>> {
        self.record("bootstrap".into())
    }
    fn successor(&self, request: Option<&[u8]>) -> Option<Arc<dyn cpa_core::exec::UsageObserver>> {
        self.record(match request {
            Some(frame) => get(&String::from_utf8_lossy(frame), "service_tier"),
            None => format!("{}+auto", self.label),
        })
    }
}

fn tiered(tier: &str) -> String {
    format!(r#"{{"type":"response.create","model":"gpt-6-astra","service_tier":"{tier}","input":[]}}"#)
}

fn event(kind: &str, id: &str) -> String {
    format!(r#"{{"type":"{kind}","response":{{"id":"{id}","output":[]}}}}"#)
}

/// The steering stream has ended and its records settled: the duplex released the
/// client queue, which it holds until it is gone (on this single-threaded runtime, its
/// drop has then finished).
async fn duplex_gone(client: &Client) {
    tokio::time::timeout(Duration::from_secs(5), client.frames.clone().lock_owned())
        .await
        .expect("the duplex released the client queue");
}

/// A failure that names a response the socket saw start settles that response's record,
/// even while a later response runs and a create waits: the waiting create keeps its
/// own record and stays an explicit request (its metadata precedes its
/// `response.created`). A failure naming a settled response settles nothing more. One
/// naming a response the socket never saw start is still the rejection of the oldest
/// waiting create, and the responses running keep running. Every event reaches the
/// client as sent.
#[tokio::test]
async fn a_failure_settles_the_response_it_names() {
    const LATER: &str = r#"{"type":"response.metadata","headers":{"openai-model":"gpt-6-astra-served"}}"#;
    const FAILED_R1: &str = r#"{"type":"response.failed","response":{"id":"r1","output":[],"error":{"type":"invalid_request_error","message":"r1 failed"}}}"#;
    const ERROR_R1: &str = r#"{"type":"error","status":400,"response_id":"r1","error":{"type":"invalid_request_error","message":"r1 rejected"}}"#;
    const UNSTARTED: &str = r#"{"type":"error","status":400,"response_id":"never-started","error":{"type":"invalid_request_error","message":"rejected before it started"}}"#;
    // (case, r1 completes before r2 starts, failure, the create waiting is rejected)
    for (name, settled_first, failure, rejects) in [
        ("older open, response.id", false, FAILED_R1, false),
        ("older open, response_id", false, ERROR_R1, false),
        ("settled", true, FAILED_R1, false),
        ("never started", false, UNSTARTED, true),
    ] {
        let up = Scripted::start_with_handshake(
            &[("openai-model", "gpt-6-astra-served")],
            move |mut c: Peer| async move {
                c.read().await;
                c.write(&event("response.created", "r1")).await;
                if settled_first {
                    c.write(&event("response.completed", "r1")).await;
                }
                c.read().await;
                c.write(&event("response.created", "r2")).await;
                // The third create is upstream, not started: only then does the failure come.
                c.read().await;
                c.write(failure).await;
                c.write(&event("response.completed", "r2")).await;
                if rejects {
                    c.write(&event("response.completed", "r1")).await;
                } else {
                    c.write(&event("response.created", "r3")).await;
                    c.write(&event("response.completed", "r3")).await;
                }
                c.idle().await;
            },
        )
        .await;
        let client = Client::new(&format!("named-failure-{name}"));
        let ledger = Ledger::attempt();
        let usage = cpa_core::exec::UsageSink::new(Arc::new(ledger.clone()));
        let mut stream = client
            .start_with(&up.url, &tiered("auto"), Format::OpenAIResponse, usage)
            .await;
        let last = if rejects {
            ("response.completed", "r1")
        } else {
            ("response.completed", "r3")
        };
        let mut delivered = Vec::new();
        while let Some(chunk) = next(&mut stream).await {
            let chunk = chunk.unwrap_or_else(|e| panic!("{name}: {e}"));
            let (kind, id) = (get(&chunk, "type"), get(&chunk, "response.id"));
            delivered.push(chunk);
            match (kind.as_str(), id.as_str()) {
                ("response.created", "r1") => client.send(&tiered("flex")).await,
                ("response.created", "r2") => client.send(&tiered("priority")).await,
                _ => {}
            }
            if (kind.as_str(), id.as_str()) == last {
                break;
            }
        }
        let mut want = vec![event("response.created", "r1")];
        if settled_first {
            want.push(event("response.completed", "r1"));
        }
        want.extend([LATER.to_owned(), event("response.created", "r2"), failure.to_owned()]);
        want.push(event("response.completed", "r2"));
        if rejects {
            want.push(event("response.completed", "r1"));
        } else {
            want.extend([
                LATER.to_owned(),
                event("response.created", "r3"),
                event("response.completed", "r3"),
            ]);
        }
        assert_eq!(
            delivered, want,
            "{name}: every event as sent, metadata before explicit creates"
        );
        drop(stream);
        duplex_gone(&client).await;
        let want: Vec<(String, u16)> = match name {
            "settled" => vec![("bootstrap", 200), ("flex", 200), ("priority", 200)],
            "never started" => vec![("bootstrap", 200), ("flex", 200), ("priority", 400)],
            _ => vec![("bootstrap", 400), ("flex", 200), ("priority", 200)],
        }
        .into_iter()
        .map(|(label, status)| (label.to_owned(), status))
        .collect();
        assert_eq!(ledger.sorted(), want, "{name}: one settlement per record, its own");
        up.finish().await;
    }
}

/// An upstream outcome whose observation already started when the consumer left still
/// settles its own record, once, as it would have with the consumer there: a terminal
/// as a success, a failure or a rejection as that failure, an account failure every
/// record. Only what it does not settle is the client's cancellation (499), as is a
/// failure no request can be assigned. Nothing is delivered after the consumer left.
#[tokio::test]
async fn an_outcome_observed_when_the_consumer_leaves_still_settles() {
    /// Holds every observation but `response.created`'s until `release`, telling `held`
    /// when it starts, and `repolled` when polled again after `left` was set.
    struct Holding {
        held: tokio::sync::Notify,
        release: tokio::sync::Semaphore,
        left: std::sync::atomic::AtomicBool,
        repolled: tokio::sync::Notify,
    }
    impl cpa_core::exec::WebSocketObserver for Holding {
        fn response<'a>(
            &'a self,
            _: &'a str,
            _: &'a cpa_core::credential::Credential,
            _: &'a str,
            payload: &'a [u8],
        ) -> futures_util::future::BoxFuture<'a, ()> {
            Box::pin(async move {
                if crate::rawjson::get(&String::from_utf8_lossy(payload), "type").str() == "response.created" {
                    return;
                }
                self.held.notify_one();
                let mut acquire = std::pin::pin!(self.release.acquire());
                std::future::poll_fn(|cx| {
                    if self.left.load(Ordering::SeqCst) {
                        self.repolled.notify_one();
                    }
                    acquire.as_mut().poll(cx).map(|permit| permit.unwrap().forget())
                })
                .await;
            })
        }
    }
    let statuses = |pairs: &[(&str, u16)]| -> Vec<(String, u16)> {
        let mut v: Vec<(String, u16)> = pairs.iter().map(|(l, s)| ((*l).to_owned(), *s)).collect();
        v.sort();
        v
    };
    // r1 runs (the detached bootstrap record) and a `flex` create waits upstream when the
    // held message arrives.
    for (name, held, want) in [
        (
            "completed",
            r#"{"type":"response.completed","response":{"id":"r1","output":[],"usage":{"input_tokens":5,"output_tokens":1,"total_tokens":6}}}"#,
            statuses(&[("bootstrap", 200), ("flex", 499)]),
        ),
        (
            "failed",
            r#"{"type":"response.failed","response":{"id":"r1","output":[],"error":{"type":"invalid_request_error","message":"failed"}}}"#,
            statuses(&[("bootstrap", 400), ("flex", 499)]),
        ),
        (
            "error naming r1",
            r#"{"type":"error","status":400,"response_id":"r1","error":{"type":"invalid_request_error","message":"rejected"}}"#,
            statuses(&[("bootstrap", 400), ("flex", 499)]),
        ),
        (
            "rejected create",
            r#"{"type":"error","status":400,"response_id":"never-started","error":{"type":"invalid_request_error","message":"rejected"}}"#,
            statuses(&[("bootstrap", 499), ("flex", 400)]),
        ),
        (
            "ambiguous",
            r#"{"type":"error","status":400,"error":{"type":"invalid_request_error","message":"whose?"}}"#,
            statuses(&[("bootstrap", 499), ("flex", 499)]),
        ),
        (
            "account failure",
            r#"{"type":"error","status":401,"error":{"type":"authentication_error","message":"expired"}}"#,
            statuses(&[("bootstrap", 401), ("flex", 401)]),
        ),
    ] {
        let up = Scripted::start(move |mut c: Peer| async move {
            c.read().await;
            c.write(&event("response.created", "r1")).await;
            c.read().await;
            c.write(held).await;
            c.idle().await;
        })
        .await;
        let client = Client::new(&format!("observed-outcome-{name}"));
        let observer = Arc::new(Holding {
            held: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
            left: Default::default(),
            repolled: tokio::sync::Notify::new(),
        });
        let ledger = Ledger::attempt();
        let usage = cpa_core::exec::UsageSink::new(Arc::new(ledger.clone())).with_websocket(Some(observer.clone()));
        let mut stream = client
            .start_with(&up.url, &tiered("auto"), Format::OpenAIResponse, usage)
            .await;
        let created = next(&mut stream).await.unwrap().unwrap();
        assert_eq!(created, event("response.created", "r1"), "{name}");
        client.send(&tiered("flex")).await;
        tokio::time::timeout(Duration::from_secs(5), observer.held.notified())
            .await
            .unwrap_or_else(|_| panic!("{name}: the message was not observed"));
        assert!(
            ledger.sorted().is_empty(),
            "{name}: settled before its observation returned"
        );
        // The consumer leaves while the observation runs. A poll of it after that shows
        // the duplex saw the consumer gone: its stream no longer delivers.
        drop(stream);
        observer.left.store(true, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(5), observer.repolled.notified())
            .await
            .unwrap_or_else(|_| panic!("{name}: the observation was not polled after the consumer left"));
        observer.release.add_permits(1);
        duplex_gone(&client).await;
        assert_eq!(ledger.sorted(), want, "{name}");
        up.finish().await;
    }
}

// ------------------------------------------------------------------ bounded queues (#24)

/// #24, `docs/codex/CONTRACT.md` "Socket and account ownership": the duplex keeps at most
/// 16 creates outstanding upstream, and holds at most 16 creates back while a steer is
/// unacknowledged. One more ends the connection with a local, request-scoped error
/// instead of growing a queue. That create never reaches upstream, and neither does a
/// held create. Barriers on what upstream read, not quiet periods, order each step.
#[tokio::test]
async fn outstanding_and_held_creates_stay_bounded() {
    const BOUND: usize = 16;
    const STEER: &str = r#"{"type":"response.steer","previous_response_id":"r1","input":"hold"}"#;
    let create = |n: usize| {
        format!(
            r#"{{"type":"response.create","input":[{{"type":"message","role":"user","content":[{{"type":"input_text","text":"turn {n}"}}]}}]}}"#
        )
    };
    for held in [false, true] {
        let received: Arc<Mutex<Vec<String>>> = Arc::default();
        let read_all = Arc::new(tokio::sync::Notify::new());
        let (log, signal) = (received.clone(), read_all.clone());
        let up = Scripted::start(move |mut c: Peer| {
            let (log, signal) = (log.clone(), signal.clone());
            async move {
                c.read().await;
                c.write(r#"{"type":"response.created","response":{"id":"r1","output":[]}}"#)
                    .await;
                if !held {
                    c.write(r#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#)
                        .await;
                }
                // Outstanding: the 16 creates. Held: only the steer, never acknowledged.
                let expected = if held { 1 } else { BOUND };
                loop {
                    match tokio::time::timeout(Duration::from_secs(10), c.socket.recv()).await {
                        Ok(Some(Ok(AxMessage::Text(text)))) => {
                            let mut log = log.lock().unwrap();
                            log.push(text.as_str().to_owned());
                            if log.len() == expected {
                                signal.notify_one();
                            }
                        }
                        Ok(Some(Ok(AxMessage::Ping(_) | AxMessage::Pong(_)))) => {}
                        _ => break,
                    }
                }
            }
        })
        .await;
        let client = Client::new(&format!("duplex-bounded-{held}"));
        let mut stream = client.start(&up.url, r#"{"input":[]}"#, Format::Codex).await;
        let created = next(&mut stream).await.unwrap().unwrap();
        assert_eq!(get(&created, "type"), "response.created", "held={held}");
        if held {
            client.send(STEER).await;
            tokio::time::timeout(Duration::from_secs(5), read_all.notified())
                .await
                .unwrap_or_else(|_| panic!("held={held}: the steer never reached upstream"));
            for n in 0..BOUND {
                client.send(&create(n)).await;
            }
        } else {
            let completed = next(&mut stream).await.unwrap().unwrap();
            assert_eq!(get(&completed, "type"), "response.completed");
            for n in 0..BOUND {
                client.send(&create(n)).await;
            }
            tokio::time::timeout(Duration::from_secs(5), read_all.notified())
                .await
                .unwrap_or_else(|_| panic!("held={held}: the outstanding creates never reached upstream"));
        }
        client.send(&create(BOUND)).await;
        let error = loop {
            match next(&mut stream).await {
                Some(Ok(event)) => panic!("held={held}: unexpected event {event}"),
                Some(Err(error)) => break error,
                None => panic!("held={held}: the stream ended without the bound's error"),
            }
        };
        assert!(
            error
                .to_string()
                .contains("too many outstanding response.create requests"),
            "held={held}: {error}"
        );
        assert_eq!(error.scope, FailureScope::Request, "held={held}: {error}");
        assert!(next(&mut stream).await.is_none(), "held={held}: the stream ends");
        drop(stream);
        up.finish().await;
        let routed = |n: usize| format!(r#"{{"model":"{MODEL}",{}"#, &create(n)[1..]);
        let want: Vec<String> = if held {
            vec![STEER.to_owned()]
        } else {
            (0..BOUND).map(routed).collect()
        };
        assert_eq!(
            *received.lock().unwrap(),
            want,
            "held={held}: nothing past the bound, and no held create, went upstream"
        );
    }
}

/// #24, `docs/codex/CONTRACT.md` "Socket and account ownership": the duplex reads client
/// frames only while its write queue has room, so an upstream that stops reading
/// backpressures the client. The frames wait in the client's own bounded queue; the duplex
/// neither fails with a full write queue nor buffers them without bound. Once upstream reads
/// again, every frame arrives, in order and unchanged, and the stream reports no error.
/// The 64 steers of 256 KiB (16 MiB) exceed the 16-frame write queue plus the loopback
/// socket buffers, so the client must block before it has sent them all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_upstream_backpressures_client_frames() {
    const STEERS: usize = 64;
    let pad = "x".repeat(256 * 1024);
    let steer =
        move |n: usize| format!(r#"{{"type":"response.steer","previous_response_id":"r1","input":"{n:03} {pad}"}}"#);
    let received: Arc<Mutex<Vec<String>>> = Arc::default();
    let release = Arc::new(tokio::sync::Notify::new());
    let all_read = Arc::new(tokio::sync::Notify::new());
    let (log, resume, done) = (received.clone(), release.clone(), all_read.clone());
    let up = Scripted::start(move |mut c: Peer| {
        let (log, resume, done) = (log.clone(), resume.clone(), done.clone());
        async move {
            c.read().await;
            c.write(r#"{"type":"response.created","response":{"id":"r1","output":[]}}"#)
                .await;
            // Stop reading: the proxy's writes fill the socket buffers, then its queue.
            resume.notified().await;
            for _ in 0..STEERS {
                let frame = c.read().await;
                log.lock().unwrap().push(frame);
            }
            done.notify_one();
            c.idle().await;
        }
    })
    .await;
    let client = Client::new("duplex-backpressure");
    let mut stream = client.start(&up.url, r#"{"input":[]}"#, Format::Codex).await;
    let created = next(&mut stream).await.unwrap().unwrap();
    assert_eq!(get(&created, "type"), "response.created");

    let sent = Arc::new(AtomicUsize::new(0));
    let (input, counter, frames) = (client.input.clone(), sent.clone(), steer.clone());
    let sender = tokio::spawn(async move {
        for n in 0..STEERS {
            input.send(Bytes::from(frames(n))).await.unwrap();
            counter.fetch_add(1, Ordering::SeqCst);
        }
    });
    // The client blocks while upstream does not read: its progress stops short of the end.
    let mut last = usize::MAX;
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let now = sent.load(Ordering::SeqCst);
        assert!(now < STEERS, "the duplex took every frame while upstream was stalled");
        if now == last {
            break;
        }
        last = now;
    }
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(10), all_read.notified())
        .await
        .expect("upstream read again");
    let want: Vec<String> = (0..STEERS).map(steer).collect();
    assert!(
        *received.lock().unwrap() == want,
        "the client's steers, in order and unchanged"
    );
    tokio::time::timeout(Duration::from_secs(10), sender)
        .await
        .expect("the client's blocked sends complete")
        .unwrap();
    // Nothing failed meanwhile: no full write queue, no local error waits on the stream.
    assert!(
        futures_util::FutureExt::now_or_never(stream.next()).is_none(),
        "the stream stays open with nothing to deliver"
    );
    drop(stream);
    up.finish().await;
}
