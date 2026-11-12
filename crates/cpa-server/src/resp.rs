//! Go's Redis protocol on the main listener (internal/api/redis_queue_protocol.go):
//! management clients `AUTH` with the management key, then `LPOP`/`RPOP` queued usage
//! records or `SUBSCRIBE` to the `usage` and `errors` channels. The listener routes a
//! connection here when its first byte is a RESP type prefix.
//!
//! In Home mode (a remote dispatcher is installed) Go refuses these connections with
//! one error before reading a command.
//!
//! Each command is read within fixed bounds (Go v8.0.67 888f2382 added its own): an
//! argument count, a line length, the bulk ceiling and a per-command byte budget. A
//! refused command gets one `ERR` reply and a graceful close; see [`MAX_COMMAND_BYTES`].

use cpa_core::gostr::GoStr;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};

use crate::management::Management;

/// Go `isRedisRESPPrefix`.
pub(crate) fn is_resp_prefix(byte: u8) -> bool {
    matches!(byte, b'*' | b'$' | b'+' | b'-' | b':')
}

/// At most this many arguments in one command (Go v8.0.67 `maxRESPArrayLen`). The
/// commands served here take at most three; the cap bounds the per-argument overhead
/// that empty bulks would otherwise buy at six wire bytes each.
pub(crate) const MAX_ARRAY_ENTRIES: usize = 1024;
/// At most this many bytes in a count or length line or a simple or integer argument,
/// without its CRLF (Go v8.0.67 `maxRESPLineLen`).
pub(crate) const MAX_LINE_BYTES: usize = 64 * 1024;
/// At most this many payload bytes in one bulk string: the server's shared request
/// limit, as before. Go v8.0.67 allows 64 KiB; no compatibility evidence justified
/// lowering the existing ceiling, so it stays.
pub(crate) const MAX_BULK_BYTES: usize = crate::MAX_REQUEST_BYTES;
/// Every byte of one command counts against this budget, from its `*` to its last
/// CRLF, plus what lossy UTF-8 decoding adds. It admits one bulk of
/// [`MAX_BULK_BYTES`] with 64 KiB for the prefixes, length lines and other arguments,
/// and it starts again for each command, so pipelined commands do not share it.
/// Go v8.0.67 `maxRESPFrameBytes` is 64 KiB and counts payload bytes only.
pub(crate) const MAX_COMMAND_BYTES: usize = MAX_BULK_BYTES + 64 * 1024;
/// A bulk payload buffer starts at this size and doubles as bytes arrive, up to the
/// declared length, so a declared length alone allocates no more than this.
const BULK_CHUNK: usize = 64 * 1024;
/// After refusing a command: what the client already sent is read and dropped, up to
/// this many bytes and for at most [`DRAIN_TIME`], so closing the socket with unread
/// input does not reset the connection and discard the error reply.
const DRAIN_BYTES: u64 = 1024 * 1024;
const DRAIN_TIME: std::time::Duration = std::time::Duration::from_secs(1);

/// Why a command could not be read. A clean end (Go `io.EOF`) gets no reply.
#[derive(Debug)]
enum ReadError {
    Eof,
    UnexpectedEof,
    Protocol,
    TooLarge,
    Io(std::io::Error),
}

impl ReadError {
    /// The `ERR` text Go writes, or `None` at a clean end.
    fn message(&self) -> Option<String> {
        match self {
            ReadError::Eof => None,
            ReadError::UnexpectedEof => Some("unexpected EOF".into()),
            ReadError::Protocol => Some("protocol error".into()),
            ReadError::TooLarge => Some("bulk string too large".into()),
            ReadError::Io(e) => Some(e.to_string()),
        }
    }
}

/// Go `strconv.Atoi`.
fn atoi(s: &str) -> Option<i64> {
    s.parse().ok()
}

/// One command being read, with what is left of its [`MAX_COMMAND_BYTES`] budget.
/// Every read is bounded before it happens: nothing is buffered or allocated beyond
/// the budget, a line beyond [`MAX_LINE_BYTES`], or a payload beyond what arrived.
struct Command<'a, R> {
    reader: &'a mut BufReader<R>,
    remaining: usize,
}

impl<R: AsyncRead + Unpin> Command<'_, R> {
    fn charge(&mut self, bytes: usize, over: ReadError) -> Result<(), ReadError> {
        self.remaining = self.remaining.checked_sub(bytes).ok_or(over)?;
        Ok(())
    }

    /// A type prefix byte; the end of the stream is Go's `io.EOF`.
    async fn prefix(&mut self) -> Result<u8, ReadError> {
        self.charge(1, ReadError::Protocol)?;
        match self.reader.read_u8().await {
            Ok(b) => Ok(b),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(ReadError::Eof),
            Err(e) => Err(ReadError::Io(e)),
        }
    }

    /// Go `readRESPLine`: up to `\n`, without `\n` and one `\r`. A line cut off by the
    /// end of the stream is `io.EOF`, as `bufio.Reader` reports it. A line longer than
    /// [`MAX_LINE_BYTES`], or than the budget, is refused once that many bytes arrived.
    async fn line(&mut self) -> Result<String, ReadError> {
        let limit = (MAX_LINE_BYTES + 2).min(self.remaining);
        let mut buf = Vec::new();
        loop {
            let available = self.reader.fill_buf().await.map_err(ReadError::Io)?;
            if available.is_empty() {
                return Err(ReadError::Eof);
            }
            let (take, done) = match available.iter().position(|&b| b == b'\n') {
                Some(i) => (i + 1, true),
                None => (available.len(), false),
            };
            if buf.len() + take > limit {
                return Err(ReadError::Protocol);
            }
            buf.extend_from_slice(&available[..take]);
            self.reader.consume(take);
            if done {
                break;
            }
        }
        self.charge(buf.len(), ReadError::Protocol)?;
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
        if buf.len() > MAX_LINE_BYTES {
            return Err(ReadError::Protocol);
        }
        self.text(buf, ReadError::Protocol)
    }

    /// Go `readRESPBulkString`. `$-1` is the empty string; a length below -1 is a
    /// protocol error, as in Go v8.0.67 and the RESP specification.
    async fn bulk(&mut self) -> Result<String, ReadError> {
        let length = atoi(&self.line().await?).ok_or(ReadError::Protocol)?;
        if length < -1 {
            return Err(ReadError::Protocol);
        }
        let Ok(length) = usize::try_from(length) else {
            return Ok(String::new());
        };
        if length > MAX_BULK_BYTES {
            return Err(ReadError::TooLarge);
        }
        let wire = length + 2;
        self.charge(wire, ReadError::TooLarge)?;
        // Go `io.ReadFull`: EOF when nothing arrived, "unexpected EOF" when part did.
        let mut buf = Vec::with_capacity(wire.min(BULK_CHUNK));
        while buf.len() < wire {
            if buf.len() == buf.capacity() {
                let target = (buf.capacity() * 2).min(wire);
                buf.reserve_exact(target - buf.len());
            }
            // Never past this bulk: the next command's bytes stay in the reader.
            let room = (buf.capacity().min(wire) - buf.len()) as u64;
            match (&mut *self.reader)
                .take(room)
                .read_buf(&mut buf)
                .await
                .map_err(ReadError::Io)?
            {
                0 if buf.is_empty() => return Err(ReadError::Eof),
                0 => return Err(ReadError::UnexpectedEof),
                _ => {}
            }
        }
        if &buf[length..] != b"\r\n" {
            return Err(ReadError::Protocol);
        }
        buf.truncate(length);
        self.text(buf, ReadError::TooLarge)
    }

    /// The argument as text, reusing `bytes` when they are UTF-8. Otherwise each invalid
    /// sequence becomes U+FFFD, as before; the bytes that adds are charged first, so the
    /// decoded copy stays within the budget too.
    fn text(&mut self, bytes: Vec<u8>, over: ReadError) -> Result<String, ReadError> {
        let bytes = match String::from_utf8(bytes) {
            Ok(text) => return Ok(text),
            Err(e) => e.into_bytes(),
        };
        let mut decoded = 0usize;
        for chunk in bytes.utf8_chunks() {
            decoded += chunk.valid().len();
            if !chunk.invalid().is_empty() {
                decoded += char::REPLACEMENT_CHARACTER.len_utf8();
            }
        }
        self.charge(decoded.saturating_sub(bytes.len()), over)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// Go `readRESPArray`: one command as an array of bulk or simple strings, within the
/// bounds above.
async fn read_command<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> Result<Vec<String>, ReadError> {
    let mut command = Command {
        reader,
        remaining: MAX_COMMAND_BYTES,
    };
    if command.prefix().await? != b'*' {
        return Err(ReadError::Protocol);
    }
    let count = atoi(&command.line().await?)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| *n <= MAX_ARRAY_ENTRIES)
        .ok_or(ReadError::Protocol)?;
    let mut args = Vec::with_capacity(count);
    for _ in 0..count {
        args.push(match command.prefix().await? {
            b'$' => command.bulk().await?,
            b'+' | b':' => command.line().await?,
            _ => return Err(ReadError::Protocol),
        });
    }
    Ok(args)
}

/// After a refused command, once its reply is flushed: a FIN, then a bounded drain of
/// what the client already sent (see [`DRAIN_BYTES`]).
async fn close_after_error<R, W>(reader: &mut BufReader<R>, writer: &mut BufWriter<W>)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let _ = writer.shutdown().await;
    drain(reader).await;
}

async fn drain<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) {
    let mut input = (&mut *reader).take(DRAIN_BYTES);
    let _ = tokio::time::timeout(DRAIN_TIME, tokio::io::copy(&mut input, &mut tokio::io::sink())).await;
}

/// RESP replies, buffered until [`flush`].
struct Reply(Vec<u8>);

impl Reply {
    fn simple(&mut self, value: &str) {
        self.0.extend_from_slice(format!("+{value}\r\n").as_bytes());
    }

    fn error(&mut self, message: &str) {
        self.0.extend_from_slice(format!("-{message}\r\n").as_bytes());
    }

    fn bulk(&mut self, payload: Option<&[u8]>) {
        match payload {
            None => self.0.extend_from_slice(b"$-1\r\n"),
            Some(p) => {
                self.0.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
                self.0.extend_from_slice(p);
                self.0.extend_from_slice(b"\r\n");
            }
        }
    }

    fn array(&mut self, count: usize) {
        self.0.extend_from_slice(format!("*{count}\r\n").as_bytes());
    }

    fn integer(&mut self, value: i64) {
        self.0.extend_from_slice(format!(":{value}\r\n").as_bytes());
    }

    /// Go `writeRedisPubSubSubscribe` / `Unsubscribe` / `Message`.
    fn pubsub(&mut self, kind: &str, channel: &str, last: PubSub<'_>) {
        self.array(3);
        self.bulk(Some(kind.as_bytes()));
        self.bulk(Some(channel.as_bytes()));
        match last {
            PubSub::Count(n) => self.integer(n),
            PubSub::Payload(p) => self.bulk(Some(p)),
        }
    }
}

enum PubSub<'a> {
    Count(i64),
    Payload(&'a [u8]),
}

/// Writes and flushes what `reply` holds; false when the client is gone.
async fn flush<W: AsyncWrite + Unpin>(writer: &mut BufWriter<W>, reply: &mut Reply) -> bool {
    let ok = writer.write_all(&reply.0).await.is_ok() && writer.flush().await.is_ok();
    reply.0.clear();
    if !ok {
        tracing::error!("redis protocol flush error");
    }
    ok
}

/// Go `resolveRemoteIP`: the peer address, IPv4-mapped addresses unmapped.
fn remote_ip(peer: SocketAddr) -> (String, bool) {
    let ip = match peer.ip() {
        std::net::IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map_or(std::net::IpAddr::V6(v6), std::net::IpAddr::V4),
        ip => ip,
    };
    let host = ip.to_string();
    let local = host == "127.0.0.1" || host == "::1";
    (host, local)
}

/// Go `handleRedisConnection`.
pub(crate) async fn serve<IO>(io: IO, peer: SocketAddr, mgmt: Arc<Management>)
where
    IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (read, write) = tokio::io::split(io);
    let mut reader = BufReader::new(read);
    let mut writer = BufWriter::new(write);
    let mut reply = Reply(Vec::new());
    let (ip, local) = remote_ip(peer);
    let mut authed = false;
    // Go reports a ban instead of NOAUTH when an empty key check says so.
    let banned_or = |result: Result<(), (axum::http::StatusCode, String)>, otherwise: &str| match result {
        Err((status, message))
            if status == axum::http::StatusCode::FORBIDDEN
                && message.starts_with("IP banned due to too many failed attempts") =>
        {
            format!("ERR {message}")
        }
        _ => otherwise.to_owned(),
    };
    if mgmt.rt.remote_dispatch().is_some() {
        // Go's listener already buffered what arrived with the first byte (bufio
        // `Peek`); reading it too keeps the close a FIN instead of a reset.
        let _ = reader.fill_buf().await;
        reply.error("ERR redis usage output disabled in home mode");
        let _ = writer.write_all(&reply.0).await;
        let _ = writer.flush().await;
        return;
    }
    loop {
        if !mgmt.routes_enabled() {
            return;
        }
        let args = match read_command(&mut reader).await {
            Ok(args) => args,
            Err(e) => {
                if let Some(message) = e.message() {
                    reply.error(&format!("ERR {message}"));
                    if flush(&mut writer, &mut reply).await {
                        close_after_error(&mut reader, &mut writer).await;
                    }
                }
                return;
            }
        };
        let Some(first) = args.first() else {
            reply.error("ERR empty command");
            if !flush(&mut writer, &mut reply).await {
                return;
            }
            continue;
        };
        let cmd = first.trim().go_upper();
        if cmd != "AUTH" && !authed {
            let result = mgmt.authenticate_key(&ip, local, b"").await;
            reply.error(&banned_or(result, "NOAUTH Authentication required."));
            if !flush(&mut writer, &mut reply).await {
                return;
            }
            continue;
        }
        match cmd.as_str() {
            "AUTH" => {
                // Go `parseAuthPassword`: `AUTH password` or `AUTH user password`.
                let password = match args.len() {
                    2 => Some(&args[1]),
                    3 => Some(&args[2]),
                    _ => None,
                };
                match password {
                    None => {
                        let result = mgmt.authenticate_key(&ip, local, b"").await;
                        reply.error(&banned_or(result, "ERR wrong number of arguments for 'auth' command"));
                    }
                    Some(password) => match mgmt.authenticate_key(&ip, local, password.as_bytes()).await {
                        Ok(()) => {
                            authed = true;
                            reply.simple("OK");
                        }
                        Err((_, message)) => reply.error(&format!("ERR {message}")),
                    },
                }
            }
            "SUBSCRIBE" => {
                if args.len() != 2 {
                    reply.error("ERR wrong number of arguments for 'subscribe' command");
                } else {
                    let channel = args[1].trim().to_owned();
                    let queue = mgmt.rt.usage_queue();
                    let subscription = match channel.go_lower().as_str() {
                        "usage" => queue.subscribe_usage(),
                        "errors" => queue.subscribe_errors(),
                        _ => {
                            reply.error(&format!("ERR unsupported channel '{channel}'"));
                            if !flush(&mut writer, &mut reply).await {
                                return;
                            }
                            continue;
                        }
                    };
                    reply.pubsub("subscribe", &channel, PubSub::Count(1));
                    if flush(&mut writer, &mut reply).await {
                        stream(reader, writer, &channel, subscription).await;
                    }
                    return;
                }
            }
            "LPOP" | "RPOP" => {
                // Go `parsePopCount`: a count that does not parse is 0.
                let count = match args.len() {
                    2 => Some((1, false)),
                    3 => Some((atoi(args[2].trim()).unwrap_or(0), true)),
                    _ => None,
                };
                match count {
                    None => reply.error(&format!(
                        "ERR wrong number of arguments for '{}' command",
                        cmd.go_lower()
                    )),
                    Some((count, _)) if count <= 0 => reply.error("ERR value is not an integer or out of range"),
                    Some((count, has_count)) => {
                        let channel = args[1].trim();
                        if !channel.eq_ignore_ascii_case("usage") {
                            reply.error(&format!("ERR unsupported channel '{channel}'"));
                        } else {
                            let items = mgmt.rt.usage_queue().pop_oldest(count as usize);
                            if has_count {
                                reply.array(items.len());
                                for item in &items {
                                    reply.bulk(Some(item));
                                }
                            } else {
                                reply.bulk(items.first().map(Vec::as_slice));
                            }
                        }
                    }
                }
            }
            _ => reply.error(&format!("ERR unknown command '{}'", cmd.go_lower())),
        }
        if !flush(&mut writer, &mut reply).await {
            return;
        }
    }
}

/// Go `streamRedisSubscription`: channel messages out, `PING`, `UNSUBSCRIBE` and
/// `QUIT` in, until either side ends.
async fn stream<R, W>(
    mut reader: BufReader<R>,
    mut writer: BufWriter<W>,
    channel: &str,
    mut subscription: crate::usage::Subscription,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin,
{
    let (tx, mut commands) = tokio::sync::mpsc::channel::<Result<Vec<String>, String>>(1);
    let read_task = tokio::spawn(async move {
        loop {
            match read_command(&mut reader).await {
                Ok(args) => {
                    if tx.send(Ok(args)).await.is_err() {
                        return;
                    }
                }
                Err(e) => {
                    if let Some(message) = e.message() {
                        let _ = tx.send(Err(message)).await;
                        drain(&mut reader).await;
                    }
                    return;
                }
            }
        }
    });
    let mut reply = Reply(Vec::new());
    let mut read_failed = false;
    loop {
        tokio::select! {
            message = subscription.messages.recv() => {
                let Some(payload) = message else { break };
                reply.pubsub("message", channel, PubSub::Payload(&payload));
                if !flush(&mut writer, &mut reply).await {
                    break;
                }
            }
            command = commands.recv() => {
                let Some(command) = command else { break };
                let keep_open = match command {
                    Err(message) => {
                        reply.error(&format!("ERR {message}"));
                        read_failed = true;
                        false
                    }
                    Ok(args) => match args.first().map(|c| c.trim().go_upper()) {
                        None => {
                            reply.error("ERR empty command");
                            true
                        }
                        Some(cmd) if cmd == "PING" => {
                            reply.array(2);
                            reply.bulk(Some(b"pong"));
                            reply.bulk(args.get(1).map(String::as_bytes));
                            true
                        }
                        Some(cmd) if cmd == "UNSUBSCRIBE" => {
                            reply.pubsub("unsubscribe", channel, PubSub::Count(0));
                            false
                        }
                        Some(cmd) if cmd == "QUIT" => {
                            reply.simple("OK");
                            false
                        }
                        Some(cmd) => {
                            reply.error(&format!("ERR unknown command '{}'", cmd.go_lower()));
                            true
                        }
                    },
                };
                if !flush(&mut writer, &mut reply).await || !keep_open {
                    break;
                }
            }
        }
    }
    if read_failed {
        // The reader is draining what the client sent after the refused command
        // (bounded by DRAIN_BYTES and DRAIN_TIME); the FIN goes out first.
        let _ = writer.shutdown().await;
        let _ = read_task.await;
    } else {
        read_task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// `head`, then `tail` repeated forever (or the end of the stream when `tail` is
    /// empty), counting every byte the reader pulls.
    struct Source {
        head: Vec<u8>,
        at: usize,
        tail: Vec<u8>,
        tail_at: usize,
        pulled: usize,
    }

    impl AsyncRead for Source {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let this = &mut *self;
            while buf.remaining() > 0 {
                if this.at < this.head.len() {
                    let n = buf.remaining().min(this.head.len() - this.at);
                    buf.put_slice(&this.head[this.at..this.at + n]);
                    this.at += n;
                    this.pulled += n;
                } else if !this.tail.is_empty() {
                    let n = buf.remaining().min(this.tail.len() - this.tail_at);
                    buf.put_slice(&this.tail[this.tail_at..this.tail_at + n]);
                    this.tail_at = (this.tail_at + n) % this.tail.len();
                    this.pulled += n;
                } else {
                    break;
                }
            }
            Poll::Ready(Ok(()))
        }
    }

    fn reader(head: impl Into<Vec<u8>>, tail: impl Into<Vec<u8>>) -> BufReader<Source> {
        BufReader::new(Source {
            head: head.into(),
            at: 0,
            tail: tail.into(),
            tail_at: 0,
            pulled: 0,
        })
    }

    async fn read(head: impl Into<Vec<u8>>) -> Result<Vec<String>, ReadError> {
        read_command(&mut reader(head, Vec::new())).await
    }

    /// What one read of an endless stream returns, and how many bytes it pulled.
    async fn endless(head: impl Into<Vec<u8>>, tail: impl Into<Vec<u8>>) -> (Result<Vec<String>, ReadError>, usize) {
        let mut r = reader(head, tail);
        let result = read_command(&mut r).await;
        (result, r.get_ref().pulled)
    }

    /// `tokio::io::BufReader`'s default capacity: what one fill may pull ahead.
    const READ_AHEAD: usize = 8 * 1024;

    fn bulk(payload: &[u8]) -> Vec<u8> {
        let mut out = format!("${}\r\n", payload.len()).into_bytes();
        out.extend_from_slice(payload);
        out.extend_from_slice(b"\r\n");
        out
    }

    #[tokio::test]
    async fn array_counts_are_checked_before_any_argument_is_read() {
        for header in [
            "*1025\r\n",
            "*-1\r\n",
            "*9223372036854775807\r\n",
            "*99999999999999999999\r\n",
            "*x\r\n",
        ] {
            // An endless supply of valid empty bulks never gets read.
            let (result, pulled) = endless(header, "$0\r\n\r\n").await;
            assert!(matches!(result, Err(ReadError::Protocol)), "{header:?}: {result:?}");
            assert!(pulled <= READ_AHEAD, "{header:?}: pulled {pulled}");
        }
        let mut max = b"*1024\r\n".to_vec();
        for _ in 0..MAX_ARRAY_ENTRIES {
            max.extend_from_slice(b"$0\r\n\r\n");
        }
        let args = read(max).await.unwrap();
        assert_eq!(args.len(), MAX_ARRAY_ENTRIES);
        assert!(args.iter().all(String::is_empty));
        assert!(read("*0\r\n").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn bulk_lengths_are_checked_before_the_payload() {
        assert_eq!(read("*1\r\n$-1\r\n").await.unwrap(), [""]);
        for (header, refused) in [
            ("*1\r\n$-2\r\n", "protocol error"),
            ("*1\r\n$-9223372036854775808\r\n", "protocol error"),
            ("*1\r\n$99999999999999999999\r\n", "protocol error"),
            ("*1\r\n$9223372036854775807\r\n", "bulk string too large"),
            ("*1\r\n$67108865\r\n", "bulk string too large"),
        ] {
            let (result, pulled) = endless(header, "a").await;
            assert_eq!(result.unwrap_err().message().as_deref(), Some(refused), "{header:?}");
            assert!(pulled <= READ_AHEAD, "{header:?}: pulled {pulled}");
        }
        assert_eq!(MAX_BULK_BYTES, 67_108_864);
        assert!(matches!(read("*1\r\n$3\r\nabcXY").await, Err(ReadError::Protocol)));
    }

    #[tokio::test]
    async fn lines_are_refused_once_they_pass_the_line_limit() {
        // Count line, simple argument, integer argument and bulk length line alike.
        for head in ["*", "*1\r\n+", "*1\r\n:", "*1\r\n$"] {
            let (result, pulled) = endless(head, "1").await;
            assert!(matches!(result, Err(ReadError::Protocol)), "{head:?}: {result:?}");
            assert!(
                pulled <= head.len() + MAX_LINE_BYTES + 2 + READ_AHEAD,
                "{head:?}: pulled {pulled}"
            );
        }
        let line = |content: usize, end: &str| format!("*1\r\n+{}{end}", "a".repeat(content));
        assert_eq!(
            read(line(MAX_LINE_BYTES, "\r\n")).await.unwrap()[0].len(),
            MAX_LINE_BYTES
        );
        assert_eq!(read(line(MAX_LINE_BYTES, "\n")).await.unwrap()[0].len(), MAX_LINE_BYTES);
        assert!(matches!(
            read(line(MAX_LINE_BYTES + 1, "\r\n")).await,
            Err(ReadError::Protocol)
        ));
        assert!(matches!(
            read(line(MAX_LINE_BYTES + 1, "\n")).await,
            Err(ReadError::Protocol)
        ));
    }

    #[tokio::test]
    async fn many_valid_bulks_stop_at_the_command_budget() {
        let one = bulk(&vec![b'a'; 1024 * 1024]);
        let (result, pulled) = endless("*1024\r\n", one.clone()).await;
        assert!(matches!(result, Err(ReadError::TooLarge)), "{result:?}");
        // Refused at the first length that no longer fits, before its payload.
        assert!(pulled <= MAX_COMMAND_BYTES + READ_AHEAD, "pulled {pulled}");
        assert!(pulled > MAX_COMMAND_BYTES - one.len(), "pulled {pulled}");
        // Simple arguments count too: no bypass around the bulk check.
        let mut head = b"*2\r\n".to_vec();
        head.extend_from_slice(&bulk(&vec![b'a'; MAX_BULK_BYTES]));
        let (result, _) = endless(head, "+").await;
        assert!(matches!(result, Err(ReadError::Protocol)), "{result:?}");
    }

    #[tokio::test]
    async fn the_budget_admits_one_maximum_bulk_with_its_framing() {
        let mut head = b"*2\r\n".to_vec();
        head.extend_from_slice(&bulk(&vec![b'a'; MAX_BULK_BYTES]));
        head.push(b'+');
        // Every byte so far, the last line's CRLF, and the rest is the largest line left.
        let room = MAX_COMMAND_BYTES - head.len() - 2;
        assert!(room < MAX_LINE_BYTES, "the budget, not the line limit, decides here");
        for (content, fits) in [(room, true), (room + 1, false)] {
            let mut command = head.clone();
            command.extend(std::iter::repeat_n(b'b', content));
            command.extend_from_slice(b"\r\n");
            match read(command).await {
                Ok(args) if fits => assert_eq!((args[0].len(), args[1].len()), (MAX_BULK_BYTES, room)),
                Err(ReadError::Protocol) if !fits => {}
                other => panic!("{content}: {:?}", other.map(|a| a.len())),
            }
        }
    }

    #[tokio::test]
    async fn each_pipelined_command_gets_a_fresh_budget() {
        let big = |c: u8| {
            let mut out = b"*1\r\n".to_vec();
            out.extend_from_slice(&bulk(&vec![c; MAX_COMMAND_BYTES / 2 + 1]));
            out
        };
        let mut both = big(b'x');
        both.extend_from_slice(&big(b'y'));
        both.extend_from_slice(b"*1\r\n$4\r\nPING\r\n");
        let mut r = reader(both, Vec::new());
        assert_eq!(read_command(&mut r).await.unwrap()[0].as_bytes()[0], b'x');
        assert_eq!(read_command(&mut r).await.unwrap()[0].as_bytes()[0], b'y');
        assert_eq!(read_command(&mut r).await.unwrap(), ["PING"]);
        assert!(matches!(read_command(&mut r).await, Err(ReadError::Eof)));
    }

    #[tokio::test]
    async fn truncated_commands_end_as_before() {
        for (input, want) in [
            ("", None),
            ("*2\r", None),
            ("*1\r\n", None),
            ("*2\r\n$4\r\nAUTH\r\n$10\r\n", None),
            ("*2\r\n$4\r\nAU", Some("unexpected EOF")),
            ("*2\r\n$4\r\nAUTH\r\n$10\r\nabc", Some("unexpected EOF")),
        ] {
            let error = read(input).await.unwrap_err();
            assert_eq!(error.message().as_deref(), want, "{input:?}");
        }
        assert!(matches!(read("+PING\r\n").await, Err(ReadError::Protocol)));
    }

    #[tokio::test]
    async fn lossy_decoding_is_charged_before_it_allocates() {
        assert_eq!(
            read(b"*2\r\n$1\r\n\xff\r\n+\xff\r\n".to_vec()).await.unwrap(),
            ["\u{FFFD}", "\u{FFFD}"]
        );
        assert_eq!(read("*1\r\n$5\r\n\u{e9}t\u{e9}\r\n").await.unwrap(), ["\u{e9}t\u{e9}"]);
        // Fits on the wire, but each 0xFF decodes to three bytes: over the budget.
        let mut command = b"*1\r\n".to_vec();
        command.extend_from_slice(&bulk(&vec![0xff; MAX_COMMAND_BYTES / 2]));
        assert!(matches!(read(command).await, Err(ReadError::TooLarge)));
    }
}
