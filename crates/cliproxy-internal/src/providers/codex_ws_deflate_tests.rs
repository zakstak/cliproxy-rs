//! RFC 7692 compression examples and locally constructed WebSocket inputs.
//! Expected messages are authored here; no recorder output defines the contract.

use super::*;
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::{Message, protocol::Role};

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

async fn read(bytes: &[u8], limit: usize) -> Vec<String> {
    let (mut server, client) = tokio::io::duplex(1 << 20);
    server.write_all(bytes).await.unwrap();
    server.shutdown().await.unwrap();
    let mut socket = WebSocketStream::from_raw_socket(Inflate::new(client, true, limit), Role::Client, None).await;
    let mut messages = Vec::new();
    while let Some(Ok(message)) = socket.next().await {
        match message {
            Message::Text(text) => messages.push(text.to_string()),
            Message::Binary(bytes) => messages.push(String::from_utf8_lossy(&bytes).into_owned()),
            _ => {}
        }
    }
    drop(server);
    messages
}

/// The local client supports no-context-takeover in both directions. Incomplete
/// agreements are refused; unrelated extensions do not enable compression.
#[test]
fn compression_requires_both_no_context_takeover_parameters() {
    assert_eq!(
        OFFER,
        "permessage-deflate; server_no_context_takeover; client_no_context_takeover"
    );
    for (values, expected) in [
        (vec![], Ok(false)),
        (vec!["unrelated"], Ok(false)),
        (vec!["permessage-deflate"], Err(())),
        (vec!["permessage-deflate; server_no_context_takeover"], Err(())),
        (vec!["permessage-deflate; client_no_context_takeover"], Err(())),
        (vec![OFFER], Ok(true)),
        (
            vec!["permessage-deflate;client_no_context_takeover;server_no_context_takeover"],
            Ok(true),
        ),
        (vec!["unrelated", OFFER], Ok(true)),
        (
            vec!["unrelated, permessage-deflate; server_no_context_takeover; client_no_context_takeover"],
            Ok(true),
        ),
        (
            vec![
                "permessage-deflate; server_no_context_takeover; client_no_context_takeover; server_max_window_bits=10",
            ],
            Ok(true),
        ),
        (vec!["permessage-deflate; client_no_context_takeover", OFFER], Err(())),
        (
            vec!["permessage-deflate; server_no_context_takeover; client_no_context_takeover; bad@"],
            Ok(false),
        ),
    ] {
        let mut headers = HeaderMap::new();
        for value in &values {
            headers.append(http::header::SEC_WEBSOCKET_EXTENSIONS, value.parse().unwrap());
        }
        assert_eq!(negotiated(&headers), expected, "{values:?}");
    }
}

/// RFC 7692 section 7.2.3.1 encodes "Hello" as f2 48 cd c9 c9 07 00.
/// Section 7.2.3.2 permits a ping between its fragments.
#[tokio::test]
async fn compressed_uncompressed_fragmented_and_repeated_messages_keep_their_payloads() {
    for (frame, messages) in [
        ("c107f248cdc9c90700", vec!["Hello"]),
        ("4103f248cd89008004c9c90700", vec!["Hello"]),
        ("c10b000500faff48656c6c6f00", vec!["Hello"]),
        ("810548656c6c6f", vec!["Hello"]),
        ("0102486580036c6c6f", vec!["Hello"]),
        ("c107f248cdc9c90700c107f248cdc9c90700", vec!["Hello", "Hello"]),
    ] {
        assert_eq!(read(&hex(frame), 1024).await, messages, "{frame}");
    }
}

#[tokio::test]
async fn corrupt_reserved_masked_and_interleaved_data_frames_do_not_deliver_payloads() {
    for frame in [
        "a10548656c6c6f",           // unnegotiated RSV2
        "81850000000048656c6c6f",   // servers must not mask
        "4103f248cd810548656c6c6f", // data before continuation completes
        "c103ffffff",               // corrupt DEFLATE
        "e107f248cdc9c90700",       // reserved bit on compressed data
        "4103f248cd9004c9c90700",   // reserved bit on continuation
    ] {
        assert!(read(&hex(frame), 1024).await.is_empty(), "{frame}");
    }
    assert_eq!(read(&hex("c107f248cdc9c90700c103ffffff"), 1024).await, ["Hello"]);
}

#[tokio::test]
async fn inflated_messages_are_bounded() {
    use std::io::Write as _;
    let message = format!("{{\"response\":\"{}\"}}", "a".repeat(4096));
    let mut compressor = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    compressor.write_all(message.as_bytes()).unwrap();
    let compressed = compressor.finish().unwrap();
    let mut frame = vec![0xc1, compressed.len() as u8];
    assert!(compressed.len() < 126);
    frame.extend(compressed);
    assert!(frame.len() < message.len() - 1);
    assert_eq!(read(&frame, message.len()).await, [message.as_str()]);
    assert!(read(&frame, message.len() - 1).await.is_empty());
}

/// Without an agreement the layer is transparent and tungstenite rejects RSV1.
#[tokio::test]
async fn without_agreement_frames_pass_through() {
    let (mut server, client) = tokio::io::duplex(1024);
    server
        .write_all(&hex("810548656c6c6fc107f248cdc9c90700"))
        .await
        .unwrap();
    server.shutdown().await.unwrap();
    let mut socket = WebSocketStream::from_raw_socket(Inflate::new(client, false, 1024), Role::Client, None).await;
    assert_eq!(socket.next().await.unwrap().unwrap(), Message::text("Hello"));
    assert!(socket.next().await.unwrap().is_err());
}
