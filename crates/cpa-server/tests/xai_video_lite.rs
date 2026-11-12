//! Grok Imagine Video 1.5 Lite and per-model resolutions (issue #87, Go v8.0.67
//! f0ddc38f and c69cdd93, checked against the xAI video generation guide) through the
//! real router against loopback xAI mocks of the generation and poll endpoints. Fake
//! keys and a private auth-dir; nothing leaves loopback.
//!
//! The documented capabilities (xAI Video Overview capability table and the video
//! generation guide): `grok-imagine-video` up to 720p, reference images at 720p;
//! `grok-imagine-video-1.5` 1080p natively for text- and image-to-video, reference
//! images at 720p; `grok-imagine-video-1.5-lite` 1080p for text- and image-to-video by
//! upscaling 720p, reference images "Not supported". xAI's request has no upscale field.
//! Native routes forward the client's body as sent apart from the canonical model.

use std::sync::{Arc, Mutex};

use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// `install_registry` sets the process-wide registry the routes read: one test at a time.
static REGISTRY: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Requests one mock received: method, path, JSON body and the body's bytes as sent.
type Seen = Arc<Mutex<Vec<(String, String, Value, Vec<u8>)>>>;

async fn read_request(conn: &mut TcpStream) -> (String, String, Vec<u8>) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        let n = conn.read(&mut tmp).await.unwrap();
        assert!(n > 0, "upstream connection closed before the request was read");
        buf.extend_from_slice(&tmp[..n]);
        let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buf[..end]).into_owned();
        let length: usize = head
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().to_owned())
            })
            .map_or(0, |v| v.parse().unwrap());
        if buf.len() >= end + 4 + length {
            let mut first = head.lines().next().unwrap_or_default().split_whitespace();
            let method = first.next().unwrap_or_default().to_owned();
            let path = first.next().unwrap_or_default().to_owned();
            return (method, path, buf[end + 4..end + 4 + length].to_vec());
        }
    }
}

/// An xAI video upstream: generations answer `{"request_id": "<tag>-job"}`, and a poll
/// answers `poll` (the job's status payload).
async fn upstream(tag: &'static str, poll: Value) -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let seen: Seen = Arc::default();
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let (mut conn, _) = listener.accept().await.unwrap();
            let (log, poll) = (log.clone(), poll.clone());
            tokio::spawn(async move {
                let (method, path, raw) = read_request(&mut conn).await;
                let body: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
                let reply = if method == "GET" {
                    poll
                } else {
                    json!({"request_id": format!("{tag}-job")})
                };
                log.lock().unwrap().push((method, path, body, raw));
                let reply = reply.to_string();
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    reply.len()
                );
                let _ = conn.write_all(head.as_bytes()).await;
                let _ = conn.write_all(reply.as_bytes()).await;
                let _ = conn.shutdown().await;
            });
        }
    });
    (addr, seen)
}

struct Proxy {
    addr: String,
    _rt: Arc<cpa_server::Runtime>,
}

/// A proxy over xAI API-key credentials on `upstreams`, in order.
async fn proxy(upstreams: &[&str]) -> Proxy {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // A private auth-dir: without one, credential loading reads the real ~/.cli-proxy-api.
    let auth_dir =
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("xai-video-lite-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&auth_dir).unwrap();
    let mut yaml = format!(
        "auth-dir: {}\naccess:\n  api-keys: [client-key]\napi-keys:\n  xai:\n",
        auth_dir.display()
    );
    for (i, upstream) in upstreams.iter().enumerate() {
        yaml.push_str(&format!(
            "    - name: xai-{i}\n      base-url: http://{upstream}/v1\n      keys:\n        - api-key: sk-fake-xai-{i}\n"
        ));
    }
    let config = Config::parse(&yaml).unwrap();
    let credentials = cpa_core::config::credentials::load(&config);
    let executors = Executors {
        claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: Default::default(),
        meta: Default::default(),
        xai: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(config, credentials, executors));
    cpa_server::install_registry(&rt);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let app = cpa_server::router(rt.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Proxy { addr, _rt: rt }
}

async fn send(addr: &str, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
    let mut conn = TcpStream::connect(addr).await.unwrap();
    let payload = body.map(Value::to_string).unwrap_or_default();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer client-key\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{payload}",
        payload.len()
    );
    conn.write_all(head.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    conn.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text[9..12].parse().unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        dechunk(body)
    } else {
        body.to_owned()
    };
    (status, serde_json::from_str(&body).unwrap_or(Value::Null))
}

/// An HTTP/1.1 chunked body's data.
fn dechunk(mut body: &str) -> String {
    let mut out = String::new();
    while let Some((size, rest)) = body.split_once("\r\n") {
        let size = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        out.push_str(&rest[..size]);
        body = &rest[size + 2..];
    }
    out
}

fn last_sent(seen: &Seen) -> Value {
    seen.lock().unwrap().last().expect("an upstream request").2.clone()
}

const CAT: &str = "https://example.invalid/cat.png";

/// A model, the request fields beside it, and what xAI must receive (model,
/// resolution) or the refusal message.
type Case = (&'static str, Value, Result<(&'static str, &'static str), &'static str>);

/// The OpenAI-compatible route: every model and alias, absent and explicit
/// resolutions, image- and reference-to-video. `Ok(sent)` is what xAI must receive
/// (model, resolution); `Err(message)` a 400 that names the documented limit.
#[tokio::test]
async fn openai_route_resolutions_follow_each_model() {
    let _registry = REGISTRY.lock().await;
    let (up, seen) = upstream("a", json!({})).await;
    let p = proxy(&[&up]).await;
    let image = json!({"image_url": CAT});
    let references = json!([{"url": CAT}]);
    let cases: Vec<Case> = vec![
        // Absent resolution: the route's 720p default, valid for every model.
        ("grok-imagine-video", json!({}), Ok(("grok-imagine-video", "720p"))),
        (
            "grok-imagine-video-1.5",
            json!({}),
            Ok(("grok-imagine-video-1.5", "720p")),
        ),
        (
            "grok-imagine-video-1.5-lite",
            json!({}),
            Ok(("grok-imagine-video-1.5-lite", "720p")),
        ),
        (
            "xai/grok-imagine-video-1.5-lite",
            json!({}),
            Ok(("grok-imagine-video-1.5-lite", "720p")),
        ),
        (
            "GROK/Grok-Imagine-Video-1.5-Lite",
            json!({}),
            Ok(("grok-imagine-video-1.5-lite", "720p")),
        ),
        (
            "grok-imagine-video-1.5-preview",
            json!({}),
            Ok(("grok-imagine-video-1.5", "720p")),
        ),
        ("sora-2", json!({}), Ok(("grok-imagine-video", "720p"))),
        // Explicit 480p and 720p reach every model.
        (
            "grok-imagine-video",
            json!({"resolution": "480p"}),
            Ok(("grok-imagine-video", "480p")),
        ),
        (
            "grok-imagine-video-1.5-lite",
            json!({"resolution": "480p"}),
            Ok(("grok-imagine-video-1.5-lite", "480p")),
        ),
        (
            "grok-imagine-video-1.5",
            json!({"resolution": "720P"}),
            Ok(("grok-imagine-video-1.5", "720p")),
        ),
        // 1080p where xAI documents it: 1.5 natively, Lite by upscaling 720p.
        (
            "grok-imagine-video-1.5",
            json!({"resolution": "1080p"}),
            Ok(("grok-imagine-video-1.5", "1080p")),
        ),
        (
            "grok-imagine-video-1.5-preview",
            json!({"resolution": "1080p"}),
            Ok(("grok-imagine-video-1.5", "1080p")),
        ),
        (
            "grok-imagine-video-1.5-lite",
            json!({"resolution": "1080p"}),
            Ok(("grok-imagine-video-1.5-lite", "1080p")),
        ),
        // Image-to-video keeps the model's text-to-video limit.
        (
            "grok-imagine-video-1.5-lite",
            json!({"resolution": "1080p", "input_reference": image.clone()}),
            Ok(("grok-imagine-video-1.5-lite", "1080p")),
        ),
        (
            "grok-imagine-video-1.5",
            json!({"resolution": "1080p", "image": CAT}),
            Ok(("grok-imagine-video-1.5", "1080p")),
        ),
        // Reference-to-video: the classic model and 1.5 take reference images (720p).
        (
            "grok-imagine-video",
            json!({"reference_images": references.clone()}),
            Ok(("grok-imagine-video", "720p")),
        ),
        (
            "grok-imagine-video-1.5",
            json!({"resolution": "720p", "reference_images": references.clone()}),
            Ok(("grok-imagine-video-1.5", "720p")),
        ),
        // An unknown value keeps the route's default (unchanged).
        (
            "grok-imagine-video-1.5-lite",
            json!({"resolution": "4k"}),
            Ok(("grok-imagine-video-1.5-lite", "720p")),
        ),
        // 1080p where xAI documents at most 720p: refused, not silently lowered.
        (
            "grok-imagine-video",
            json!({"resolution": "1080p"}),
            Err("resolution 1080p is not supported by grok-imagine-video; xAI documents up to 720p"),
        ),
        (
            "sora-2",
            json!({"resolution": "1080p"}),
            Err("resolution 1080p is not supported by grok-imagine-video; xAI documents up to 720p"),
        ),
        (
            "grok-imagine-video",
            json!({"resolution": "1080p", "image": CAT}),
            Err("resolution 1080p is not supported by grok-imagine-video; xAI documents up to 720p"),
        ),
        (
            "grok-imagine-video-1.5",
            json!({"resolution": "1080p", "reference_images": references.clone()}),
            Err(
                "resolution 1080p is not supported by grok-imagine-video-1.5 for reference-to-video; xAI documents up to 720p",
            ),
            // xAI's capability table: Lite takes no reference images, at any resolution.
            (
                "grok-imagine-video-1.5-lite",
                json!({"resolution": "1080p", "reference_images": references.clone()}),
                Err(
                    "reference_images is not supported by grok-imagine-video-1.5-lite; xAI offers reference-to-video on grok-imagine-video and grok-imagine-video-1.5",
                ),
            ),
            // xAI's capability table: Lite takes no reference images, at any resolution.
            (
                "x-ai/grok-imagine-video-1.5-lite",
                json!({"resolution": "1080p", "reference_images": references.clone()}),
                Err(
                    "reference_images is not supported by grok-imagine-video-1.5-lite; xAI offers reference-to-video on grok-imagine-video and grok-imagine-video-1.5",
                ),
            ),
            // xAI's capability table: Lite takes no reference images, at any resolution.
            (
                "grok-imagine-video-1.5-lite",
                json!({"reference_images": references.clone()}),
                Err(
                    "reference_images is not supported by grok-imagine-video-1.5-lite; xAI offers reference-to-video on grok-imagine-video and grok-imagine-video-1.5",
                ),
            ),
            // xAI's capability table: Lite takes no reference images, at any resolution.
            (
                "grok-imagine-video-1.5-lite",
                json!({"resolution": "480p", "reference_image_urls": [CAT]}),
                Err(
                    "reference_images is not supported by grok-imagine-video-1.5-lite; xAI offers reference-to-video on grok-imagine-video and grok-imagine-video-1.5",
                ),
            ),
        ),
    ];
    for (model, extra, want) in cases {
        let mut body = json!({"model": model, "prompt": "a cat"});
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        let before = seen.lock().unwrap().len();
        let (status, out) = send(&p.addr, "POST", "/openai/v1/videos", Some(&body)).await;
        match want {
            Ok((sent_model, sent_resolution)) => {
                assert_eq!(status, 200, "{body}: {out}");
                let sent = last_sent(&seen);
                assert_eq!(
                    (sent["model"].as_str(), sent["resolution"].as_str()),
                    (Some(sent_model), Some(sent_resolution)),
                    "{body}: {sent}"
                );
                assert!(sent.get("upscale").is_none(), "no upscale field is invented: {sent}");
                assert_eq!(out["model"], sent_model, "{out}");
            }
            Err(message) => {
                assert_eq!(status, 400, "{body}: {out}");
                assert_eq!(out["error"]["message"], format!("Invalid request: {message}"), "{out}");
                assert_eq!(seen.lock().unwrap().len(), before, "{body}: nothing sent upstream");
            }
        }
    }
}

/// The native routes accept Lite and forward the client's body as sent apart from the
/// canonical model: its resolution, image, unknown fields and an `upscale` member alike.
#[tokio::test]
async fn native_route_forwards_lite_requests_unchanged() {
    let _registry = REGISTRY.lock().await;
    let (up, seen) = upstream("n", json!({})).await;
    let p = proxy(&[&up]).await;
    for model in ["grok-imagine-video-1.5-lite", "x-ai/grok-imagine-video-1.5-lite"] {
        let body = json!({"model": model, "prompt": "a cat", "resolution": "1080p", "image": {"url": CAT},
            "upscale": true, "x_vendor": {"opaque": [1, null]}});
        let (status, out) = send(&p.addr, "POST", "/v1/videos/generations", Some(&body)).await;
        assert_eq!(status, 200, "{model}: {out}");
        assert_eq!(out["request_id"], "n-job");
        let (method, path, sent, _) = seen.lock().unwrap().last().unwrap().clone();
        assert_eq!((method.as_str(), path.as_str()), ("POST", "/v1/videos/generations"));
        let mut want = body.clone();
        want["model"] = json!("grok-imagine-video-1.5-lite");
        assert_eq!(sent, want, "{model}");
    }
    // The classic model's documented limit is xAI's to enforce on the native route.
    let classic = json!({"model": "grok-imagine-video", "prompt": "a cat", "resolution": "1080p"});
    let (status, _) = send(&p.addr, "POST", "/v1/videos/generations", Some(&classic)).await;
    assert_eq!(status, 200);
    assert_eq!(last_sent(&seen)["resolution"], "1080p");
}

/// A Lite job polls on the credential that created it, and when xAI omits the model (a
/// failed job does) the answer names the model the video was created with, not the
/// classic model.
#[tokio::test]
async fn lite_jobs_poll_their_credential_and_keep_their_model() {
    let _registry = REGISTRY.lock().await;
    let failed = json!({"status": "failed", "error": {"code": "failed_precondition", "message": "unsupported"}});
    let (a, seen_a) = upstream("a", failed.clone()).await;
    let (b, seen_b) = upstream("b", failed).await;
    let p = proxy(&[&a, &b]).await;
    let mut created = Vec::new();
    for _ in 0..2 {
        let body = json!({"model": "grok-imagine-video-1.5-lite", "prompt": "a cat"});
        let (status, out) = send(&p.addr, "POST", "/openai/v1/videos", Some(&body)).await;
        assert_eq!(status, 200, "{out}");
        created.push(out["id"].as_str().unwrap().to_owned());
    }
    for id in &created {
        let (polls_a, polls_b) = (seen_a.lock().unwrap().len(), seen_b.lock().unwrap().len());
        let (status, out) = send(&p.addr, "GET", &format!("/openai/v1/videos/{id}"), None).await;
        assert_eq!(status, 200, "{out}");
        assert_eq!(out["status"], "failed", "{out}");
        assert_eq!(out["model"], "grok-imagine-video-1.5-lite", "{out}");
        // The poll went to the credential whose upstream created the job.
        let creator = if id.starts_with("a-") { &seen_a } else { &seen_b };
        let other = if id.starts_with("a-") { &seen_b } else { &seen_a };
        let creator_before = if id.starts_with("a-") { polls_a } else { polls_b };
        let other_before = if id.starts_with("a-") { polls_b } else { polls_a };
        assert_eq!(creator.lock().unwrap().len(), creator_before + 1, "{id}");
        assert_eq!(other.lock().unwrap().len(), other_before, "{id}");
        assert_eq!(creator.lock().unwrap().last().unwrap().1, format!("/v1/videos/{id}"));
    }
}

/// Lite is listed for xAI credentials as a built-in (it stays when a refreshed catalog
/// omits it) and, like the other video models, hidden from Codex clients.
#[tokio::test]
async fn lite_is_listed_and_hidden_from_codex_clients() {
    let _registry = REGISTRY.lock().await;
    let (up, _) = upstream("m", json!({})).await;
    let p = proxy(&[&up]).await;
    let (status, out) = send(&p.addr, "GET", "/v1/models", None).await;
    assert_eq!(status, 200, "{out}");
    let ids: Vec<&str> = out["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    for id in [
        "grok-imagine-video",
        "grok-imagine-video-1.5",
        "grok-imagine-video-1.5-lite",
    ] {
        assert!(ids.contains(&id), "{id} missing from {ids:?}");
    }
    let (status, out) = send(&p.addr, "GET", "/v1/models?client_version=0.150.0", None).await;
    assert_eq!(status, 200, "{out}");
    let models = out["models"].as_array().expect("Codex catalog shape");
    for slug in ["grok-imagine-video-1.5", "grok-imagine-video-1.5-lite"] {
        let entry = models
            .iter()
            .find(|m| m["slug"] == slug)
            .unwrap_or_else(|| panic!("no {slug}"));
        assert_eq!(entry["visibility"], "hide", "{slug}: {entry}");
    }
}

/// Controls: video models of removed providers and unknown models stay refused on both
/// routes, and nothing reaches the xAI upstream.
#[tokio::test]
async fn other_video_models_stay_refused() {
    let _registry = REGISTRY.lock().await;
    let (up, seen) = upstream("r", json!({})).await;
    let p = proxy(&[&up]).await;
    for model in [
        "veo-3.0-generate-preview",
        "kling-v2",
        "grok-imagine-video-2-lite",
        "team/grok-imagine-video-1.5-lite",
    ] {
        let body = json!({"model": model, "prompt": "a cat"});
        let (status, out) = send(&p.addr, "POST", "/openai/v1/videos", Some(&body)).await;
        assert_eq!(status, 400, "{model}: {out}");
        let (status, out) = send(&p.addr, "POST", "/v1/videos/generations", Some(&body)).await;
        assert_eq!(status, 400, "{model}: {out}");
    }
    assert!(seen.lock().unwrap().is_empty());
}

/// One request with `payload` sent as these exact bytes; the status.
async fn send_raw(addr: &str, path: &str, payload: &str) -> u16 {
    let mut conn = TcpStream::connect(addr).await.unwrap();
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer client-key\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{payload}",
        payload.len()
    );
    conn.write_all(head.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    conn.read_to_end(&mut out).await.unwrap();
    String::from_utf8_lossy(&out)[9..12].parse().unwrap()
}

/// A native body reaches xAI as the client sent it, apart from the canonical model:
/// spacing, member order, duplicate members, number spellings, escapes, opaque nested
/// `image_url` members and the `image_url` alias xAI documents for `image` and
/// `reference_images` all pass unchanged, on every native path and vendor prefix.
#[tokio::test]
async fn native_routes_forward_bodies_byte_for_byte() {
    let _registry = REGISTRY.lock().await;
    let (up, seen) = upstream("b", json!({})).await;
    let p = proxy(&[&up]).await;
    let models = [
        ("grok-imagine-video-1.5-lite", "grok-imagine-video-1.5-lite"),
        ("grok-imagine-video", "grok-imagine-video"),
        ("grok-imagine-video-1.5-preview", "grok-imagine-video-1.5"),
    ];
    for (model, canonical) in models {
        for prefix in ["", "xai/", "x-ai/", "GROK/"] {
            for path in [
                "/v1/videos",
                "/v1/videos/generations",
                "/v1/videos/edits",
                "/v1/videos/extensions",
            ] {
                let raw = format!(
                    r#" {{ "model" : "{prefix}{model}", "prompt":"cat",  "resolution":"1080p", "image" : {{ "image_url" : "https://example.invalid/cat.png" }}, "reference_images":[{{"image_url":{{"url":"https://example.invalid/ref.png"}}}}], "vendor_extension":{{"image":{{"image_url":"opaque-value"}},"images":[{{"image_url":"opaque"}}]}}, "upscale":true, "big":9007199254740993, "exp":1e+07, "esc":"\u0061\/", "same":1, "same":2 }} "#
                );
                let status = send_raw(&p.addr, path, &raw).await;
                assert_eq!(status, 200, "{prefix}{model} {path}");
                let (method, sent_path, _, bytes) = seen.lock().unwrap().last().unwrap().clone();
                assert_eq!(method, "POST");
                let route = if path == "/v1/videos" {
                    "/v1/videos/generations"
                } else {
                    path
                };
                assert_eq!(sent_path, route, "{prefix}{model} {path}");
                let want = raw.replace(&format!("\"{prefix}{model}\""), &format!("\"{canonical}\""));
                assert_eq!(String::from_utf8(bytes).unwrap(), want, "{prefix}{model} {path}");
            }
        }
    }
}

/// Control: the compatibility route still builds xAI's own shape (`image.url`,
/// `reference_images[].url`) from its OpenAI-style inputs.
#[tokio::test]
async fn compatibility_route_still_builds_url_references() {
    let _registry = REGISTRY.lock().await;
    let (up, seen) = upstream("c", json!({})).await;
    let p = proxy(&[&up]).await;
    let image =
        json!({"model": "grok-imagine-video-1.5-lite", "prompt": "a cat", "input_reference": {"image_url": CAT}});
    let (status, out) = send(&p.addr, "POST", "/openai/v1/videos", Some(&image)).await;
    assert_eq!(status, 200, "{out}");
    assert_eq!(last_sent(&seen)["image"], json!({"url": CAT}));
    let references = json!({"model": "grok-imagine-video-1.5", "prompt": "a cat", "reference_images": [{"image_url": {"url": CAT}}]});
    let (status, out) = send(&p.addr, "POST", "/openai/v1/videos", Some(&references)).await;
    assert_eq!(status, 200, "{out}");
    assert_eq!(last_sent(&seen)["reference_images"], json!([{"url": CAT}]));
}
