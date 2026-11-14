//! FP3 #117: a large upstream message crosses the wire as one text message in frames of at
//! most 1 MiB, as first-party Codex writes it (`codex-rs/codex-api/src/endpoint/
//! responses_websocket.rs` `send_message`, openai/codex d15bf1d, inspected at 5c3ff1d).
//! The loopback upstream parses the client's frames itself, so the assertions do not
//! rest on the WebSocket library that wrote them. Fake credentials, loopback only.

use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MIB: usize = 1 << 20;

/// One client frame as it crossed the wire, unmasked.
#[derive(Debug)]
pub(super) struct WireFrame {
    fin: bool,
    rsv1: bool,
    pub(super) opcode: u8,
    payload: Vec<u8>,
}

#[derive(Clone, Default)]
pub(super) struct Plan {
    /// Agree to permessage-deflate (the proxy reads compressed, never writes compressed).
    pub(super) deflate: bool,
    /// `x-codex-websocket-max-message-bytes` on the 101, as given.
    pub(super) limit: Option<String>,
    /// Stop reading once this many frames arrived, with a small receive buffer.
    pub(super) stall_after: Option<usize>,
    /// Ping once this many frames arrived.
    pub(super) ping_after: Option<usize>,
    /// Answer each finished data message with `response.created` and `.completed`.
    pub(super) answer: bool,
}

/// A WebSocket upstream on raw TCP for one connection; each client frame is reported.
pub(super) async fn raw_upstream(plan: Plan) -> (String, mpsc::UnboundedReceiver<WireFrame>) {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    if plan.stall_after.is_some() {
        socket.set_recv_buffer_size(16 << 10).unwrap();
    }
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = socket.listen(8).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let (mut tcp, _) = listener.accept().await.unwrap();
        accept(&mut tcp, &plan).await;
        let mut count = 0;
        loop {
            if plan.stall_after == Some(count) {
                // Keep the connection open without reading from it.
                std::future::pending::<()>().await;
            }
            let Some(frame) = read_frame(&mut tcp).await else {
                break;
            };
            count += 1;
            if plan.ping_after == Some(count) {
                tcp.write_all(&[0x89, 4, b'p', b'i', b'n', b'g']).await.unwrap();
            }
            let ends_message = frame.fin && frame.opcode < 8;
            let close = frame.opcode == 8;
            if tx.send(frame).is_err() || close {
                break;
            }
            if plan.answer && ends_message {
                for event in [CREATED, COMPLETED] {
                    let mut out = vec![0x81, 126];
                    out.extend_from_slice(&(event.len() as u16).to_be_bytes());
                    out.extend_from_slice(event.as_bytes());
                    tcp.write_all(&out).await.unwrap();
                }
            }
        }
    });
    (url, rx)
}

async fn accept(tcp: &mut tokio::net::TcpStream, plan: &Plan) {
    let mut request = Vec::new();
    let mut buf = [0u8; 4096];
    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = tcp.read(&mut buf).await.unwrap();
        request.extend_from_slice(&buf[..n]);
    }
    let request = String::from_utf8(request).unwrap();
    let key = request
        .lines()
        .find_map(|line| {
            let (k, v) = line.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("sec-websocket-key")
                .then(|| v.trim().to_owned())
        })
        .unwrap();
    let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
    let mut extensions = if plan.deflate {
        format!("Sec-WebSocket-Extensions: {}\r\n", deflate::OFFER)
    } else {
        String::new()
    };
    if let Some(limit) = &plan.limit {
        extensions.push_str(&format!("x-codex-websocket-max-message-bytes: {limit}\r\n"));
    }
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n{extensions}\r\n"
    );
    tcp.write_all(response.as_bytes()).await.unwrap();
}

/// RFC 6455 5.2, client side: always masked.
async fn read_frame(tcp: &mut tokio::net::TcpStream) -> Option<WireFrame> {
    let mut head = [0u8; 2];
    tcp.read_exact(&mut head).await.ok()?;
    assert_ne!(head[1] & 0x80, 0, "client frames are masked");
    let len = match head[1] & 0x7f {
        126 => {
            let mut n = [0u8; 2];
            tcp.read_exact(&mut n).await.ok()?;
            usize::from(u16::from_be_bytes(n))
        }
        127 => {
            let mut n = [0u8; 8];
            tcp.read_exact(&mut n).await.ok()?;
            usize::try_from(u64::from_be_bytes(n)).unwrap()
        }
        n => usize::from(n),
    };
    let mut mask = [0u8; 4];
    tcp.read_exact(&mut mask).await.ok()?;
    let mut payload = vec![0u8; len];
    tcp.read_exact(&mut payload).await.ok()?;
    for (i, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[i % 4];
    }
    Some(WireFrame {
        fin: head[0] & 0x80 != 0,
        rsv1: head[0] & 0x40 != 0,
        opcode: head[0] & 0x0f,
        payload,
    })
}

/// A reassembled data message and the sizes of its frames.
#[derive(Debug, PartialEq)]
pub(super) struct Reassembled {
    pub(super) bytes: Vec<u8>,
    frames: Vec<usize>,
}

/// Reads frames until `count` data messages finished. Control frames between fragments
/// are allowed (RFC 6455 5.4) and returned apart; a data message starting inside an
/// unfinished one, a compressed frame, or a binary frame fails the test.
pub(super) async fn messages(
    rx: &mut mpsc::UnboundedReceiver<WireFrame>,
    count: usize,
) -> (Vec<Reassembled>, Vec<WireFrame>) {
    let mut done = Vec::new();
    let mut control = Vec::new();
    let mut open: Option<Reassembled> = None;
    while done.len() < count {
        let frame = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("frame arrives")
            .expect("upstream reading");
        assert!(!frame.rsv1, "the proxy never compresses what it writes");
        match frame.opcode {
            1 => {
                assert!(open.is_none(), "a text frame inside an unfinished message");
                open = Some(Reassembled {
                    frames: vec![frame.payload.len()],
                    bytes: frame.payload,
                });
            }
            0 => {
                let message = open.as_mut().expect("a continuation without a message");
                message.frames.push(frame.payload.len());
                message.bytes.extend_from_slice(&frame.payload);
            }
            8..=10 => {
                control.push(frame);
                continue;
            }
            other => panic!("unexpected opcode {other}"),
        }
        if frame.fin {
            done.push(open.take().unwrap());
        }
    }
    (done, control)
}

fn frame_sizes(len: usize) -> Vec<usize> {
    if len == 0 {
        return vec![0];
    }
    (0..len).step_by(MIB).map(|start| (len - start).min(MIB)).collect()
}

/// Below, at and above the boundary, with a four-byte character split across two frames:
/// every message is one text message whose frames are at most 1 MiB, uncompressed whether
/// or not deflate was agreed, and reassembles to exactly the bytes given. Native raw
/// creates and formulated sends write alike, and each logical message waits for the
/// socket once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_messages_cross_the_wire_as_one_message_in_bounded_frames() {
    let mut split = "a".repeat(MIB - 1);
    split.push('🌱');
    split.push_str(&"b".repeat(MIB));
    assert!(!split.is_char_boundary(MIB), "the character spans the first boundary");
    let cases = [
        "a".repeat(MIB - 1),
        "a".repeat(MIB),
        "a".repeat(MIB + 1),
        split,
        "c".repeat(3 * MIB),
    ];
    for deflate in [false, true] {
        let (url, mut frames) = raw_upstream(Plan {
            deflate,
            ..Plan::default()
        })
        .await;
        let executor = CodexExecutor::new().unwrap();
        let conn = pooled(&executor, &url).await;
        for (i, text) in cases.iter().enumerate() {
            let waits = *conn.sink_waits.borrow();
            if i % 2 == 0 {
                conn.send_create(Bytes::from(text.clone()), None).await.unwrap();
            } else {
                conn.send(text.clone()).await.unwrap();
            }
            assert_eq!(*conn.sink_waits.borrow(), waits + 1, "one write per logical message");
            let (got, _) = messages(&mut frames, 1).await;
            assert_eq!(
                got[0].frames,
                frame_sizes(text.len()),
                "deflate={deflate} len={}",
                text.len()
            );
            assert!(
                got[0].bytes == text.as_bytes(),
                "deflate={deflate} case {i}: bytes differ"
            );
        }
        executor.close_session("conn-1");
    }
}

/// A large native turn end to end: the upstream receives the client's create unchanged,
/// in bounded frames, and the capture records it once as the logical message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_large_native_turn_is_sent_unchanged_and_captured_once() {
    use crate::codex_testkit::{Tap, Wiretap};
    let body = format!(
        r#"{{"model":"gpt-fixture","input":[{{"type":"message","role":"user","content":"{}🌱{}"}}]}}"#,
        "a".repeat(MIB - 60),
        "z".repeat(MIB)
    );
    for deflate in [false, true] {
        let (url, mut frames) = raw_upstream(Plan {
            deflate,
            answer: true,
            ..Plan::default()
        })
        .await;
        let executor = CodexExecutor::new().unwrap();
        let tap = Arc::new(Wiretap::default());
        let req = tapped(&body, &tap);
        let sent = req.body.clone();
        let response = executor
            .execute_in_session(&credential(&url), req, &Config::default(), &session(false))
            .await
            .unwrap();
        let events: Vec<String> = collect(response).await.into_iter().map(Result::unwrap).collect();
        assert_eq!(events.len(), 2, "{events:?}");
        let (got, _) = messages(&mut frames, 1).await;
        assert!(
            got[0].bytes == sent[..],
            "deflate={deflate}: the create is forwarded unchanged"
        );
        assert_eq!(got[0].frames, frame_sizes(sent.len()));
        assert!(got[0].frames.len() > 1);
        let requests: Vec<_> = tap
            .taps()
            .into_iter()
            .filter_map(|t| match t {
                Tap::WsRequest { body, .. } => Some(body),
                _ => None,
            })
            .collect();
        assert_eq!(requests.len(), 1, "captured once per logical message");
        assert!(requests[0].as_bytes() == &sent[..]);
        executor.close_session("conn-1");
    }
}

/// Two large creates written at once on one socket: each is one whole message, never
/// interleaved with the other's frames.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_large_writes_never_interleave() {
    let (url, mut frames) = raw_upstream(Plan::default()).await;
    let executor = CodexExecutor::new().unwrap();
    let conn = pooled(&executor, &url).await;
    let payloads = [
        Bytes::from("a".repeat(5 * MIB / 2)),
        Bytes::from("b".repeat(5 * MIB / 2)),
    ];
    let writes: Vec<_> = payloads
        .iter()
        .map(|payload| {
            let (conn, payload) = (conn.clone(), payload.clone());
            tokio::spawn(async move { conn.send_create(payload, None).await.map_err(|f| f.error) })
        })
        .collect();
    for write in writes {
        write.await.unwrap().unwrap();
    }
    let (got, _) = messages(&mut frames, 2).await;
    let mut got: Vec<Vec<u8>> = got.into_iter().map(|m| m.bytes).collect();
    got.sort();
    assert!(got[0] == payloads[0][..] && got[1] == payloads[1][..]);
    executor.close_session("conn-1");
}

/// The upstream pings while a fragmented message is being written: the reader answers,
/// a pong may arrive between fragments, and the message still reassembles unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ping_during_a_fragmented_write_is_answered_without_corrupting_it() {
    let (url, mut frames) = raw_upstream(Plan {
        ping_after: Some(1),
        ..Plan::default()
    })
    .await;
    let executor = CodexExecutor::new().unwrap();
    let conn = pooled(&executor, &url).await;
    let text = "p".repeat(4 * MIB + 7);
    conn.send_create(Bytes::from(text.clone()), None).await.unwrap();
    let (got, mut control) = messages(&mut frames, 1).await;
    assert!(got[0].bytes == text.as_bytes());
    assert_eq!(got[0].frames, frame_sizes(text.len()));
    while control.is_empty() {
        // The pong may follow the message.
        conn.send_create(Bytes::from_static(b"{}"), None).await.unwrap();
        control = messages(&mut frames, 1).await.1;
    }
    assert_eq!((control[0].opcode, &control[0].payload[..]), (10, &b"ping"[..]));
    executor.close_session("conn-1");
}

/// A fragmented write cancelled after its first frame left the upstream inside an
/// unfinished message: the socket records the loss before it releases the sink, so a
/// write already waiting for the sink, and any later one, writes nothing; each is
/// therefore provably unsent and may go to a fresh socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_write_cancelled_between_fragments_retires_the_socket() {
    let (url, mut frames) = raw_upstream(Plan {
        stall_after: Some(1),
        ..Plan::default()
    })
    .await;
    let executor = CodexExecutor::new().unwrap();
    let conn = pooled(&executor, &url).await;
    let write = {
        let conn = conn.clone();
        tokio::spawn(async move {
            let _ = conn.send_create(Bytes::from("x".repeat(24 * MIB)), None).await;
        })
    };
    let first = tokio::time::timeout(Duration::from_secs(30), frames.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((first.opcode, first.fin, first.payload.len()), (1, false, MIB));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !write.is_finished(),
        "the upstream stopped reading, so the write is parked"
    );
    assert!(conn.link.lock().unwrap().lost.is_none());
    let waits = *conn.sink_waits.borrow();
    let queued = {
        let conn = conn.clone();
        tokio::spawn(async move {
            conn.send_create(Bytes::from_static(br#"{"type":"response.create"}"#), None)
                .await
        })
    };
    let mut parked = conn.sink_waits.subscribe();
    tokio::time::timeout(Duration::from_secs(5), parked.wait_for(|w| *w > waits))
        .await
        .expect("the second write waits for the sink")
        .unwrap();
    write.abort();
    let _ = write.await;
    let failure = tokio::time::timeout(Duration::from_secs(5), queued)
        .await
        .expect("the waiting write finds the loss at once")
        .unwrap()
        .expect_err("a write queued behind the cancelled message writes nothing");
    assert!(failure.unsent);
    assert!(conn.link.lock().unwrap().lost.is_some(), "the socket is retired");
    let failure = conn
        .send_create(Bytes::from_static(br#"{"type":"response.create"}"#), None)
        .await
        .expect_err("a socket inside an unfinished message takes no write");
    assert!(failure.unsent);
}

/// A write that fails on a later fragment may have sent the earlier ones: it is never
/// provably unsent, even when tungstenite refuses that fragment before queueing it.
#[tokio::test]
async fn a_failure_after_the_first_fragment_is_never_unsent() {
    use tokio_tungstenite::tungstenite::Error;
    let mut sink = MockSink::default();
    let mut queued = false;
    let sent = super::write_phases(&mut sink, Bytes::from(vec![b'a'; 2 * MIB + 1]), None, &mut queued).await;
    assert_eq!(sent.unwrap(), None);
    assert_eq!(sink.started, 3, "three frames for 2 MiB + 1 byte");

    for error in [Error::AlreadyClosed, Error::ConnectionClosed] {
        let name = error.to_string();
        let mut sink = MockSink {
            start: VecDeque::from([Ok(()), Err(error)]),
            ..MockSink::default()
        };
        let mut queued = false;
        let failed = super::write_phases(&mut sink, Bytes::from(vec![b'a'; 2 * MIB]), None, &mut queued).await;
        assert!(matches!(failed, Err((_, false))), "{name}: {failed:?}");
        assert_eq!((sink.started, queued), (2, true), "{name}");
    }
}
