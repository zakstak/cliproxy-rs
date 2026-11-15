//! Local scripted upstreams and typed test observers.

use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use cpa_core::exec::{Caller, ExecRequest, Operation};
use cpa_core::format::Format;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Captured {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// One scripted upstream answer: status, ordered headers, body.
/// Status, headers, body and `missing`: bytes the Content-Length promises beyond the body,
/// after which the connection closes (a read error).
type Scripted = (u16, Vec<(String, String)>, Vec<u8>, usize);

/// Raw HTTP/1.1 capture server answering with scripted responses in order.
pub(crate) struct Mock {
    pub url: String,
    captured: Arc<Mutex<Vec<Captured>>>,
}

impl Mock {
    pub(crate) async fn start(responses: &Value) -> Self {
        Self::start_replacing(responses, None).await
    }

    /// Like [`Mock::start`], with every occurrence of `peer_origin` in the scripted bodies
    /// replaced by this mock's URL (scripted peers that answer with their own URL).
    pub(crate) async fn start_replacing(responses: &Value, peer_origin: Option<&str>) -> Self {
        let mut scripted: Vec<Scripted> = responses
            .as_array()
            .map(|list| {
                list.iter()
                    .map(|r| {
                        let body = match r["body_b64"].as_str() {
                            Some(b64) if !b64.is_empty() => STANDARD.decode(b64).unwrap(),
                            _ => r["body"].as_str().unwrap_or_default().as_bytes().to_vec(),
                        };
                        let headers = r["headers"]
                            .as_array()
                            .map(|hs| {
                                hs.iter()
                                    .map(|h| (h[0].as_str().unwrap().to_owned(), h[1].as_str().unwrap().to_owned()))
                                    .collect()
                            })
                            .unwrap_or_default();
                        let missing = r["missing"].as_u64().unwrap_or(0) as usize;
                        (r["status"].as_u64().unwrap() as u16, headers, body, missing)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        if let Some(origin) = peer_origin {
            for (_, _, body, _) in &mut scripted {
                if let Ok(text) = std::str::from_utf8(body) {
                    *body = text.replace(origin, &url).into_bytes();
                }
            }
        }
        let captured: Arc<Mutex<Vec<Captured>>> = Arc::default();
        let queue = Arc::new(Mutex::new(std::collections::VecDeque::from(scripted)));
        let sink = captured.clone();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let sink = sink.clone();
                let queue = queue.clone();
                tokio::spawn(async move {
                    let (read, mut write) = socket.into_split();
                    let mut reader = BufReader::new(read);
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let mut parts = line.trim_end().splitn(3, ' ');
                        let method = parts.next().unwrap_or_default().to_owned();
                        let target = parts.next().unwrap_or_default().to_owned();
                        let mut headers = Vec::new();
                        let (mut length, mut chunked) = (0usize, false);
                        loop {
                            let mut h = String::new();
                            if reader.read_line(&mut h).await.unwrap_or(0) == 0 {
                                return;
                            }
                            let h = h.trim_end_matches(['\r', '\n']);
                            if h.is_empty() {
                                break;
                            }
                            let (name, value) = h.split_once(':').unwrap_or((h, ""));
                            let value = value.trim().to_owned();
                            if name.eq_ignore_ascii_case("content-length") {
                                length = value.parse().unwrap_or(0);
                            }
                            if name.eq_ignore_ascii_case("transfer-encoding") {
                                chunked = value.eq_ignore_ascii_case("chunked");
                            }
                            headers.push((name.to_owned(), value));
                        }
                        let mut body = Vec::new();
                        if chunked {
                            loop {
                                let mut size = String::new();
                                reader.read_line(&mut size).await.unwrap();
                                let n = usize::from_str_radix(size.trim().split(';').next().unwrap(), 16).unwrap();
                                let mut chunk = vec![0; n + 2];
                                reader.read_exact(&mut chunk).await.unwrap();
                                if n == 0 {
                                    break;
                                }
                                body.extend_from_slice(&chunk[..n]);
                            }
                        } else {
                            body.resize(length, 0);
                            reader.read_exact(&mut body).await.unwrap();
                        }
                        sink.lock().unwrap().push(Captured {
                            method,
                            target,
                            headers,
                            body,
                        });
                        let (status, headers, body, missing) = queue.lock().unwrap().pop_front().unwrap_or((
                            500,
                            Vec::new(),
                            b"no scripted response".to_vec(),
                            0,
                        ));
                        let mut out = format!("HTTP/1.1 {status} {}\r\n", reason(status)).into_bytes();
                        for (n, v) in &headers {
                            out.extend_from_slice(format!("{n}: {v}\r\n").as_bytes());
                        }
                        out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len() + missing).as_bytes());
                        out.extend_from_slice(&body);
                        if write.write_all(&out).await.is_err() || missing > 0 {
                            return;
                        }
                    }
                });
            }
        });
        Self { url, captured }
    }

    pub(crate) fn captured(&self) -> Vec<Captured> {
        self.captured.lock().unwrap().clone()
    }
}

fn reason(status: u16) -> &'static str {
    http::StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason())
        .unwrap_or("Unknown")
}

pub(crate) fn format(name: &str) -> Format {
    Format::parse(name).unwrap_or_else(|| panic!("format {name}"))
}

/// Build a client request from input-only test data.
pub(crate) fn request(fixture: &Value, principal: &str) -> ExecRequest {
    let r = &fixture["request"];
    let body = Bytes::from(r["body"].as_str().unwrap().to_owned());
    let source = format(r["source"].as_str().unwrap());
    let mut headers = http::HeaderMap::new();
    if let Some(list) = r["headers"].as_array() {
        for h in list {
            headers.append(
                http::HeaderName::from_bytes(h[0].as_str().unwrap().as_bytes()).unwrap(),
                h[1].as_str().unwrap().parse().unwrap(),
            );
        }
    }
    ExecRequest {
        operation: if r["count"].as_bool() == Some(true) {
            Operation::CountTokens
        } else {
            Operation::Generate
        },
        source_format: source,
        response_format: source,
        requested_model: r["model"].as_str().unwrap().into(),
        model: r["model"].as_str().unwrap().into(),
        original_body: body.clone(),
        body,
        stream: r["stream"].as_bool().unwrap_or(false),
        alt: r["alt"].as_str().map(str::to_owned),
        session: None,
        execution_session: None,
        derived_session: None,
        resolved_model: None,
        usage: Default::default(),
        request_path: String::new(),
        headers,
        caller: Caller {
            principal: principal.into(),
            source: "authorization",
            provider: String::new(),
        },
    }
}

/// The failure status and body the server records for an executor error when the
/// executor publishes nothing itself (cpa_server::classify `go_status`, `error_text`):
/// transport faults carry no status, as Go's plain errors.
/// A translator that echoes events and rejects apply_patch input on a marked line.
pub(crate) struct PatchProbe {
    pub failed: bool,
    pub fail_on_finalize: bool,
    pub log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl cpa_translate::StreamTranslator for PatchProbe {
    fn event(&mut self, event: &[u8]) -> Result<Vec<Bytes>, cpa_translate::Error> {
        let event = String::from_utf8_lossy(event).into_owned();
        self.log.lock().unwrap().push(format!("event {event}"));
        if event.contains("BAD_PATCH") {
            self.failed = true;
        }
        Ok(vec![Bytes::from(format!("frame {event}"))])
    }
    fn finish(&mut self) -> Result<Vec<Bytes>, cpa_translate::Error> {
        self.log.lock().unwrap().push("finish".into());
        Ok(vec![Bytes::from_static(b"finished")])
    }
    fn flush_frames(&mut self) -> Vec<Bytes> {
        vec![Bytes::from_static(b"flushed")]
    }
    fn tool_input_failed(&self) -> bool {
        self.failed
    }
    fn finalize_tool_input(&mut self) -> Vec<Bytes> {
        self.log.lock().unwrap().push("finalize".into());
        if self.fail_on_finalize {
            self.failed = true;
            return vec![Bytes::from_static(b"response.failed")];
        }
        vec![]
    }
}

/// Captures every tracing event emitted on the current thread while installed (no
/// subscriber crate needed): each event's fields, formatted.
#[derive(Clone, Default)]
pub(crate) struct LogCapture(pub Arc<Mutex<Vec<String>>>);

/// An installed [`LogCapture`]; capturing ends when it is dropped.
pub(crate) struct LogCaptureGuard {
    _default: tracing::subscriber::DefaultGuard,
    _second: tracing::Dispatch,
}

impl LogCapture {
    /// Installs the capture as this thread's default subscriber. With a single live
    /// dispatcher, tracing-core computes a callsite's cached interest from the
    /// registering thread's default alone (`Rebuilder::JustOne`), so a parallel test
    /// that hits the same callsite first, with no subscriber, caches `never` for
    /// everyone and the event never reaches this capture. A second live dispatcher
    /// makes every registration consult all live dispatchers, this one included.
    pub(crate) fn install(&self) -> LogCaptureGuard {
        let second = tracing::Dispatch::new(LogCapture::default());
        LogCaptureGuard {
            _default: tracing::subscriber::set_default(self.clone()),
            _second: second,
        }
    }
}

impl tracing::Subscriber for LogCapture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        struct Fields(String);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.push_str(&format!("{}={value:?} ", field.name()));
            }
        }
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// A `UsageSink` call other than a reported payload (Server 13's additions).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum UsageEvent {
    RequestFor(String, Vec<u8>),
    UpstreamModel(String),
    ResponseModel(String),
    RoundTripStarted,
    FirstByte,
    Token(bool),
    Publish,
    PublishFailure(u16, String),
    /// `publish_failure_with_usage`: the failure keeps the usage reported so far.
    PublishFailureWithUsage(u16, String),
    UsageRequired,
    Discard,
}

/// Records what an executor reports to its usage sink: the payloads (Server 6) and,
/// in order, every other call.
#[derive(Default)]
pub(crate) struct UsageLog(Mutex<Vec<(&'static str, Format, Vec<u8>)>>, Mutex<Vec<UsageEvent>>);

impl cpa_core::exec::UsageObserver for UsageLog {
    fn response_body(&self, format: Format, body: &[u8]) {
        self.0.lock().unwrap().push(("body", format, body.to_vec()));
    }
    fn response_line(&self, format: Format, line: &[u8]) {
        self.0.lock().unwrap().push(("line", format, line.to_vec()));
    }
    fn request(&self, format: Format, payload: &[u8]) {
        self.0.lock().unwrap().push(("request", format, payload.to_vec()));
    }
    fn request_for(&self, identifier: &str, payload: &[u8]) {
        self.event(UsageEvent::RequestFor(identifier.into(), payload.to_vec()));
    }
    fn upstream_model(&self, model: &str) {
        self.event(UsageEvent::UpstreamModel(model.into()));
    }
    fn response_model(&self, model: &str) {
        self.event(UsageEvent::ResponseModel(model.into()));
    }
    fn round_trip_started(&self) {
        self.event(UsageEvent::RoundTripStarted);
    }
    fn first_byte(&self) {
        self.event(UsageEvent::FirstByte);
    }
    fn token_event(&self, is_token: bool) {
        self.event(UsageEvent::Token(is_token));
    }
    fn publish(&self) {
        self.event(UsageEvent::Publish);
    }
    fn publish_failure(&self, status: u16, body: &str) {
        self.event(UsageEvent::PublishFailure(status, body.into()));
    }
    fn publish_failure_with_usage(&self, status: u16, body: &str) {
        self.event(UsageEvent::PublishFailureWithUsage(status, body.into()));
    }
    fn usage_required(&self) {
        self.event(UsageEvent::UsageRequired);
    }
    fn discard(&self) {
        self.event(UsageEvent::Discard);
    }
}

impl UsageLog {
    pub(crate) fn sink(self: &Arc<Self>) -> cpa_core::exec::UsageSink {
        cpa_core::exec::UsageSink::new(self.clone())
    }

    fn event(&self, event: UsageEvent) {
        self.1.lock().unwrap().push(event);
    }

    pub(crate) fn events(&self) -> Vec<UsageEvent> {
        self.1.lock().unwrap().clone()
    }

    /// The reported upstream payloads (bodies and lines, `data:` stripped), in order.
    pub(crate) fn payloads(&self) -> Vec<(Format, Vec<u8>)> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(kind, _, _)| *kind != "request")
            .map(|(_, f, p)| {
                let t = p.trim_ascii();
                (*f, t.strip_prefix(b"data:").map_or(t, <[u8]>::trim_ascii).to_vec())
            })
            .collect()
    }
}

#[test]
fn log_capture_sees_callsites_another_thread_registered_first() {
    // Hit first on a thread without a subscriber. With a single live dispatcher tracing
    // would cache `never` for this callsite there and drop the event below.
    fn probe() {
        tracing::warn!("log capture probe");
    }
    let capture = LogCapture::default();
    let guard = capture.install();
    std::thread::spawn(probe).join().unwrap();
    probe();
    drop(guard);
    let logs = capture.0.lock().unwrap().clone();
    assert_eq!(logs.len(), 1, "{logs:?}");
    assert!(logs[0].contains("log capture probe"), "{logs:?}");
}

/// One request-capture call (`cpa_core::exec::CaptureEvent`), owned.
#[derive(Debug, Clone)]
pub(crate) enum CaptureRecord {
    Request {
        url: String,
        method: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        auth: [String; 5],
    },
    Metadata(u16, Vec<(String, String)>),
    Error(String),
    Chunk(Vec<u8>),
}

/// Records what an executor reports to its request-capture sink.
#[derive(Default)]
pub(crate) struct CaptureLog(Mutex<Vec<CaptureRecord>>);

impl cpa_core::exec::CaptureObserver for CaptureLog {
    fn record(&self, event: cpa_core::exec::CaptureEvent<'_>, _: &cpa_core::exec::SentCredentials) {
        use cpa_core::exec::CaptureEvent as E;
        let record = match event {
            E::Request(r) => CaptureRecord::Request {
                url: r.url.into(),
                method: r.method.into(),
                headers: r.headers.to_vec(),
                body: r.body.to_vec(),
                auth: [r.provider, r.auth_id, r.auth_label, r.auth_type, r.auth_value].map(String::from),
            },
            E::ResponseMetadata(status, headers) => CaptureRecord::Metadata(status, headers.to_vec()),
            E::ResponseError(error) => CaptureRecord::Error(error.into()),
            E::ResponseChunk(chunk) => CaptureRecord::Chunk(chunk.to_vec()),
            _ => panic!("device providers send no upstream WebSocket events"),
        };
        self.0.lock().unwrap().push(record);
    }
}

impl CaptureLog {
    pub(crate) fn records(&self) -> Vec<CaptureRecord> {
        self.0.lock().unwrap().clone()
    }

    pub(crate) fn sink(self: &Arc<Self>) -> cpa_core::exec::CaptureSink {
        cpa_core::exec::CaptureSink::new(self.clone())
    }
}
