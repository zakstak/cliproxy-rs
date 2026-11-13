//! Images handler tests. The routes are checked against the Go server in
//! tests/media_go.rs; these cover the content sniffer the goldens touch only twice.

use super::*;

/// Expectations printed by Go 1.x `http.DetectContentType` for the same bytes.
#[test]
fn sniffs_like_go() {
    let cases: [(&[u8], &str); 28] = [
        (b"", "text/plain; charset=utf-8"),
        (b"   ", "text/plain; charset=utf-8"),
        (b"\x89PNG\r\n\x1a\nxx", "image/png"),
        (b"GIF87a..", "image/gif"),
        (b"GIF89a..", "image/gif"),
        (b"\xff\xd8\xff\xe0", "image/jpeg"),
        (b"BMxx", "image/bmp"),
        (b"RIFF\x00\x00\x00\x00WEBPVP8 ", "image/webp"),
        (b"RIFF\x00\x00\x00\x00WAVEfmt ", "audio/wave"),
        (b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom", "video/mp4"),
        (b"\x1a\x45\xdf\xa3", "video/webm"),
        (b" \n<html>x", "text/html; charset=utf-8"),
        (b"<!DOCTYPE HTML>", "text/html; charset=utf-8"),
        (b"<p", "text/plain; charset=utf-8"),
        (b"<?xml version", "text/xml; charset=utf-8"),
        (b"%PDF-1.4", "application/pdf"),
        (b"\xef\xbb\xbfhi", "text/plain; charset=utf-8"),
        (b"\xfe\xffhi", "text/plain; charset=utf-16be"),
        (b"PK\x03\x04", "application/zip"),
        (b"\x1f\x8b\x08", "application/x-gzip"),
        (b"hello world", "text/plain; charset=utf-8"),
        (b"hi\x00there", "application/octet-stream"),
        (b"OggS\x00", "application/ogg"),
        (b"ID3\x03", "audio/mpeg"),
        (b"wOFF", "font/woff"),
        (b"\x00asm", "application/wasm"),
        (b"<b>bold</b>", "text/html; charset=utf-8"),
        (b"<br/>", "text/plain; charset=utf-8"),
    ];
    for (input, want) in cases {
        assert_eq!(detect_content_type(input), want, "{input:?}");
    }
}

/// #105: an image stream forwards upstream reads as they arrive. Whether the upstream
/// fails inside a line, after a line of an unfinished event (LF, CRLF or lone CR) or at
/// an event boundary, the client parses every event the upstream finished, then one
/// complete `error` event: the error is never merged into the cut-off event, and the
/// body ends with it, heartbeats included, although the upstream stream never ends. The
/// reads arrive without a pause, so no heartbeat falls due inside an event and this test
/// does not depend on the heartbeat gate.
#[tokio::test]
async fn raw_image_stream_errors_start_on_an_event_boundary_and_end_the_body() {
    use futures_util::StreamExt;
    tokio::time::pause();
    let error = ExecError::local(502, cpa_core::exec::FailureScope::Transport, "upstream reset");
    let failure = Failure::Exec(error.clone());
    let error_event = stream_error_event(failure.status(), &failure.text());
    let (_, error_payload) = respond::parse_sse(&error_event).0.remove(0);
    let done = r#"data: {"type":"image_generation.partial_image","partial_image_index":0}"#;
    for (sent, close) in [
        (format!("{done}\n\ndata: {{\"b64_json\":\"AA"), "\n\n"),
        (format!("{done}\n\ndata: {{\"b64_json\":\"AA\"}}\n"), "\n"),
        (format!("{done}\r\n\r\ndata: x\r\n"), "\n"),
        (format!("{done}\r\rdata: x\r"), "\n\n"),
        (format!("{done}\n\n"), ""),
        (format!("{done}\r\n\r\n"), ""),
        (format!("{done}\r\r"), ""),
    ] {
        for routed in [true, false] {
            let case = format!("routed={routed} sent={sent:?}");
            let half = sent.len() / 2;
            let reads = [&sent[..half], &sent[half..]].map(|read| Ok(Bytes::copy_from_slice(read.as_bytes())));
            let failed = error.clone();
            let rest = futures_util::stream::iter(reads)
                .chain(futures_util::stream::once(async move { Err(failed) }))
                .chain(futures_util::stream::pending())
                .boxed();
            let keepalive = Some(std::time::Duration::from_millis(100));
            let body = respond::stream(None, rest, Raw::new(routed), keepalive);
            let out = tokio::time::timeout(
                std::time::Duration::from_secs(60),
                axum::body::to_bytes(body, usize::MAX),
            )
            .await
            .unwrap_or_else(|_| panic!("{case}: the body did not end after the error event"))
            .unwrap();
            let want = [sent.as_bytes(), close.as_bytes(), &error_event[..]].concat();
            assert_eq!(
                String::from_utf8_lossy(&out),
                String::from_utf8_lossy(&want),
                "{case}: error event merged into the cut-off event, or bytes after it"
            );
            let (events, _) = respond::parse_sse(&out);
            assert_eq!(
                events.first(),
                Some(&(String::new(), done["data: ".len()..].to_owned())),
                "{case}: the finished event"
            );
            assert_eq!(
                events.last(),
                Some(&("error".to_owned(), error_payload.clone())),
                "{case}: the error event"
            );
        }
    }
}
