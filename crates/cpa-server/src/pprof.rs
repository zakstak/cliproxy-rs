//! Optional, separate debug listener (sdk/cliproxy/pprof_server.go, v8.0.35).
//! One task owns the listener and its connections, so a stale config application or
//! failed listener cannot stop its replacement. No API or management middleware runs here.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{OriginalUri, Request};
use axum::http::{Method, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::any;
use cpa_core::config::Config;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};
use tower::ServiceExt;

use crate::Runtime;

const STOP_TIMEOUT: Duration = Duration::from_secs(5);
const PROFILES: &[&str] = &[
    "allocs",
    "block",
    "cmdline",
    "goroutine",
    "heap",
    "mutex",
    "profile",
    "symbol",
    "threadcreate",
    "trace",
];

/// The debug listener follows config publications, including Home config updates.
/// Dropping it requests shutdown; [`Self::shutdown`] waits up to five seconds for
/// draining. Active requests can finish afterwards, as with Go's http.Server.Shutdown.
pub struct Server {
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
    #[cfg(test)]
    accept_errors: tokio::sync::mpsc::UnboundedSender<(std::io::Error, tokio::sync::oneshot::Sender<()>)>,
}

impl Server {
    pub fn start(rt: &Arc<Runtime>) -> Self {
        let (stop, stopped) = watch::channel(false);
        #[cfg(test)]
        let (accept_errors, mut injected_errors) =
            tokio::sync::mpsc::unbounded_channel::<(std::io::Error, tokio::sync::oneshot::Sender<()>)>();
        // Subscribe before taking the first snapshot, so startup cannot miss a reload.
        let changed = rt.subscribe_config();
        let weak = Arc::downgrade(rt);
        let task = tokio::spawn(async move {
            let mut changed = changed;
            let mut stopped = stopped;
            let mut listener: Option<TcpListener> = None;
            let mut current = (false, String::new());
            let mut connections = JoinSet::new();
            let (closing, _) = watch::channel(());
            let mut apply = true;
            let mut accept_delay = Duration::ZERO;
            let mut retry_at = None;
            loop {
                if *stopped.borrow() {
                    break;
                }
                if apply {
                    apply = false;
                    // Mark this publication before draining. A newer publication
                    // remains eligible even when this apply times out.
                    changed.borrow_and_update();
                    let Some(rt) = weak.upgrade() else { break };
                    let desired = settings(&rt.config());
                    drop(rt);
                    if desired != current || (desired.0 && listener.is_none()) {
                        // Stop the physical listener before draining; later binds cannot
                        // accidentally retain connections from the previous generation.
                        listener = None;
                        closing.send_replace(());
                        if tokio::time::timeout(STOP_TIMEOUT, async {
                            while connections.join_next().await.is_some() {}
                        })
                        .await
                        .is_err()
                        {
                            // http.Server.Shutdown's deadline does not abort active requests.
                            connections.detach_all();
                            tracing::error!("pprof server stop timed out on {}", current.1);
                            // Go's ApplyContext returns the shutdown error without binding
                            // a replacement. Only a later publication can retry.
                            continue;
                        }
                        // A newer configuration may have arrived while old requests drained.
                        if *stopped.borrow() {
                            break;
                        }
                        changed.borrow_and_update();
                        let Some(rt) = weak.upgrade() else { break };
                        current = settings(&rt.config());
                        drop(rt);
                        if current.0 {
                            let binding = async {
                                if let Some(port) = current.1.strip_prefix(':') {
                                    let port = port.parse::<u16>().map_err(std::io::Error::other)?;
                                    TcpListener::from_std(crate::listener::bind("", port)?)
                                } else {
                                    TcpListener::bind(&current.1).await
                                }
                            };
                            // Hostname resolution must not hold shutdown or a newer config hostage.
                            let bound = tokio::select! {
                                biased;
                                _ = stopped.changed() => break,
                                result = changed.changed() => {
                                    if result.is_err() { break; }
                                    apply = true;
                                    continue;
                                },
                                bound = binding => bound,
                            };
                            match bound {
                                Ok(bound) => {
                                    tracing::info!("pprof server starting on {}", current.1);
                                    listener = Some(bound);
                                    accept_delay = Duration::ZERO;
                                    retry_at = None;
                                }
                                Err(error) => tracing::error!("pprof server failed on {}: {error}", current.1),
                            }
                        }
                    }
                }
                if *stopped.borrow() {
                    break;
                }
                tokio::select! {
                    biased;
                    _ = stopped.changed() => break,
                    result = changed.changed() => {
                        if result.is_err() { break; }
                        apply = true;
                    },
                    _ = connections.join_next(), if !connections.is_empty() => {},
                    accepted = async {
                        let Some(listener) = &listener else {
                            return std::future::pending().await;
                        };
                        if let Some(deadline) = retry_at {
                            tokio::time::sleep_until(deadline).await;
                        }
                        #[cfg(test)]
                        {
                            tokio::select! {
                                biased;
                                error = injected_errors.recv() => {
                                    let (error, accepted) = error.expect("server owns injection sender");
                                    let _ = accepted.send(());
                                    Err(error)
                                },
                                accepted = listener.accept() => accepted,
                            }
                        }
                        #[cfg(not(test))]
                        listener.accept().await
                    } => match accepted {
                        Ok((stream, _)) => {
                            accept_delay = Duration::ZERO;
                            retry_at = None;
                            let mut closing = closing.subscribe();
                            connections.spawn(async move {
                                let app = router();
                                let service = hyper::service::service_fn(move |req| app.clone().oneshot(req));
                                let mut builder = hyper::server::conn::http1::Builder::new();
                                builder.timer(TokioTimer::new()).header_read_timeout(STOP_TIMEOUT);
                                let connection = builder.serve_connection(TokioIo::new(stream), service);
                                tokio::pin!(connection);
                                tokio::select! {
                                    _ = &mut connection => {},
                                    _ = closing.changed() => {
                                        connection.as_mut().graceful_shutdown();
                                        let _ = connection.await;
                                    }
                                }
                            });
                        }
                        Err(error) if temporary_accept_error(&error) => {
                            // net/http.Server.Serve keeps its socket and active
                            // connections, retrying from 5 ms up to one second.
                            accept_delay = if accept_delay.is_zero() {
                                Duration::from_millis(5)
                            } else {
                                (accept_delay * 2).min(Duration::from_secs(1))
                            };
                            retry_at = Some(tokio::time::Instant::now() + accept_delay);
                            tracing::error!("pprof accept failed on {}: {error}; retrying in {accept_delay:?}", current.1);
                        }
                        Err(error) => {
                            tracing::error!("pprof accept failed on {}: {error}", current.1);
                            // A terminal failure waits for another config publication.
                            listener = None;
                        }
                    }
                }
            }
            drop(listener);
            closing.send_replace(());
            if tokio::time::timeout(STOP_TIMEOUT, async { while connections.join_next().await.is_some() {} })
                .await
                .is_err()
            {
                connections.detach_all();
                tracing::error!("pprof server stop timed out on {}", current.1);
            }
        });
        Self {
            stop,
            task: Some(task),
            #[cfg(test)]
            accept_errors,
        }
    }

    pub async fn shutdown(mut self) {
        self.stop.send_replace(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}

fn temporary_accept_error(error: &std::io::Error) -> bool {
    use std::io::ErrorKind::{ConnectionAborted, ConnectionReset, Interrupted, TimedOut, WouldBlock};
    if matches!(
        error.kind(),
        ConnectionAborted | ConnectionReset | Interrupted | TimedOut | WouldBlock
    ) {
        return true;
    }
    // net.OpError.Temporary also includes exhausted process/system descriptor tables.
    if matches!(error.raw_os_error(), Some(libc::EMFILE | libc::ENFILE)) {
        return true;
    }
    false
}

fn settings(config: &Config) -> (bool, String) {
    let pprof = config.document.get("observability").and_then(|o| o.get("pprof"));
    let enabled = pprof.and_then(|p| p.get("enable")).and_then(|v| v.as_bool()) == Some(true);
    let addr = pprof.and_then(|p| p.get("addr")).and_then(|v| v.as_str()).unwrap_or("");
    let addr = crate::gojson::trim(addr);
    (enabled, if addr.is_empty() { "127.0.0.1:8316" } else { addr }.into())
}

fn router() -> Router {
    Router::new()
        .route(
            "/debug/pprof",
            any(|OriginalUri(uri): OriginalUri| async move {
                let location = match uri.query() {
                    Some(query) => format!("/debug/pprof/?{query}"),
                    None => "/debug/pprof/".into(),
                };
                // Go 1.26 ServeMux (CLIProxyAPI v8.0.35's release toolchain) answers 307.
                (StatusCode::TEMPORARY_REDIRECT, [(header::LOCATION, location)])
            }),
        )
        .route("/debug/pprof/", any(index))
        .route("/debug/pprof/cmdline", any(cmdline))
        .route("/debug/pprof/symbol", any(symbol))
        .route("/debug/pprof/profile", any(profile))
        .route("/debug/pprof/{name}", any(unsupported))
}

fn text(status: StatusCode, message: impl Into<String>) -> Response {
    (status, [(header::X_CONTENT_TYPE_OPTIONS, "nosniff")], message.into()).into_response()
}

async fn index() -> Response {
    let mut html = String::from("<html><head><title>/debug/pprof/</title></head><body><h1>/debug/pprof/</h1><ul>");
    for name in PROFILES {
        html.push_str(&format!("<li><a href=\"{name}\">{name}</a></li>\n"));
    }
    html.push_str("</ul><p>Go runtime profiles are unavailable in Rust. CPU profiling requires the pprof build feature.</p></body></html>");
    Html(html).into_response()
}

async fn cmdline() -> Response {
    // net/http/pprof.Cmdline joins os.Args with NUL, with no trailing separator.
    text(
        StatusCode::OK,
        std::env::args_os()
            .map(|s| s.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\0"),
    )
}

async fn symbol(OriginalUri(uri): OriginalUri, req: Request) -> Response {
    let raw = if req.method() == Method::POST {
        match axum::body::to_bytes(req.into_body(), crate::MAX_REQUEST_BYTES).await {
            Ok(body) => String::from_utf8_lossy(&body).into_owned(),
            Err(_) => return text(StatusCode::PAYLOAD_TOO_LARGE, "symbol body too large\n"),
        }
    } else {
        uri.query().unwrap_or_default().to_owned()
    };
    #[cfg(not(feature = "pprof"))]
    {
        let _ = raw;
        text(StatusCode::OK, "num_symbols: 1\n")
    }
    #[cfg(feature = "pprof")]
    {
        // Native symbolization takes a blocking global lock. Bound dispatched jobs, not
        // just awaiting requests: a cancelled request keeps its slot until the job stops.
        static SYMBOLS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
        let capacity = SYMBOLS.acquire().await.expect("symbol semaphore is never closed");
        let (active, cancelled) = watch::channel(());
        let result = tokio::task::spawn_blocking(move || {
            let _capacity = capacity;
            let mut output = String::from("num_symbols: 1\n");
            for pc in raw.split('+') {
                if cancelled.has_changed().is_err() {
                    break;
                }
                let parsed = if let Some(hex) = pc.strip_prefix("0x").or_else(|| pc.strip_prefix("0X")) {
                    usize::from_str_radix(hex, 16)
                } else if pc.starts_with('0') {
                    usize::from_str_radix(pc, 8)
                } else {
                    pc.parse()
                };
                if let Ok(addr) = parsed
                    && addr != 0
                {
                    // backtrace subtracts one from a return address; pprof takes the actual PC.
                    backtrace::resolve(addr.wrapping_add(1) as *mut std::ffi::c_void, |symbol| {
                        if let Some(name) = symbol.name() {
                            output.push_str(&format!("{addr:#x} {name}\n"));
                        }
                    });
                }
            }
            output
        })
        .await;
        drop(active);
        match result {
            Ok(output) => text(StatusCode::OK, output),
            Err(_) => text(StatusCode::INTERNAL_SERVER_ERROR, "Could not resolve symbols\n"),
        }
    }
}

async fn unsupported(axum::extract::Path(name): axum::extract::Path<String>) -> Response {
    if PROFILES.contains(&name.as_str()) {
        text(
            StatusCode::NOT_IMPLEMENTED,
            format!("{name}: Go runtime profiles are unavailable in Rust\n"),
        )
    } else {
        text(StatusCode::NOT_FOUND, "Unknown profile\n")
    }
}

#[cfg(not(all(feature = "pprof", unix)))]
async fn profile() -> Response {
    text(
        StatusCode::NOT_IMPLEMENTED,
        "CPU profiling requires the cpa-server/pprof build feature on Unix\n",
    )
}

#[cfg(all(feature = "pprof", unix))]
async fn profile(OriginalUri(uri): OriginalUri, req: Request) -> Response {
    use pprof::protos::Message;
    // net/http FormValue prefers POST, PUT and PATCH form bodies to the URL query.
    let mut seconds = None;
    if matches!(*req.method(), Method::POST | Method::PUT | Method::PATCH)
        // mime.ParseMediaType: the type is case-insensitive, parameters and spaces ignored.
        && req.headers().get(header::CONTENT_TYPE).is_some_and(|v| {
            v.to_str().unwrap_or_default().split(';').next().unwrap_or_default().trim()
                .eq_ignore_ascii_case("application/x-www-form-urlencoded")
        })
    {
        let Ok(body) = axum::body::to_bytes(req.into_body(), crate::MAX_REQUEST_BYTES).await else {
            return text(StatusCode::PAYLOAD_TOO_LARGE, "profile body too large\n");
        };
        seconds = crate::access::query_get(&String::from_utf8_lossy(&body), "seconds");
    }
    // Go defaults invalid, zero and negative durations to 30 seconds.
    let seconds = seconds
        .or_else(|| crate::access::query_get(uri.query().unwrap_or_default(), "seconds"))
        .and_then(|s| String::from_utf8(s).ok())
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(30);
    // Go time.Duration is signed nanoseconds and multiplication wraps.
    let duration = Duration::from_nanos(seconds.wrapping_mul(1_000_000_000).max(0) as u64);
    // Reject overlap before touching native locks, including initialization/reporting.
    static CPU: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
    let Ok(permit) = CPU.try_acquire() else {
        return text(StatusCode::INTERNAL_SERVER_ERROR, "Could not enable CPU profiling\n");
    };
    let started = tokio::task::spawn_blocking(move || {
        pprof::ProfilerGuardBuilder::default()
            .frequency(100)
            .blocklist(&["libc", "libgcc", "pthread", "vdso"])
            .build()
            .map(|guard| (guard, permit))
    })
    .await;
    let (guard, permit) = match started {
        Ok(Ok(started)) => started,
        _ => return text(StatusCode::INTERNAL_SERVER_ERROR, "Could not enable CPU profiling\n"),
    };
    // Locals drop in reverse declaration order: stop sampling before releasing admission.
    let guard = guard;
    // Keep the guard in the request future: cancelling it releases the global profiler.
    tokio::time::sleep(duration).await;
    // Native unwinding/symbolization is blocking. Only the one global profiler can
    // reach this worker. Cancellation during sampling still drops the guard above.
    let bytes = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let report = guard.report().build();
        drop(guard);
        report
            .and_then(|r| r.pprof())
            .ok()
            .and_then(|p| p.write_to_bytes().ok())
    })
    .await
    .ok()
    .flatten();
    match bytes {
        Some(bytes) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream"),
                (header::CONTENT_DISPOSITION, "attachment; filename=\"profile\""),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            bytes,
        )
            .into_response(),
        None => text(StatusCode::INTERNAL_SERVER_ERROR, "Could not build CPU profile\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn symbol_on(stream: &mut tokio::net::TcpStream) {
        stream
            .write_all(b"GET /debug/pprof/symbol HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            headers.push(stream.read_u8().await.unwrap());
        }
        assert!(headers.starts_with(b"HTTP/1.1 200"));
        let mut body = [0; 15];
        stream.read_exact(&mut body).await.unwrap();
        assert_eq!(&body, b"num_symbols: 1\n");
    }

    #[tokio::test]
    async fn pprof_routes_and_symbols() {
        let response = router()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/debug/pprof?seconds=2")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 307);
        assert_eq!(response.headers()[header::LOCATION], "/debug/pprof/?seconds=2");
        for (path, expected) in [("", 200), ("cmdline", 200), ("symbol", 200), ("unknown", 404)] {
            let response = router()
                .oneshot(
                    Request::builder()
                        .uri(format!("/debug/pprof/{path}"))
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{path}");
        }
        for name in ["allocs", "block", "goroutine", "heap", "mutex", "threadcreate", "trace"] {
            let response = router()
                .oneshot(
                    Request::builder()
                        .uri(format!("/debug/pprof/{name}"))
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), 501, "{name}");
            assert!(
                response.headers()[header::CONTENT_TYPE]
                    .to_str()
                    .unwrap()
                    .starts_with("text/plain")
            );
        }
        let addr = pprof_routes_and_symbols as *const () as usize;
        for method in [Method::GET, Method::POST] {
            let raw = format!("{addr:#x}+0x0+garbage");
            let uri = if method == Method::GET {
                format!("/debug/pprof/symbol?{raw}")
            } else {
                "/debug/pprof/symbol".into()
            };
            let response = router()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .body(axum::body::Body::from(raw))
                        .unwrap(),
                )
                .await
                .unwrap();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let body = String::from_utf8(body.to_vec()).unwrap();
            assert!(body.starts_with("num_symbols: 1\n"));
            #[cfg(feature = "pprof")]
            assert!(body.contains("pprof_routes_and_symbols"), "{body}");
            #[cfg(not(feature = "pprof"))]
            assert_eq!(body, "num_symbols: 1\n");
        }
        #[cfg(not(feature = "pprof"))]
        {
            let response = router()
                .oneshot(
                    Request::builder()
                        .uri("/debug/pprof/profile")
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), 501);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert!(String::from_utf8_lossy(&body).contains("cpa-server/pprof"));
        }
    }

    #[tokio::test]
    async fn pprof_listener_reload_failure_and_shutdown() {
        let dir = std::env::temp_dir().join(format!("pprof-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = |enabled, addr: &str, legacy| {
            let pprof = format!("pprof: {{enable: {enabled}, addr: '{addr}'}}");
            Config::parse(&format!(
                "oauth: {{auth-dir: '{}'}}\n{}\n",
                dir.display(),
                if legacy {
                    pprof
                } else {
                    format!("observability:\n  {pprof}")
                }
            ))
            .unwrap()
        };
        let executors = cpa_exec::Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            meta: Default::default(),
            xai: Default::default(),
        };
        let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = occupied.local_addr().unwrap().to_string();
        let rt = Arc::new(crate::testing::runtime(config(false, &addr, false), vec![], executors));
        let server = Server::start(&rt);
        tokio::task::yield_now().await;
        rt.publish_config(config(true, &addr, false));
        tokio::task::yield_now().await;
        // A failed bind is retried on a later same-address publication.
        drop(occupied);
        rt.publish_config(config(true, &addr, true));
        let client = wreq::Client::builder().no_proxy().build().unwrap();
        let fetch = |addr: String| {
            let client = client.clone();
            async move {
                tokio::time::timeout(Duration::from_secs(7), async {
                    loop {
                        if let Ok(response) = client.get(format!("http://{addr}/debug/pprof/cmdline")).send().await {
                            assert_eq!(response.status(), 200);
                            let expected = std::env::args_os()
                                .map(|s| s.to_string_lossy().into_owned())
                                .collect::<Vec<_>>()
                                .join("\0");
                            assert_eq!(response.text().await.unwrap(), expected);
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .unwrap();
            }
        };
        fetch(addr.clone()).await;
        // An unrelated/same-address reload does not close an existing connection.
        let mut keep = tokio::net::TcpStream::connect(&addr).await.unwrap();
        symbol_on(&mut keep).await;
        rt.publish_config(config(true, &addr, false));
        tokio::task::yield_now().await;
        symbol_on(&mut keep).await;
        // Inject repeated descriptor exhaustion at the real accept boundary.
        // Retries back off, but neither the socket nor existing connections close.
        let started = tokio::time::Instant::now();
        for _ in 0..9 {
            let (accepted, observed) = tokio::sync::oneshot::channel();
            let error = std::io::Error::from_raw_os_error(libc::EMFILE);
            server.accept_errors.send((error, accepted)).unwrap();
            observed.await.unwrap();
            symbol_on(&mut keep).await;
        }
        assert!(
            started.elapsed() >= Duration::from_secs(1),
            "accept errors did not back off"
        );
        // The current retry delay is one second; a disable must interrupt it.
        rt.publish_config(config(false, &addr, false));
        tokio::time::timeout(Duration::from_millis(500), async {
            while tokio::net::TcpStream::connect(&addr).await.is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        rt.publish_config(config(true, &addr, false));
        fetch(addr.clone()).await;
        // Hold a request body open past the drain deadline. Go leaves it running
        // and does not bind the replacement until a later config publication.
        let mut active = tokio::net::TcpStream::connect(&addr).await.unwrap();
        active.write_all(b"POST /debug/pprof/symbol HTTP/1.1\r\nHost: localhost\r\nContent-Length: 3\r\nExpect: 100-continue\r\n\r\n").await.unwrap();
        let mut interim = [0; 25];
        active.read_exact(&mut interim).await.unwrap();
        assert_eq!(&interim, b"HTTP/1.1 100 Continue\r\n\r\n");
        let replacement = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stale = replacement.local_addr().unwrap().to_string();
        drop(replacement);
        rt.publish_config(config(true, &stale, false));
        tokio::time::timeout(Duration::from_secs(5), async {
            while tokio::net::TcpStream::connect(&addr).await.is_ok() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(STOP_TIMEOUT + Duration::from_millis(100)).await;
        assert!(tokio::net::TcpStream::connect(&stale).await.is_err());
        let replacement = crate::listener::bind("", 0).unwrap();
        let port = replacement.local_addr().unwrap().port();
        let ipv6 = replacement.local_addr().unwrap().is_ipv6();
        let next = format!("127.0.0.1:{port}");
        let wildcard = format!(":{port}");
        drop(replacement);
        rt.publish_config(config(true, &wildcard, false));
        // The next publication remains eligible while the detached request runs.
        fetch(next.clone()).await;
        active.write_all(b"0x0").await.unwrap();
        let mut completed = String::new();
        tokio::time::timeout(Duration::from_secs(5), active.read_to_string(&mut completed))
            .await
            .unwrap()
            .unwrap();
        assert!(completed.starts_with("HTTP/1.1 200"), "{completed}");
        assert!(completed.contains("num_symbols: 1"), "{completed}");
        assert!(tokio::net::TcpStream::connect(&addr).await.is_err());
        assert!(tokio::net::TcpStream::connect(&stale).await.is_err());
        if ipv6 {
            fetch(format!("[::1]:{port}")).await;
        }
        rt.publish_config(config(false, &next, false));
        tokio::time::timeout(Duration::from_secs(5), async {
            while tokio::net::TcpStream::connect(&next).await.is_ok() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        rt.publish_config(config(true, &next, false));
        fetch(next.clone()).await;
        // Terminal accept failures do not spin on rebind or close active sockets.
        let mut keep = tokio::net::TcpStream::connect(&next).await.unwrap();
        symbol_on(&mut keep).await;
        let mut held = tokio::net::TcpStream::connect(&next).await.unwrap();
        held.write_all(b"POST /debug/pprof/symbol HTTP/1.1\r\nHost: localhost\r\nContent-Length: 3\r\nExpect: 100-continue\r\n\r\n").await.unwrap();
        let mut interim = [0; 25];
        tokio::time::timeout(Duration::from_secs(5), held.read_exact(&mut interim))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&interim, b"HTTP/1.1 100 Continue\r\n\r\n");
        let (accepted, observed) = tokio::sync::oneshot::channel();
        server
            .accept_errors
            .send((std::io::Error::other("terminal accept failure"), accepted))
            .unwrap();
        observed.await.unwrap();
        symbol_on(&mut keep).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(tokio::net::TcpStream::connect(&next).await.is_err());
        // Unlike Go, the next publication drains surviving connections before binding:
        // the held request keeps running and the replacement waits for it.
        rt.publish_config(config(true, &next, false));
        // EOF on the idle connection proves the publication started its graceful drain.
        let mut rest = Vec::new();
        let read = tokio::time::timeout(Duration::from_secs(3), keep.read_to_end(&mut rest)).await;
        assert!(matches!(read, Ok(Ok(0))), "{read:?}");
        // Draining waits for the held request, so nothing is bound yet.
        assert!(tokio::net::TcpStream::connect(&next).await.is_err());
        // Released well inside the five-second drain. Had the drain timed out instead,
        // the request would be detached and nothing rebound, so `fetch` below fails
        // within its own seven-second bound.
        held.write_all(b"0x0").await.unwrap();
        let mut completed = String::new();
        tokio::time::timeout(Duration::from_secs(5), held.read_to_string(&mut completed))
            .await
            .unwrap()
            .unwrap();
        assert!(completed.starts_with("HTTP/1.1 200"), "{completed}");
        fetch(next.clone()).await;
        server.shutdown().await;
        assert!(tokio::net::TcpStream::connect(&next).await.is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(all(feature = "pprof", unix))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pprof_cpu_profile_is_decodable_and_exclusive() {
        use pprof::protos::Message;
        let request = || {
            Request::builder()
                .uri("/debug/pprof/profile?seconds=1")
                .body(axum::body::Body::empty())
                .unwrap()
        };
        let mut first = Box::pin(router().oneshot(request()));
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut first)
                .await
                .is_err()
        );
        let first = tokio::spawn(first);
        // Keep real CPU work running throughout sampling, not just during native
        // initialization (which can exceed 200 ms on a loaded CI worker).
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped = stop.clone();
        let workload = std::thread::spawn(move || {
            while !stopped.load(std::sync::atomic::Ordering::Relaxed) {
                std::hint::black_box((0u64..1000).fold(0u64, |sum, n| sum.wrapping_add(n)));
            }
        });
        let second = router().oneshot(request()).await.unwrap();
        assert_eq!(second.status(), 500);
        let first = first.await.unwrap().unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        workload.join().unwrap();
        assert_eq!(first.status(), 200);
        assert_eq!(first.headers()[header::CONTENT_TYPE], "application/octet-stream");
        let bytes = axum::body::to_bytes(first.into_body(), usize::MAX).await.unwrap();
        let decoded = pprof::protos::Profile::parse_from_bytes(&bytes).unwrap();
        assert!(decoded.duration_nanos > 0);
        assert!(!decoded.sample.is_empty(), "profile did not sample the CPU workload");
        // Cancellation frees the global profiler; a later request can acquire it.
        let mut pending = Box::pin(router().oneshot(request()));
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut pending)
                .await
                .is_err()
        );
        drop(pending);
        let types = [
            "application/x-www-form-urlencoded",
            " Application/X-Www-Form-Urlencoded ; charset=utf-8",
        ];
        for (method, content_type) in [Method::POST, Method::PUT, Method::PATCH]
            .into_iter()
            .flat_map(|m| types.map(|t| (m.clone(), t)))
        {
            let response = tokio::time::timeout(
                Duration::from_secs(5),
                router().oneshot(
                    Request::builder()
                        .method(method)
                        .uri("/debug/pprof/profile?seconds=30")
                        .header(header::CONTENT_TYPE, content_type)
                        .body(axum::body::Body::from("seconds=1"))
                        .unwrap(),
                ),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(response.status(), 200);
        }
    }
}
