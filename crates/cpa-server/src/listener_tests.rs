//! #103 (Go v8.0.67 477605dc, GHSA-j4xx-x47j-7h54): no client holds a connection on the
//! main listener open without authenticating. Real loopback sockets against the stack
//! `main.rs` serves (access log, body limit, request capture, CORS, client-key auth),
//! with [`LIMIT`] standing for Go's 10 s deadline. Descriptors are counted from
//! `/proc/self`, so these tests run on Linux only.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use cpa_core::config::Config;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;

use crate::management::{Management, Options};

const KEY: &str = "client-key-listener-bounds";
const PASSWORD: &str = "management-listener-bounds";
/// Stands for Go's 10 s `muxSniffDeadline` / `ReadHeaderTimeout`.
const LIMIT: Duration = Duration::from_millis(1000);
/// How long a bounded connection may take to close: [`LIMIT`] plus scheduling slack.
const CLOSED_WITHIN: Duration = Duration::from_secs(8);
/// Concurrent connections per case.
const CONNECTIONS: usize = 16;

struct Proxy {
    addr: SocketAddr,
    dir: PathBuf,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The served stack on a fresh loopback port, with a private `auth-dir`, one client
/// key, a management password for RESP and the Claude executor on the discard port
/// (no credential is loaded, so nothing is sent upstream).
async fn proxy(request_log: bool) -> Proxy {
    let dir = std::env::temp_dir().join(format!("cpa-listener-bounds-{}", uuid::Uuid::new_v4()));
    let auth_dir = dir.join("auth");
    std::fs::create_dir_all(&auth_dir).unwrap();
    let yaml = format!(
        "auth-dir: {}\napi-keys: [{KEY}]\nobservability: {{logs: {{request-log: {request_log}}}}}\n",
        auth_dir.display()
    );
    let rt = Arc::new(crate::testing::runtime(
        Config::parse(&yaml).unwrap(),
        Vec::new(),
        cpa_exec::Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:9").unwrap(),
            codex: Default::default(),
            meta: Default::default(),
            xai: Default::default(),
        },
    ));
    let management = Management::with_options(
        rt.clone(),
        dir.join("config.yaml"),
        Options {
            log_dir: Some(dir.join("logs")),
            management_password: Some(PASSWORD.into()),
            ..Default::default()
        },
    );
    // main.rs `app`, without the example-key warning page (no example keys here).
    let app = crate::app(rt.clone(), crate::management::router(management.clone()));
    let app = app.layer(axum::middleware::from_fn(crate::management::cors));
    let app = crate::request_logging::router(&management, app);
    let app = crate::body_limit::layer(&rt, app);
    let app = crate::observability::router(&rt, app);
    let addr = spawn(app, Some(management)).await;
    Proxy { addr, dir }
}

async fn spawn(app: Router, management: Option<Arc<Management>>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(super::accept(listener, app, None, management, LIMIT));
    addr
}

/// The descriptors this process holds for connections the listener on `port`
/// accepted: sockets whose local port is `port`, the listening socket excepted.
#[cfg(target_os = "linux")]
fn server_descriptors(port: u16) -> usize {
    let mut sockets = std::collections::HashSet::new();
    for table in ["/proc/self/net/tcp", "/proc/self/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(table) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (Some(local), Some(state), Some(inode)) = (fields.get(1), fields.get(3), fields.get(9)) else {
                continue;
            };
            let local_port = local
                .rsplit(':')
                .next()
                .and_then(|hex| u16::from_str_radix(hex, 16).ok());
            // State 0A is LISTEN; an orphaned socket has inode 0 and no descriptor.
            if local_port == Some(port) && *state != "0A" && *inode != "0" {
                sockets.insert(format!("socket:[{inode}]"));
            }
        }
    }
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .flatten()
        .filter_map(|entry| std::fs::read_link(entry.path()).ok())
        .filter(|target| sockets.contains(&*target.to_string_lossy()))
        .count()
}

/// Waits until the server holds exactly `want` accepted connections.
#[cfg(target_os = "linux")]
async fn descriptors_settle(port: u16, want: usize, case: &str) {
    let deadline = Instant::now() + CLOSED_WITHIN;
    loop {
        let held = server_descriptors(port);
        if held == want {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{case}: the server still holds {held} connections, want {want}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Reads until the server closes the connection and returns what arrived, or why the
/// connection is still open after [`CLOSED_WITHIN`].
async fn until_closed(stream: &mut TcpStream, case: &str) -> Result<Vec<u8>, String> {
    let mut received = Vec::new();
    match tokio::time::timeout(CLOSED_WITHIN, stream.read_to_end(&mut received)).await {
        Ok(Ok(_)) => Ok(received),
        Ok(Err(e)) if matches!(e.kind(), std::io::ErrorKind::ConnectionReset) => Ok(received),
        Ok(Err(e)) => Err(format!("{case}: {e}")),
        Err(_) => Err(format!(
            "{case}: still open after {CLOSED_WITHIN:?}; received {:?}",
            String::from_utf8_lossy(&received)
        )),
    }
}

/// [`until_closed`], failing the test when the connection stays open.
async fn read_until_closed(stream: &mut TcpStream, case: &str) -> Vec<u8> {
    until_closed(stream, case).await.unwrap_or_else(|why| panic!("{why}"))
}

/// Each case: what the client sends before it stalls, the status line it gets first
/// (`None` when the server must close without answering), and whether only a deadline
/// can end it (the server holds it until [`LIMIT`]).
const CASES: [(&str, &str, Option<&str>, bool); 7] = [
    ("no byte at all", "", None, true),
    (
        "incomplete request head",
        "POST /v1/messages HTTP/1.1\r\nHost: proxy\r\nContent-Type: appl",
        None,
        true,
    ),
    (
        "incomplete body under 1 MiB, no client key",
        "POST /v1/messages HTTP/1.1\r\nHost: proxy\r\nContent-Type: application/json\r\n\
         Content-Length: 4096\r\n\r\n{\"model\":",
        Some("HTTP/1.1 401 "),
        false,
    ),
    (
        "incomplete chunked body, no client key",
        "POST /v1/chat/completions HTTP/1.1\r\nHost: proxy\r\nContent-Type: application/json\r\n\
         Transfer-Encoding: chunked\r\n\r\n40\r\n{\"model\":",
        Some("HTTP/1.1 401 "),
        false,
    ),
    (
        "incomplete body on a removed-provider route",
        "POST /v1beta/models/m:generateContent HTTP/1.1\r\nHost: proxy\r\n\
         Content-Length: 4096\r\n\r\n{",
        Some("HTTP/1.1 410 "),
        false,
    ),
    (
        "incomplete body on an unrouted path",
        "POST /wp-login.php HTTP/1.1\r\nHost: proxy\r\nContent-Length: 4096\r\n\r\n{",
        Some("HTTP/1.1 404 "),
        false,
    ),
    (
        "idle keep-alive connection after a complete request",
        "GET /healthz HTTP/1.1\r\nHost: proxy\r\n\r\n",
        Some("HTTP/1.1 200 "),
        true,
    ),
];

/// Concurrent stalled connections of every kind, with `request-log` off (a small body
/// used to be read before authentication) and on (every body was): each one closes
/// within the deadline, with the answer its request gets, and the server then holds
/// none of their descriptors. Before #103 the head, keep-alive and pre-auth body cases
/// stayed open for as long as the client kept them.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unauthenticated_stalls_close_and_release_their_descriptors() {
    for request_log in [false, true] {
        let proxy = proxy(request_log).await;
        let port = proxy.addr.port();
        descriptors_settle(port, 0, "before any connection").await;
        let started = Instant::now();
        let mut clients = Vec::new();
        for (case, sent, status, _) in CASES {
            for _ in 0..CONNECTIONS {
                let mut stream = TcpStream::connect(proxy.addr).await.unwrap();
                stream.write_all(sent.as_bytes()).await.unwrap();
                clients.push((format!("request-log={request_log}, {case}"), status, stream));
            }
        }
        // Control: the count sees the connections the server holds. Skipped when the
        // machine was too slow to look before the first deadline could have passed.
        let stalled = CASES.iter().filter(|case| case.3).count() * CONNECTIONS;
        tokio::time::sleep(LIMIT / 5).await;
        let held = server_descriptors(port);
        if started.elapsed() < LIMIT * 3 / 4 {
            assert!(
                held >= stalled,
                "request-log={request_log}: the server holds {held} connections, want at least {stalled}"
            );
        } else {
            eprintln!("descriptor control skipped: {:?} passed", started.elapsed());
        }
        let mut readers = tokio::task::JoinSet::new();
        for (case, status, mut stream) in clients {
            readers.spawn(async move {
                let received = until_closed(&mut stream, &case).await;
                (case, status, received, stream)
            });
        }
        // Every connection is judged before the test fails, so the report names each
        // kind that stayed open or got the wrong answer.
        let mut failures = std::collections::BTreeSet::new();
        let mut still_open = Vec::new();
        while let Some(joined) = readers.join_next().await {
            let (case, status, received, stream) = joined.unwrap();
            match received {
                Err(why) => {
                    failures.insert(why.split("; received").next().unwrap_or_default().to_owned());
                }
                Ok(received) => {
                    let received = String::from_utf8_lossy(&received);
                    let answered = match status {
                        Some(status) => received.starts_with(status),
                        None => received.is_empty(),
                    };
                    if !answered {
                        failures.insert(format!("{case}: answered {received:?}, want {status:?}"));
                    }
                }
            }
            // The client keeps its end open: only the server's side may be gone.
            still_open.push(stream);
        }
        assert!(
            failures.is_empty(),
            "{} kinds failed:\n{}",
            failures.len(),
            failures.into_iter().collect::<Vec<_>>().join("\n")
        );
        descriptors_settle(port, 0, &format!("request-log={request_log}, after closing")).await;
        drop(still_open);
    }
}

/// An authenticated client that sends its body slowly, a chunk every half deadline for
/// three deadlines, is answered by its handler: neither the request-head deadline nor
/// request capture (whole with `request-log` on, as read otherwise) cuts it. No
/// credential serves the model, so the dispatcher answers before any upstream exists:
/// `dispatch::run_with_bootstrap_retries` routes first and `route` fails with
/// `Failure::UnknownModel`, whose text `dispatch::tests::failure_texts_match_go_shapes`
/// pins and `errors::openai_body` writes compacted with status 400.
#[tokio::test]
async fn authenticated_slow_bodies_are_not_cut() {
    for request_log in [false, true] {
        let proxy = proxy(request_log).await;
        let mut stream = TcpStream::connect(proxy.addr).await.unwrap();
        let head = format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: proxy\r\nAuthorization: Bearer {KEY}\r\n\
             Content-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(head.as_bytes()).await.unwrap();
        let body = r#"{"model":"cpa-test-slow-upload","messages":[{"role":"user","content":"hi"}]}"#;
        let pieces = [&body[..10], &body[10..30], &body[30..50], &body[50..60], &body[60..]];
        for piece in pieces {
            tokio::time::sleep(LIMIT * 3 / 5).await;
            stream
                .write_all(format!("{:x}\r\n{piece}\r\n", piece.len()).as_bytes())
                .await
                .unwrap();
        }
        stream.write_all(b"0\r\n\r\n").await.unwrap();
        let case = format!("request-log={request_log}");
        let received = read_until_closed(&mut stream, &case).await;
        let received = String::from_utf8_lossy(&received);
        assert!(received.starts_with("HTTP/1.1 400 "), "{case}: {received}");
        assert!(
            received.ends_with(
                r#"{"error":{"message":"unknown provider for model cpa-test-slow-upload","type":"invalid_request_error","code":"model_not_found","param":"model"}}"#
            ),
            "{case}: {received}"
        );
    }
}

/// A response that streams for several deadlines is not cut: hyper runs the
/// request-head clock only while the connection waits for a request.
#[tokio::test]
async fn slow_response_streams_are_not_cut() {
    let app = Router::new().route(
        "/slow",
        axum::routing::get(|| async {
            let events = futures_util::stream::unfold(0, |n| async move {
                if n == 4 {
                    return None;
                }
                if n > 0 {
                    tokio::time::sleep(LIMIT * 3 / 4).await;
                }
                let event = axum::body::Bytes::from(format!("data: {n}\n\n"));
                Some((Ok::<_, std::io::Error>(event), n + 1))
            });
            (
                [("content-type", "text/event-stream")],
                axum::body::Body::from_stream(events),
            )
        }),
    );
    let addr = spawn(app, None).await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"GET /slow HTTP/1.1\r\nHost: proxy\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let received = read_until_closed(&mut stream, "slow stream").await;
    let received = String::from_utf8_lossy(&received);
    assert!(received.starts_with("HTTP/1.1 200 "), "{received}");
    let events: Vec<_> = received
        .match_indices("data: ")
        .map(|(at, _)| &received[at..at + 7])
        .collect();
    assert_eq!(events, ["data: 0", "data: 1", "data: 2", "data: 3"], "{received}");
}

/// The Redis protocol on the same listener: until `AUTH` succeeds, a client must finish
/// its commands within the deadline, whether it stalls inside one or idles after a
/// refused one; after `AUTH`, an idle client keeps its connection.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn resp_setup_is_bounded_until_auth() {
    let proxy = proxy(false).await;
    let port = proxy.addr.port();
    let mut partial = TcpStream::connect(proxy.addr).await.unwrap();
    partial.write_all(b"*1\r\n$4\r\nPI").await.unwrap();
    let mut refused = TcpStream::connect(proxy.addr).await.unwrap();
    refused.write_all(b"*1\r\n$4\r\nPING\r\n").await.unwrap();
    let mut authed = TcpStream::connect(proxy.addr).await.unwrap();
    let auth = format!("*2\r\n$4\r\nAUTH\r\n${}\r\n{PASSWORD}\r\n", PASSWORD.len());
    authed.write_all(auth.as_bytes()).await.unwrap();
    let mut ok = [0u8; 5];
    tokio::time::timeout(CLOSED_WITHIN, authed.read_exact(&mut ok))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&ok, b"+OK\r\n");

    assert!(
        read_until_closed(&mut partial, "stalled inside a command")
            .await
            .is_empty()
    );
    let received = read_until_closed(&mut refused, "idle after NOAUTH").await;
    assert_eq!(
        String::from_utf8_lossy(&received),
        "-NOAUTH Authentication required.\r\n"
    );
    descriptors_settle(port, 1, "only the authenticated client remains").await;

    tokio::time::sleep(LIMIT * 2).await;
    authed.write_all(b"*2\r\n$4\r\nLPOP\r\n$5\r\nusage\r\n").await.unwrap();
    let mut nil = [0u8; 5];
    tokio::time::timeout(CLOSED_WITHIN, authed.read_exact(&mut nil))
        .await
        .expect("an authenticated client keeps its connection")
        .unwrap();
    assert_eq!(&nil, b"$-1\r\n");
    drop((partial, refused, authed));
    descriptors_settle(port, 0, "after the clients left").await;
}
