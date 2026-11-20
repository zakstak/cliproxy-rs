//! Supported xAI media routes exercised with local scripted peers. Archived client
//! requests and peer replies are inputs only. Expectations derive from route validation,
//! upstream passthrough and the public image/video/audio response contracts.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use base64::Engine;
use cliproxy_internal::config::Config;
use cliproxy_internal::gateway::router;
use cliproxy_internal::providers::Executors;
use cliproxy_internal::providers::claude::ClaudeExecutor;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

#[derive(Deserialize)]
struct Fixture {
    config: String,
    scenarios: Vec<Scenario>,
}

#[derive(Deserialize, Clone)]
struct Upstream {
    #[serde(default)]
    delay_ms: u64,
    status: u16,
    #[serde(default)]
    headers: Vec<(String, String)>,
    body: String,
}

#[derive(Deserialize)]
struct Scenario {
    name: String,
    method: String,
    path: String,
    #[serde(default)]
    content_type: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    body_b64: String,
    /// A zstd skippable frame of this many zero bytes goes before the body.
    #[serde(default)]
    skip_prefix: usize,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    upstreams: Vec<Upstream>,
}

/// Answers each connection with the next scripted reply and records the raw request,
/// for protocol assertions.
#[derive(Default)]
struct Capture {
    replies: Mutex<VecDeque<Upstream>>,
    requests: Mutex<Vec<String>>,
}

async fn capture(listener: tokio::net::TcpListener, state: Arc<Capture>, addr: String) {
    loop {
        let Ok((conn, _)) = listener.accept().await else { return };
        let (state, addr) = (state.clone(), addr.clone());
        tokio::spawn(async move {
            let mut reader = BufReader::new(conn);
            let mut raw = Vec::new();
            let mut length = 0;
            loop {
                let mut line = Vec::new();
                if reader.read_until(b'\n', &mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                raw.extend_from_slice(&line);
                let text = String::from_utf8_lossy(&line).to_ascii_lowercase();
                if let Some(value) = text.strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap_or(0);
                }
                if line == b"\r\n" {
                    break;
                }
            }
            let mut body = vec![0; length];
            let _ = reader.read_exact(&mut body).await;
            raw.extend_from_slice(&body);
            state
                .requests
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(&raw).into_owned());
            let reply = state.replies.lock().unwrap().pop_front().unwrap_or(Upstream {
                delay_ms: 0,
                status: 599,
                headers: vec![],
                body: "no scripted reply".into(),
            });
            tokio::time::sleep(std::time::Duration::from_millis(reply.delay_ms)).await;
            let payload = reply.body.replace("UPSTREAM", &addr);
            let reason = axum::http::StatusCode::from_u16(reply.status)
                .ok()
                .and_then(|s| s.canonical_reason())
                .unwrap_or("");
            let mut out = format!("HTTP/1.1 {} {reason}\r\n", reply.status);
            for (name, value) in &reply.headers {
                out.push_str(&format!("{name}: {value}\r\n"));
            }
            out.push_str(&format!(
                "Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            ));
            let mut conn = reader.into_inner();
            let _ = conn.write_all(out.as_bytes()).await;
            let _ = conn.shutdown().await;
        });
    }
}

const DRIVER_USER_AGENT: &str = "media-contract/1";

fn expected_status(s: &Scenario) -> u16 {
    match s.name.as_str() {
        "gen_xai_no_output"
        | "gen_xai_invalid_upstream_json"
        | "video_content_connect_refused"
        | "video_content_no_url"
        | "video_content_invalid_url"
        | "video_create_missing_id" => 502,
        "gen_xai_after_429" => 429,
        // Streaming has already committed its status before the terminal error event.
        name if name.starts_with("slow_") => 200,
        _ if !s.upstreams.is_empty() => s.upstreams.last().unwrap().status,
        name if name.starts_with("models_") => 200,
        _ => 400,
    }
}

fn json_payloads(body: &[u8]) -> Vec<serde_json::Value> {
    if String::from_utf8_lossy(body)
        .lines()
        .any(|line| line.starts_with("data:"))
    {
        String::from_utf8_lossy(body)
            .lines()
            .filter_map(|line| {
                line.strip_prefix("data: ")
                    .and_then(|data| serde_json::from_str(data).ok())
            })
            .collect()
    } else {
        serde_json::from_slice(body).into_iter().collect()
    }
}

fn assert_error(body: &[u8], s: &Scenario) {
    let values = json_payloads(body);
    assert!(!values.is_empty(), "{}: machine-readable error", s.name);
    assert!(
        values.iter().any(|value| value.get("error").is_some()),
        "{}: {values:?}",
        s.name
    );
}

fn decoded_input(s: &Scenario) -> serde_json::Value {
    let mut bytes = if s.body_b64.is_empty() {
        s.body.as_bytes().to_vec()
    } else {
        base64::engine::general_purpose::STANDARD.decode(&s.body_b64).unwrap()
    };
    if let Some(encoding) = s
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-encoding"))
        .map(|(_, value)| value)
    {
        for codec in encoding.split(',').rev() {
            assert_eq!(codec.trim(), "zstd");
            bytes = zstd::stream::decode_all(bytes.as_slice()).unwrap();
        }
    }
    let text = String::from_utf8_lossy(&bytes);
    if s.content_type.starts_with("application/json") {
        return serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    }
    let mut fields = serde_json::Map::new();
    if s.content_type.starts_with("application/x-www-form-urlencoded") {
        for pair in text.split('&') {
            let (key, value) = pair.split_once('=').unwrap();
            let value = url::form_urlencoded::parse(format!("v={value}").as_bytes())
                .next()
                .unwrap()
                .1
                .into_owned();
            fields.insert(key.into(), value.into());
        }
    } else if let Some(boundary) = s.content_type.split("boundary=").nth(1) {
        let text: String = bytes.iter().map(|byte| char::from(*byte)).collect();
        let text = text.replace("\\r\\n", "\r\n");
        let mut uploads = Vec::new();
        for part in text.split(&format!("--{boundary}")) {
            let Some((head, value)) = part.trim_start_matches("\r\n").split_once("\r\n\r\n") else {
                continue;
            };
            let Some(name) = head.split("name=\"").nth(1).and_then(|rest| rest.split('"').next()) else {
                continue;
            };
            let value = value.trim_end_matches("\r\n");
            if head.contains("filename=") {
                let mime = head
                    .lines()
                    .find_map(|line| line.strip_prefix("Content-Type: "))
                    .unwrap_or(if value.starts_with("GIF8") {
                        "image/gif"
                    } else if value.starts_with("\u{89}PNG") {
                        "image/png"
                    } else {
                        "application/octet-stream"
                    });
                uploads.push((name.to_owned(), serde_json::json!({"type":"image_url","url":format!("data:{mime};base64,{}",base64::engine::general_purpose::STANDARD.encode(value.chars().map(|ch| ch as u8).collect::<Vec<_>>()))})));
            } else {
                fields.insert(name.into(), value.into());
            }
        }
        let repeated = uploads.iter().any(|(name, _)| name == "image[]");
        fields.insert(
            "uploads".into(),
            uploads
                .into_iter()
                .filter(|(name, _)| if repeated { name == "image[]" } else { name == "image" })
                .map(|(_, value)| value)
                .collect::<Vec<_>>()
                .into(),
        );
    }
    fields.into()
}

fn assert_payload(s: &Scenario, body: &str) {
    let input = decoded_input(s);
    let actual: serde_json::Value = serde_json::from_str(body).unwrap();
    let text = |field: &str| input[field].as_str().unwrap_or("").trim().to_owned();
    let canonical = |model: &str| {
        model
            .rsplit('/')
            .next()
            .unwrap_or(model)
            .split('(')
            .next()
            .unwrap()
            .to_owned()
    };
    if s.path.contains("/images/") {
        let editing = s.path.ends_with("/edits");
        let mut expected = serde_json::json!({"model":canonical(&text("model")),"prompt":text("prompt")});
        let size = text("size");
        let ratio = match size.as_str() {
            "1024x1024" | "2048x2048" => "1:1",
            "1792x1024" => "16:9",
            "1024x1792" => "9:16",
            "1024x1536" => "2:3",
            "1536x1024" => "3:2",
            _ => match text("aspect_ratio").as_str() {
                "portrait" => "9:16",
                "landscape" => "16:9",
                "20:9" => "20:9",
                _ => {
                    if editing {
                        ""
                    } else {
                        "1:1"
                    }
                }
            },
        };
        if !ratio.is_empty() {
            expected["aspect_ratio"] = ratio.into();
        }
        let resolution = if text("resolution").eq_ignore_ascii_case("2k") || size.contains("2048") {
            "2k"
        } else if editing {
            ""
        } else {
            "1k"
        };
        if !resolution.is_empty() {
            expected["resolution"] = resolution.into();
        }
        expected["response_format"] = if text("response_format").is_empty() {
            "b64_json".into()
        } else {
            text("response_format").to_ascii_lowercase().into()
        };
        if !text("quality").is_empty() {
            expected["quality"] = text("quality").into();
        }
        let n = input["n"].as_i64().or_else(|| {
            if s.content_type.starts_with("multipart/") {
                text("n").parse().ok()
            } else {
                None
            }
        });
        if let Some(n) = n.filter(|n| *n > 0) {
            expected["n"] = n.into();
        }
        if editing {
            fn image(value: &serde_json::Value) -> Option<String> {
                value
                    .as_str()
                    .or(value["url"].as_str())
                    .or(value["image_url"].as_str())
                    .or(value["image_url"]["url"].as_str())
                    .map(|value| value.trim().to_owned())
            }
            let references = if let Some(uploads) = input["uploads"].as_array() {
                uploads.clone()
            } else {
                let mut images = Vec::new();
                if let Some(url) = image(&input["image"]) {
                    images.push(serde_json::json!({"type":"image_url","url":url}));
                }
                for value in input["images"].as_array().into_iter().flatten() {
                    if let Some(url) = image(value) {
                        images.push(serde_json::json!({"type":"image_url","url":url}));
                    }
                }
                images
            };
            if references.len() == 1 {
                expected["image"] = references[0].clone();
            } else {
                expected["images"] = references.into();
            }
        }
        assert_eq!(actual, expected, "{}: exact translated image fields", s.name);
    } else if s.path.contains("/audio/speech") || s.path.ends_with("/tts") {
        let spoken = if text("input").is_empty() {
            text("text")
        } else {
            text("input")
        };
        let voice = text("voice");
        let voice = if voice.is_empty() { text("voice_id") } else { voice };
        let voice = match voice.to_ascii_lowercase().as_str() {
            "nova" => "eve".to_owned(),
            "alloy" => "ara".to_owned(),
            "ash" => "orion".to_owned(),
            "ballad" => "luna".to_owned(),
            "coral" => "celeste".to_owned(),
            "echo" => "rex".to_owned(),
            "fable" => "sal".to_owned(),
            "onyx" => "leo".to_owned(),
            "sage" => "iris".to_owned(),
            "shimmer" => "aurora".to_owned(),
            "verse" => "lumen".to_owned(),
            "" => "eve".to_owned(),
            other => other.to_owned(),
        };
        let mut expected = serde_json::json!({"text":spoken,"voice_id":voice,"language":if text("language").is_empty(){"auto".into()}else{text("language")}});
        // This input config explicitly overrides language on the voice-model route.
        if canonical(&text("model")) == "grok-voice-tts-1.0" {
            expected["language"] = "de".into();
        }
        let codec = if !text("response_format").is_empty() {
            text("response_format").to_ascii_lowercase()
        } else {
            input["output_format"]["codec"]
                .as_str()
                .unwrap_or("mp3")
                .to_ascii_lowercase()
        };
        if codec != "mp3" {
            let rate = if text("response_format").is_empty() {
                input["output_format"]["sample_rate"]
                    .as_f64()
                    .filter(|rate| *rate > 0.0)
                    .map(|rate| rate as i64)
                    .unwrap_or(24000)
            } else {
                24000
            };
            expected["output_format"] = serde_json::json!({"codec":codec,"sample_rate":rate});
        }
        if input["speed"].as_f64().is_some_and(|speed| speed > 0.0) {
            expected["speed"] = input["speed"].clone();
        }
        assert_eq!(
            actual, expected,
            "{}: exact audio text, voice, language and encoding",
            s.name
        );
    } else if s.path.starts_with("/v1/videos") {
        let mut expected = input;
        let model = expected["model"].as_str().unwrap_or("grok-imagine-video");
        expected["model"] = canonical(model).into();
        assert_eq!(actual, expected, "{}: native fields preserved", s.name);
        if s.body.contains("\"model\":") {
            let source_model = decoded_input(s)["model"].as_str().unwrap().to_owned();
            let expected = s.body.replacen(
                &serde_json::to_string(&source_model).unwrap(),
                &serde_json::to_string(&canonical(&source_model)).unwrap(),
                1,
            );
            assert_eq!(body, expected, "{}: native bytes except routed model", s.name);
        }
    } else {
        let expected = match s.name.as_str() {
            "video_create_json" => {
                serde_json::json!({"model":"grok-imagine-video","prompt":"waves","duration":15,"aspect_ratio":"16:9","resolution":"720p","image":{"url":"https://example.com/i.png"}})
            }
            "video_create_form_urlencoded" => {
                serde_json::json!({"model":"grok-imagine-video-1.5","prompt":"hi there","duration":4,"aspect_ratio":"16:9","resolution":"480p","reference_images":[{"url":"https://a/1.png"},{"url":"https://a/2.png"}]})
            }
            "video_create_multipart" => {
                serde_json::json!({"model":"grok-imagine-video","prompt":"rain","duration":1,"aspect_ratio":"9:16","resolution":"720p","image":{"url":"https://example.com/r.png"}})
            }
            "video_create_missing_id" => {
                serde_json::json!({"model":"grok-imagine-video","prompt":"x","duration":4,"aspect_ratio":"9:16","resolution":"720p"})
            }
            "slow_video_create" => {
                serde_json::json!({"model":"grok-imagine-video","prompt":"slow","duration":4,"aspect_ratio":"9:16","resolution":"720p"})
            }
            other => panic!("missing independent video request specification: {other}"),
        };
        assert_eq!(actual, expected, "{}: exact translated video fields", s.name);
    }
}

fn assert_response(s: &Scenario, status: u16, headers: &wreq::header::HeaderMap, body: &[u8], addr: &str) {
    assert_eq!(
        status,
        expected_status(s),
        "{}: {}",
        s.name,
        String::from_utf8_lossy(body)
    );
    let streaming = s.path.contains("/images/") && decoded_input(s)["stream"] == true && status == 200;
    if streaming {
        assert!(
            headers
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("text/event-stream"),
            "{}: stream content type",
            s.name
        );
        let content = String::from_utf8_lossy(body);
        let event = if s.path.ends_with("/edits") {
            "image_edit.completed"
        } else {
            "image_generation.completed"
        };
        let values = json_payloads(body);
        let errors = s.name.contains("stream_error") || s.name.contains("stream_no_output");
        if errors {
            assert!(content.contains("event: error\n"));
            assert!(
                values
                    .iter()
                    .any(|value| value["error"]["type"].is_string() && value["error"]["message"].is_string()),
                "{}: {values:?}",
                s.name
            );
        } else {
            assert!(content.contains(&format!("event: {event}\n")), "{}: {content}", s.name);
            assert!(values.iter().all(|value| value["type"] == event));
        }
    }
    if status >= 400 {
        if let Some(reply) = s.upstreams.last().filter(|reply| {
            reply.status == status && s.name != "gen_xai_after_429" && !s.name.starts_with("video_content_")
        }) {
            let peer = reply.body.replace("UPSTREAM", addr);
            if serde_json::from_str::<serde_json::Value>(&peer).is_ok() {
                assert_eq!(body, peer.as_bytes(), "{}: structured provider error", s.name);
            } else {
                assert_error(body, s);
                let value: serde_json::Value = serde_json::from_slice(body).unwrap();
                assert!(
                    value["error"]["message"].as_str().unwrap().contains(peer.trim()),
                    "{}: plain peer diagnostic",
                    s.name
                );
            }
        } else {
            assert_error(body, s);
        }
        return;
    }
    if s.name.starts_with("models_") {
        let value: serde_json::Value = serde_json::from_slice(body).unwrap();
        let text = value.to_string();
        for model in ["grok-imagine-image", "grok-imagine-video", "grok-tts"] {
            assert!(text.contains(model));
        }
        assert!(!text.contains("sk-fake"));
        return;
    }
    let reply = s.upstreams.last().unwrap();
    let peer = reply.body.replace("UPSTREAM", addr);
    if s.path.contains("/audio/speech") || s.path.ends_with("/tts") || s.name.contains("video_content") {
        assert_eq!(body, peer.as_bytes(), "{}: media bytes", s.name);
        if let Some((_, expected)) = reply
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        {
            let speech = s.path.contains("/audio/speech") || s.path.ends_with("/tts");
            let input = decoded_input(s);
            let requested = input["response_format"]
                .as_str()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            let codec = if requested.is_empty() {
                input["output_format"]["codec"]
                    .as_str()
                    .unwrap_or("mp3")
                    .to_ascii_lowercase()
            } else {
                requested
            };
            let fallback = match codec.as_str() {
                "wav" => "audio/wav",
                "pcm" => "audio/pcm",
                _ => "audio/mpeg",
            };
            let expected = if speech && !expected.trim().to_ascii_lowercase().starts_with("audio/") {
                fallback
            } else {
                expected
            };
            assert_eq!(
                headers.get("content-type").unwrap().to_str().unwrap(),
                expected,
                "{}: media content type",
                s.name
            );
        }
        return;
    }
    if s.name.starts_with("native_") || s.name == "slow_native_retrieve" {
        let data = if s.name.starts_with("slow_") {
            body.trim_ascii_start()
        } else {
            body
        };
        assert_eq!(
            data,
            peer.as_bytes(),
            "{}: native response bytes after configured keepalive",
            s.name
        );
        return;
    }
    let values = json_payloads(body);
    assert!(!values.is_empty(), "{}: response objects", s.name);
    if s.name.contains("stream_error") || s.name.contains("stream_no_output") {
        assert!(
            values.iter().any(|value| value.get("error").is_some()),
            "{}: {values:?}",
            s.name
        );
        return;
    }
    let source: serde_json::Value = serde_json::from_str(&peer).unwrap();
    if let Some(images) = source.get("data").and_then(serde_json::Value::as_array) {
        let output: Vec<&serde_json::Value> = if values[0].get("data").is_some() {
            values[0]["data"].as_array().unwrap().iter().collect()
        } else {
            values.iter().collect()
        };
        let images: Vec<_> = images
            .iter()
            .filter(|image| {
                ["b64_json", "url"]
                    .iter()
                    .any(|field| image[*field].as_str().is_some_and(|v| !v.is_empty()))
            })
            .collect();
        assert_eq!(output.len(), images.len(), "{}", s.name);
        for (image, result) in images.into_iter().zip(output) {
            for field in ["b64_json", "url"] {
                if image[field].as_str().is_some_and(|v| !v.is_empty()) {
                    if field == "b64_json" && result.get(field).is_none() {
                        assert!(result["url"].as_str().is_some_and(
                            |url| url.ends_with(image[field].as_str().unwrap()) && url.starts_with("data:image/")
                        ));
                    } else {
                        assert_eq!(result[field], image[field], "{}: {field}", s.name);
                    }
                }
            }
            if let Some(prompt) = image["revised_prompt"]
                .as_str()
                .filter(|_| result.get("revised_prompt").is_some())
            {
                assert_eq!(result["revised_prompt"], prompt.trim());
            }
        }
        if values[0].get("data").is_some() {
            assert_eq!(values[0]["usage"], source["usage"]);
        }
    } else {
        let video = &values[0];
        assert_eq!(video["object"], "video", "{}", s.name);
        assert!(video["id"].as_str().is_some_and(|id| !id.is_empty()));
        if source.get("request_id").is_some() {
            assert_eq!(video["id"], source["request_id"]);
            assert_eq!(video["status"], "queued");
            assert_eq!(video["progress"], source.get("progress").cloned().unwrap_or(0.into()));
        } else if source["status"] == "done" {
            assert_eq!(video["status"], "completed");
            assert_eq!(video["progress"], 100);
            assert_eq!(video["video_url"], source["video"]["url"]);
        }
    }
}

fn assert_requests(s: &Scenario, requests: &[String], addr: &str, pinned: &mut BTreeMap<String, String>) {
    if expected_status(s) == 400 && s.upstreams.is_empty()
        || s.name.starts_with("models_")
        || s.name == "gen_xai_after_429"
    {
        assert!(requests.is_empty(), "{}: invalid requests never reach upstream", s.name);
        return;
    }
    assert_eq!(
        requests.len(),
        match s.name.as_str() {
            "gen_xai_429" => 2,
            "speech_unknown_voice_404" => 1,
            _ => s.upstreams.len(),
        },
        "{}: each configured credential is attempted once, downloads follow their poll",
        s.name
    );
    if matches!(s.name.as_str(), "gen_xai_429" | "speech_model_not_found_404") {
        let credentials: std::collections::BTreeSet<_> = requests
            .iter()
            .map(|raw| {
                raw.lines()
                    .find_map(|line| {
                        line.split_once(':')
                            .filter(|(key, _)| key.eq_ignore_ascii_case("authorization"))
                            .map(|(_, value)| value.trim())
                    })
                    .unwrap()
            })
            .collect();
        assert_eq!(
            credentials.len(),
            2,
            "rate limiting exhausts both configured credentials without retrying one"
        );
    }
    for (hop, raw) in requests.iter().enumerate() {
        let (head, body) = raw.split_once("\r\n\r\n").unwrap();
        let mut lines = head.lines();
        let line = lines.next().unwrap();
        let target = line.split_whitespace().nth(1).unwrap();
        let method = line.split_whitespace().next().unwrap();
        let expected_path = if s.path.contains("/images/") {
            s.path.clone()
        } else if s.path.contains("/audio/speech") || s.path.ends_with("/tts") {
            "/v1/tts".into()
        } else if s.method == "GET" {
            if hop == 1 {
                let poll: serde_json::Value = serde_json::from_str(&s.upstreams[0].body).unwrap();
                let url = poll["video"]["url"].as_str().unwrap();
                url.strip_prefix("http://UPSTREAM").unwrap().to_owned()
            } else {
                s.path
                    .trim_start_matches("/openai")
                    .trim_end_matches("/content")
                    .to_owned()
            }
        } else if s.path == "/v1/videos" || s.path.starts_with("/openai/") {
            "/v1/videos/generations".into()
        } else {
            s.path.clone()
        };
        assert_eq!(target, expected_path, "{}: exact upstream endpoint", s.name);
        assert_eq!(method, if s.method == "GET" { "GET" } else { "POST" }, "{}", s.name);
        if method == "POST" {
            assert_payload(s, body);
        }
        let headers: BTreeMap<String, String> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_owned()))
            .collect();
        assert_eq!(headers["host"], addr);
        assert!(
            !head.contains("client-key-1"),
            "client key must not become an upstream credential"
        );
        assert_eq!(
            headers
                .get("content-length")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0),
            body.len()
        );
        if hop == 1 && s.method == "GET" {
            for secret in ["authorization", "x-api-key", "cookie"] {
                assert!(
                    !headers.contains_key(secret),
                    "{}: content URL must not receive {secret}",
                    s.name
                );
            }
        } else if target.starts_with("/v1/") {
            if s.path.contains("/videos/") && s.method == "GET" {
                let id = s.path.trim_end_matches("/content").rsplit('/').next().unwrap();
                if let Some(creator) = pinned.get(id) {
                    assert_eq!(
                        &headers["authorization"], creator,
                        "{}: job stays pinned to creator",
                        s.name
                    );
                }
            }
            if s.method == "POST"
                && s.path.contains("/videos")
                && let Some(reply) = s.upstreams.first()
            {
                let response: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
                if let Some(id) = response["request_id"].as_str().or(response["id"].as_str()) {
                    pinned.insert(id.into(), headers["authorization"].clone());
                }
            }
            assert!(
                headers
                    .get("authorization")
                    .is_some_and(|value| value.starts_with("Bearer sk-fake-xai-"))
            );
            assert_eq!(
                headers["user-agent"],
                s.headers.get("User-Agent").map_or(DRIVER_USER_AGENT, String::as_str)
            );
        }
        if let Ok(payload) = serde_json::from_str::<serde_json::Value>(body) {
            if let Some(model) = payload["model"].as_str() {
                assert!(!model.contains('/'), "route aliases stripped: {model}");
            }
            if let Some(prompt) = payload["prompt"].as_str() {
                assert_eq!(prompt, prompt.trim());
            }
            if let Some(input) = payload["text"].as_str() {
                assert_eq!(input, input.trim());
            }
        }
    }
}

#[tokio::test]
async fn media_routes_validate_inputs_and_preserve_provider_results() {
    for (tag, input) in [
        ("media", include_str!("gateway/fixtures/media_inputs.json")),
        ("speech", include_str!("gateway/fixtures/speech_inputs.json")),
    ] {
        let fixture: Fixture = serde_json::from_str(input).unwrap();
        replay(tag, &fixture).await;
    }
}

async fn replay(tag: &str, fixture: &Fixture) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let upstream = Arc::new(Capture::default());
    let peer = tokio::spawn(capture(listener, upstream.clone(), addr.clone()));
    let auth_dir = std::env::temp_dir().join(format!("cpa-{tag}-{}", uuid::Uuid::new_v4()));
    let config = fixture
        .config
        .replace("UPSTREAM", &addr)
        .replace("PORT", "0")
        .replace("AUTHDIR", &auth_dir.to_string_lossy());
    let config = Config::parse(&format!("{config}\nrouting:\n  strategy: round-robin\n")).unwrap();
    let credentials = cliproxy_internal::config::credentials::load(&config);
    let executors = Executors {
        claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: Default::default(),
        meta: Default::default(),
        xai: Default::default(),
    };
    let rt = Arc::new(cliproxy_internal::gateway::testing::runtime(
        config,
        credentials,
        executors,
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router(rt)).await.unwrap() });
    let client = wreq::Client::new();
    let mut pinned = BTreeMap::new();
    for s in &fixture.scenarios {
        *upstream.replies.lock().unwrap() = s.upstreams.iter().cloned().collect();
        upstream.requests.lock().unwrap().clear();
        let method = wreq::Method::from_bytes(s.method.as_bytes()).unwrap();
        let mut req = client
            .request(method, format!("{base}{}", s.path))
            .bearer_auth("client-key-1")
            .header("User-Agent", DRIVER_USER_AGENT);
        if !s.content_type.is_empty() {
            req = req.header("Content-Type", &s.content_type);
        }
        for (name, value) in &s.headers {
            req = req.header(name, value);
        }
        if s.method == "POST" {
            let mut body = Vec::new();
            if s.skip_prefix > 0 {
                body.extend([0x50, 0x2a, 0x4d, 0x18]);
                body.extend((s.skip_prefix as u32).to_le_bytes());
                body.resize(body.len() + s.skip_prefix, 0);
            }
            body.extend(if s.body_b64.is_empty() {
                s.body.clone().into_bytes()
            } else {
                base64::engine::general_purpose::STANDARD.decode(&s.body_b64).unwrap()
            });
            req = req.body(body);
        }
        let response = req.send().await.unwrap_or_else(|e| panic!("{}: {e}", s.name));
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let body = response.bytes().await.unwrap();
        assert_response(s, status, &headers, &body, &addr);
        assert_requests(s, &upstream.requests.lock().unwrap(), &addr, &mut pinned);
    }
    if tag == "speech" {
        let response = client
            .post(format!("{base}/v1/tts"))
            .bearer_auth("client-key-1")
            .header("Content-Type", "application/json")
            .body(vec![b' '; cliproxy_internal::gateway::MAX_REQUEST_BYTES + 1])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 400);
        assert!(
            serde_json::from_slice::<serde_json::Value>(&response.bytes().await.unwrap())
                .unwrap()
                .get("error")
                .is_some()
        );
    }
    server.abort();
    peer.abort();
    let _ = std::fs::remove_dir_all(auth_dir);
}
