//! Go's Redis protocol on the main listener, ported from
//! internal/api/redis_queue_protocol_integration_test.go and
//! protocol_multiplexer_test.go: one listener serves HTTP and RESP, management-key
//! `AUTH`, `LPOP`/`RPOP` on the usage queue and `SUBSCRIBE` to `usage` and `errors`.

use std::sync::Arc;
use std::time::Duration;

use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use cpa_server::management::{Management, Options};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

const PASSWORD: &str = "test-management-password";

struct Server {
    addr: std::net::SocketAddr,
    rt: Arc<cpa_server::Runtime>,
    _dir: std::path::PathBuf,
}

async fn server(management_password: Option<&str>) -> Server {
    server_with(management_password, Vec::new()).await
}

async fn server_with(management_password: Option<&str>, credentials: Vec<cpa_core::credential::Credential>) -> Server {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("cpa-resp-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.yaml");
    let yaml = "observability:\n  usage:\n    usage-statistics-enabled: true\n";
    std::fs::write(&path, yaml).unwrap();
    let executors = Executors {
        claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: Default::default(),
        meta: Default::default(),
        xai: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(
        Config::parse(yaml).unwrap(),
        credentials,
        executors,
    ));
    let options = Options {
        management_password: Some(management_password.unwrap_or_default().to_owned()),
        ..Default::default()
    };
    let management = Management::with_options(rt.clone(), path, options);
    let app = cpa_server::app(rt.clone(), cpa_server::management::router(management.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(cpa_server::listener::serve_with_resp(
        listener,
        app,
        None,
        Some(management),
    ));
    Server { addr, rt, _dir: dir }
}

/// Go `writeTestRESPCommand`.
async fn command(conn: &mut BufReader<TcpStream>, args: &[&str]) {
    let mut out = format!("*{}\r\n", args.len());
    for a in args {
        out.push_str(&format!("${}\r\n{a}\r\n", a.len()));
    }
    conn.get_mut().write_all(out.as_bytes()).await.unwrap();
}

async fn line(conn: &mut BufReader<TcpStream>) -> String {
    let mut s = String::new();
    tokio::time::timeout(Duration::from_secs(5), conn.read_line(&mut s))
        .await
        .expect("reply in time")
        .unwrap();
    s.trim_end_matches("\r\n").to_owned()
}

async fn bulk(conn: &mut BufReader<TcpStream>) -> Option<String> {
    let header = line(conn).await;
    let len: i64 = header
        .strip_prefix('$')
        .unwrap_or_else(|| panic!("bulk header {header:?}"))
        .parse()
        .unwrap();
    if len < 0 {
        return None;
    }
    let mut buf = vec![0u8; len as usize + 2];
    conn.read_exact(&mut buf).await.unwrap();
    buf.truncate(len as usize);
    Some(String::from_utf8(buf).unwrap())
}

async fn array_len(conn: &mut BufReader<TcpStream>) -> usize {
    let header = line(conn).await;
    header
        .strip_prefix('*')
        .unwrap_or_else(|| panic!("array header {header:?}"))
        .parse()
        .unwrap()
}

/// Go `readTestRESPPubSubMessage` and `readTestRESPPubSubSubscribe`.
async fn pubsub(conn: &mut BufReader<TcpStream>) -> (String, String, String) {
    assert_eq!(array_len(conn).await, 3);
    let kind = bulk(conn).await.unwrap();
    let channel = bulk(conn).await.unwrap();
    let last = if kind == "message" {
        bulk(conn).await.unwrap()
    } else {
        line(conn).await
    };
    (kind, channel, last)
}

async fn connect(addr: std::net::SocketAddr) -> BufReader<TcpStream> {
    BufReader::new(TcpStream::connect(addr).await.unwrap())
}

async fn auth(conn: &mut BufReader<TcpStream>) {
    command(conn, &["AUTH", PASSWORD]).await;
    assert_eq!(line(conn).await, "+OK");
}

/// Go `TestRedisProtocol_ManagementDisabled_RejectsConnection`: closed, not ignored.
#[tokio::test]
async fn management_disabled_rejects_connection() {
    let s = server(None).await;
    let mut conn = connect(s.addr).await;
    command(&mut conn, &["PING"]).await;
    let mut buf = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(2), conn.read(&mut buf))
        .await
        .expect("closed, not timed out");
    assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
}

/// A Home control plane that is never asked: the RESP refusal comes first.
struct Home;

impl cpa_server::remote::RemoteDispatch for Home {
    fn available(&self) -> bool {
        true
    }

    fn dispatch(
        &self,
        _: cpa_server::remote::RemoteRequest,
    ) -> futures_util::future::BoxFuture<'_, Result<cpa_server::remote::RemoteGrant, cpa_server::remote::RemoteError>>
    {
        unreachable!("RESP never dispatches")
    }

    fn models(
        &self,
        _: Vec<(String, String)>,
        _: Vec<(String, String)>,
    ) -> futures_util::future::BoxFuture<'_, Result<Vec<u8>, cpa_server::remote::ModelsError>> {
        unreachable!("RESP never lists models")
    }
}

/// Go `TestRedisProtocol_HomeModeDisablesUsageOutput`: one error, then a clean close.
/// Go checks Home before the management gate, so a disabled management key changes
/// nothing.
#[tokio::test]
async fn home_mode_disables_usage_output() {
    for password in [Some(PASSWORD), None] {
        let s = server(password).await;
        s.rt.set_remote_dispatch(Some(Arc::new(Home)));
        let mut conn = connect(s.addr).await;
        command(&mut conn, &["PING"]).await;
        assert_eq!(
            line(&mut conn).await,
            "-ERR redis usage output disabled in home mode",
            "{password:?}"
        );
        let mut buf = [0u8; 1];
        let read = tokio::time::timeout(Duration::from_secs(2), conn.read(&mut buf))
            .await
            .expect("closed, not timed out");
        assert!(matches!(read, Ok(0)), "{password:?}: {read:?}");
    }
}

/// Go `TestRedisProtocol_SUBSCRIBE_UsageSendsSupportRefresh`.
#[tokio::test]
async fn subscribe_usage_sends_support_refresh() {
    let s = server(Some(PASSWORD)).await;
    let mut conn = connect(s.addr).await;
    auth(&mut conn).await;
    command(&mut conn, &["SUBSCRIBE", "usage"]).await;
    assert_eq!(
        pubsub(&mut conn).await,
        ("subscribe".into(), "usage".into(), ":1".into())
    );
    assert_eq!(
        pubsub(&mut conn).await,
        ("message".into(), "usage".into(), r#"{"support_refresh":true}"#.into())
    );
    s.rt.usage_queue().enqueue(br#"{"id":1}"#.to_vec());
    assert_eq!(
        pubsub(&mut conn).await,
        ("message".into(), "usage".into(), r#"{"id":1}"#.into())
    );
    assert!(
        s.rt.usage_queue().pop_oldest(5).is_empty(),
        "a subscriber takes the record instead of the queue"
    );
    // Subscribed: PING answers pong, UNSUBSCRIBE ends the session.
    command(&mut conn, &["PING", "hi"]).await;
    assert_eq!(array_len(&mut conn).await, 2);
    assert_eq!(bulk(&mut conn).await.as_deref(), Some("pong"));
    assert_eq!(bulk(&mut conn).await.as_deref(), Some("hi"));
    command(&mut conn, &["UNSUBSCRIBE"]).await;
    assert_eq!(
        pubsub(&mut conn).await,
        ("unsubscribe".into(), "usage".into(), ":0".into())
    );
}

/// Go `TestRedisProtocol_SUBSCRIBE_ErrorsReceivesErrorEvents`.
#[tokio::test]
async fn subscribe_errors_receives_error_events() {
    let s = server(Some(PASSWORD)).await;
    let mut conn = connect(s.addr).await;
    auth(&mut conn).await;
    command(&mut conn, &["SUBSCRIBE", "errors"]).await;
    assert_eq!(
        pubsub(&mut conn).await,
        ("subscribe".into(), "errors".into(), ":1".into())
    );
    // The subscribe reply is written after the subscription is registered.
    s.rt.usage_queue()
        .enqueue_error(br#"{"auth_index":"auth-1","status_code":401}"#);
    assert_eq!(
        pubsub(&mut conn).await,
        (
            "message".into(),
            "errors".into(),
            r#"{"auth_index":"auth-1","status_code":401}"#.into()
        )
    );
}

/// Go `TestRedisProtocol_AUTH_And_PopContracts`, plus the NOAUTH and bad-key replies.
#[tokio::test]
async fn auth_and_pop_contracts() {
    let s = server(Some(PASSWORD)).await;
    let mut conn = connect(s.addr).await;
    command(&mut conn, &["LPOP", "usage"]).await;
    assert_eq!(line(&mut conn).await, "-NOAUTH Authentication required.");
    command(&mut conn, &["AUTH", "wrong"]).await;
    assert_eq!(line(&mut conn).await, "-ERR invalid management key");
    auth(&mut conn).await;
    for item in ["a", "b", "c"] {
        s.rt.usage_queue().enqueue(item.as_bytes().to_vec());
    }
    command(&mut conn, &["RPOP", "usage"]).await;
    assert_eq!(bulk(&mut conn).await.as_deref(), Some("a"));
    command(&mut conn, &["LPOP", "usage"]).await;
    assert_eq!(bulk(&mut conn).await.as_deref(), Some("b"));
    command(&mut conn, &["RPOP", "usage", "10"]).await;
    assert_eq!(array_len(&mut conn).await, 1);
    assert_eq!(bulk(&mut conn).await.as_deref(), Some("c"));
    command(&mut conn, &["LPOP", "usage"]).await;
    assert_eq!(bulk(&mut conn).await, None);
    command(&mut conn, &["RPOP", "usage", "2"]).await;
    assert_eq!(array_len(&mut conn).await, 0);
    command(&mut conn, &["RPOP", "errors", "2"]).await;
    assert_eq!(line(&mut conn).await, "-ERR unsupported channel 'errors'");
    command(&mut conn, &["LPOP", "usage", "x"]).await;
    assert_eq!(line(&mut conn).await, "-ERR value is not an integer or out of range");
    command(&mut conn, &["GET", "k"]).await;
    assert_eq!(line(&mut conn).await, "-ERR unknown command 'get'");
}

/// Go `TestAcceptMuxNotBlockedByIdleConnection`: a client that never sends a byte
/// does not hold up others, and HTTP shares the port with RESP.
#[tokio::test]
async fn idle_connection_does_not_block_http() {
    let s = server(Some(PASSWORD)).await;
    let _idle = TcpStream::connect(s.addr).await.unwrap();
    let res = tokio::time::timeout(
        Duration::from_secs(3),
        wreq::Client::new().get(format!("http://{}/healthz", s.addr)).send(),
    )
    .await
    .expect("not blocked")
    .unwrap();
    assert_eq!(res.status().as_u16(), 200);
}

/// A failed attempt publishes Go's error event to `errors` subscribers
/// (sdk/cliproxy/auth/error_events.go); the payload shape is checked against Go in
/// error_events.rs.
#[tokio::test]
async fn failed_attempt_reaches_errors_subscribers() {
    let mut metadata = serde_json::Map::new();
    metadata.insert("type".into(), "claude".into());
    metadata.insert("access_token".into(), "fake-token".into());
    metadata.insert("expired".into(), "2099-01-01T00:00:00Z".into());
    let mut credential = cpa_core::credential::Credential::from_file(
        std::path::Path::new("/fake"),
        std::path::Path::new("/fake/a.json"),
        metadata,
    )
    .unwrap();
    // Nothing listens on the discard port: the attempt fails at connect.
    credential
        .attributes
        .insert("base_url".into(), "http://127.0.0.1:9".into());
    let s = server_with(Some(PASSWORD), vec![credential]).await;
    let mut conn = connect(s.addr).await;
    auth(&mut conn).await;
    command(&mut conn, &["SUBSCRIBE", "errors"]).await;
    assert_eq!(pubsub(&mut conn).await.0, "subscribe");
    let res = wreq::Client::new()
        .post(format!("http://{}/v1/messages", s.addr))
        .body(r#"{"model":"claude-sonnet-4-6","max_tokens":5,"messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap();
    assert!(res.status().is_server_error(), "{}", res.status());
    let (kind, channel, payload) = pubsub(&mut conn).await;
    assert_eq!((kind.as_str(), channel.as_str()), ("message", "errors"));
    let event: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(event["provider"], "claude");
    assert_eq!(event["auth_id"], "a.json");
    assert_eq!(event["model"], "claude-sonnet-4-6");
    // Go `errorEventStatusCode`: a failure without an HTTP status reports 500.
    assert_eq!(event["status_code"], 500, "{event}");
    assert!(event["auth_status"].is_object(), "{event}");
}

// Command bounds (#102 I2; Go v8.0.67 888f2382 added its own): checked counts and
// lengths, bounded lines and a per-command byte budget, refused with one `ERR` reply
// and a graceful close (`cpa_server::resp`, docs/DIFFERENCES-FROM-GO.md "RESP command
// bounds").

/// Allocation bytes counted on one thread while [`measure`] runs. A `#[tokio::test]`
/// runtime is single-threaded, so the listener's connection tasks run on the test's
/// thread and nothing from tests running in parallel is counted.
struct ThreadCounting;

thread_local! {
    static COUNTING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static LIVE: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    static PEAK: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
}

fn counted(delta: isize) {
    if COUNTING.with(std::cell::Cell::get) {
        let live = LIVE.with(|l| {
            l.set(l.get() + delta);
            l.get()
        });
        PEAK.with(|p| p.set(p.get().max(live)));
    }
}

// SAFETY: every call is forwarded to `System`; the counters are const thread-locals,
// which neither allocate nor reenter the allocator.
unsafe impl std::alloc::GlobalAlloc for ThreadCounting {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        counted(layout.size() as isize);
        unsafe { std::alloc::System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        counted(layout.size() as isize);
        unsafe { std::alloc::System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        counted(-(layout.size() as isize));
        unsafe { std::alloc::System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        counted(new_size as isize - layout.size() as isize);
        unsafe { std::alloc::System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: ThreadCounting = ThreadCounting;

/// The peak heap growth on this thread while `work` runs.
async fn measure<F: std::future::Future<Output = ()>>(work: F) -> isize {
    LIVE.with(|l| l.set(0));
    PEAK.with(|p| p.set(0));
    COUNTING.with(|c| c.set(true));
    work.await;
    COUNTING.with(|c| c.set(false));
    PEAK.with(std::cell::Cell::get)
}

const MIB: usize = 1024 * 1024;
/// `cpa_server::resp::MAX_BULK_BYTES`, the shared 64 MiB request limit.
const MAX_BULK: usize = 64 * MIB;

fn bulk_frame(payload: &[u8]) -> Vec<u8> {
    let mut out = format!("${}\r\n", payload.len()).into_bytes();
    out.extend_from_slice(payload);
    out.extend_from_slice(b"\r\n");
    out
}

/// The server refuses with `want`, then closes with a FIN: the reply arrives whole and
/// the next read is a clean end, not a reset.
async fn refused(conn: &mut BufReader<TcpStream>, want: &str) {
    assert_eq!(line(conn).await, want);
    let mut rest = Vec::new();
    let end = tokio::time::timeout(Duration::from_secs(5), conn.read_to_end(&mut rest))
        .await
        .expect("closed in time");
    assert!(
        matches!(end, Ok(0)),
        "graceful close, got {end:?} after {} bytes",
        rest.len()
    );
}

/// A new connection after a refusal still authenticates and pops, and HTTP answers.
async fn still_healthy(s: &Server) {
    let mut conn = connect(s.addr).await;
    auth(&mut conn).await;
    command(&mut conn, &["LPOP", "usage"]).await;
    assert_eq!(bulk(&mut conn).await, None);
    let health = wreq::Client::new()
        .get(format!("http://{}/healthz", s.addr))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status().as_u16(), 200);
}

#[tokio::test]
async fn oversized_bulk_is_refused_before_its_payload() {
    let s = server(Some(PASSWORD)).await;
    let mut conn = connect(s.addr).await;
    let mut out = format!("*2\r\n$4\r\nAUTH\r\n${}\r\n", MAX_BULK + 1).into_bytes();
    // The start of the payload is already in flight; the refusal must survive it.
    out.extend(std::iter::repeat_n(b'a', 256 * 1024));
    conn.get_mut().write_all(&out).await.unwrap();
    refused(&mut conn, "-ERR bulk string too large").await;
    still_healthy(&s).await;
}

#[tokio::test]
async fn counts_and_lengths_are_checked_up_front() {
    let s = server(Some(PASSWORD)).await;
    for input in [
        "*1025\r\n",
        "*9223372036854775807\r\n",
        "*-1\r\n",
        "*1\r\n$-2\r\n",
        "*1\r\n$99999999999999999999\r\n",
    ] {
        let mut conn = connect(s.addr).await;
        conn.get_mut().write_all(input.as_bytes()).await.unwrap();
        refused(&mut conn, "-ERR protocol error").await;
    }
    still_healthy(&s).await;
}

#[tokio::test]
async fn long_lines_are_refused_at_the_line_limit() {
    let s = server(Some(PASSWORD)).await;
    for head in ["*", "*1\r\n+", "*1\r\n$"] {
        let mut conn = connect(s.addr).await;
        let mut out = head.as_bytes().to_vec();
        out.extend(std::iter::repeat_n(b'1', 70 * 1024));
        conn.get_mut().write_all(&out).await.unwrap();
        refused(&mut conn, "-ERR protocol error").await;
    }
    still_healthy(&s).await;
}

#[tokio::test]
async fn many_valid_bulks_stop_at_the_command_budget() {
    let s = server(Some(PASSWORD)).await;
    let mut conn = connect(s.addr).await;
    // Each fits the bulk ceiling; the second no longer fits what is left of the budget,
    // so it is refused at its length line, before its payload.
    let mut out = b"*3\r\n".to_vec();
    out.extend_from_slice(&bulk_frame(&vec![b'a'; 33 * MIB]));
    out.extend_from_slice(format!("${}\r\n", 33 * MIB).as_bytes());
    conn.get_mut().write_all(&out).await.unwrap();
    refused(&mut conn, "-ERR bulk string too large").await;
    still_healthy(&s).await;
}

/// Regression control: the 64 MiB ceiling still admits a maximal argument, and two
/// such commands pipelined together each get their own budget.
#[tokio::test]
async fn maximum_bulk_is_admitted_and_each_command_has_its_own_budget() {
    let s = server(Some(PASSWORD)).await;
    let conn = connect(s.addr).await;
    let mut one = b"*2\r\n$4\r\nPING\r\n".to_vec();
    one.extend_from_slice(&bulk_frame(&vec![b'p'; MAX_BULK]));
    let mut out = one.clone();
    out.extend_from_slice(&one);
    let (mut read, mut write) = conn.into_inner().into_split();
    let writer = tokio::spawn(async move {
        write.write_all(&out).await.unwrap();
        write
    });
    let mut replies = BufReader::new(&mut read);
    for _ in 0..2 {
        let mut reply = String::new();
        tokio::time::timeout(Duration::from_secs(30), replies.read_line(&mut reply))
            .await
            .expect("reply in time")
            .unwrap();
        assert_eq!(reply, "-NOAUTH Authentication required.\r\n");
    }
    let mut conn = BufReader::new(read.reunite(writer.await.unwrap()).unwrap());
    auth(&mut conn).await;
    command(&mut conn, &["LPOP", "usage"]).await;
    assert_eq!(bulk(&mut conn).await, None);
}

/// Regression control: a command cut off by the client's FIN is still Go's
/// `unexpected EOF`, then a clean close.
#[tokio::test]
async fn truncated_command_ends_with_unexpected_eof() {
    let s = server(Some(PASSWORD)).await;
    let mut conn = connect(s.addr).await;
    conn.get_mut()
        .write_all(b"*2\r\n$4\r\nAUTH\r\n$10\r\nabc")
        .await
        .unwrap();
    conn.get_mut().shutdown().await.unwrap();
    refused(&mut conn, "-ERR unexpected EOF").await;
}

#[tokio::test]
async fn subscription_commands_are_bounded_too() {
    let s = server(Some(PASSWORD)).await;
    let mut conn = connect(s.addr).await;
    auth(&mut conn).await;
    // `errors` pushes nothing until an error is queued, unlike `usage`.
    command(&mut conn, &["SUBSCRIBE", "errors"]).await;
    assert_eq!(pubsub(&mut conn).await.0, "subscribe");
    command(&mut conn, &["PING", "x"]).await;
    assert_eq!(array_len(&mut conn).await, 2);
    assert_eq!(bulk(&mut conn).await.as_deref(), Some("pong"));
    assert_eq!(bulk(&mut conn).await.as_deref(), Some("x"));
    conn.get_mut().write_all(b"*1025\r\n$0\r\n\r\n").await.unwrap();
    refused(&mut conn, "-ERR protocol error").await;
    still_healthy(&s).await;
}

/// The server holds what arrived, not what a client declares or keeps sending: a
/// 64 MiB length with three bytes behind it, and a line that never ends.
#[tokio::test]
async fn declared_lengths_and_endless_lines_allocate_only_what_arrived() {
    // Positive control: the probe sees an allocation made on this thread.
    let control = measure(async {
        drop(std::hint::black_box(vec![0u8; 2 * MIB]));
    })
    .await;
    assert!(control >= 2 * MIB as isize, "probe saw {control} bytes");
    let s = server(Some(PASSWORD)).await;
    let mut conn = connect(s.addr).await;
    auth(&mut conn).await;
    let declared = format!("*2\r\n$4\r\nPING\r\n${MAX_BULK}\r\nabc").into_bytes();
    let grown = measure(async {
        conn.get_mut().write_all(&declared).await.unwrap();
        conn.get_mut().shutdown().await.unwrap();
        // The reply proves the server read the length and the three bytes.
        assert_eq!(line(&mut conn).await, "-ERR unexpected EOF");
    })
    .await;
    eprintln!("declared 64 MiB bulk, 3 bytes sent: peak heap growth {grown} bytes");
    assert!(
        grown < MIB as isize,
        "a declared 64 MiB bulk grew the heap by {grown} bytes"
    );

    // `*` routes the connection to RESP; the count line never ends.
    let mut endless = vec![b'1'; 4 * MIB];
    endless[0] = b'*';
    let mut conn = connect(s.addr).await;
    let mut reply = String::new();
    let grown = measure(async {
        let (mut read, mut write) = conn.get_mut().split();
        let send = async {
            // The server stops reading after the line limit and a bounded drain.
            let _ = write.write_all(&endless).await;
        };
        let receive = async {
            let _ = BufReader::new(&mut read).read_line(&mut reply).await;
        };
        let _ = tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(send, receive) }).await;
    })
    .await;
    eprintln!("4 MiB unterminated count line: peak heap growth {grown} bytes");
    assert!(
        grown < MIB as isize,
        "a 4 MiB unterminated line grew the heap by {grown} bytes"
    );
    assert_eq!(reply, "-ERR protocol error\r\n");
}
