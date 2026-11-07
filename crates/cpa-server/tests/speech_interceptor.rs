//! The speech Content-Type under a response interceptor with passthrough headers off
//! (sdk/api/handlers/openai/openai_speech_handlers.go): Go picks the audio type from
//! the headers the interceptors changed, falling back to the requested format's type
//! when they name no audio. The upstream's own type is not passed through.

use std::path::Path;
use std::sync::Arc;

use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_server::router;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interceptor_content_type_decides_the_audio_type() {
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let Some(built) = cpa_plugin::testing::built_plugins(tmp, "speech_interceptor") else {
        return;
    };
    let dir = cpa_plugin::testing::scratch(tmp, "speech-interceptor");
    let (auths, plugins, records) = (dir.join("auths"), dir.join("plugins"), dir.join("records"));
    let respond = records.join("respond/shaper");
    for d in [&auths, &plugins, &respond] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::copy(built.join("recorder.so"), plugins.join("shaper.so")).unwrap();
    // The xAI upstream answers every request with WAV audio.
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_url = format!("http://{}", upstream.local_addr().unwrap());
    let audio = axum::Router::new().fallback(|| async { ([("content-type", "audio/wav")], "AUDIO") });
    tokio::spawn(async move { axum::serve(upstream, audio).await.unwrap() });
    let yaml = format!(
        "auth-dir: {}\naccess:\n  api-keys: [client-key-1]\napi-keys:\n  xai:\n    - name: xai-1\n      base-url: {upstream_url}/v1\n      keys:\n        - api-key: sk-fake-xai-a\nplugins:\n  enabled: true\n  dir: {}\n  configs:\n    shaper:\n      enabled: true\n      label: shaper\n      record: {}\n      caps: response_interceptor\n",
        auths.display(),
        plugins.display(),
        records.display()
    );
    let cfg = Config::parse(&yaml).unwrap();
    let credentials = cpa_core::config::credentials::load(&cfg);
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: Default::default(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(cfg, credentials, executors));
    cpa_server::install_registry(&rt);
    cpa_server::plugins::start(&rt).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router(rt)).await.unwrap() });

    // (interceptor Content-Type, requested format, expected Content-Type)
    for (intercepted, format, want) in [
        (None, "pcm", "audio/pcm"),
        (Some("audio/ogg"), "mp3", "audio/ogg"),
        (Some("application/json"), "wav", "audio/wav"),
        (Some("text/plain; charset=utf-8"), "mp3", "audio/mpeg"),
    ] {
        let headers = match intercepted {
            Some(value) => serde_json::json!({"Content-Type": [value]}),
            None => serde_json::json!({"X-Shaped": ["1"]}),
        };
        std::fs::write(
            respond.join("response.intercept_after.once.json"),
            serde_json::json!({"ok": true, "result": {"Headers": headers}}).to_string(),
        )
        .unwrap();
        let response = wreq::Client::new()
            .post(format!("{base}/v1/audio/speech"))
            .header("Authorization", "Bearer client-key-1")
            .body(format!(r#"{{"input":"hi","response_format":"{format}"}}"#))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200, "{intercepted:?}");
        let content_type = response.headers()["content-type"].to_str().unwrap().to_owned();
        assert_eq!(content_type, want, "{intercepted:?} {format}");
        if intercepted.is_none() {
            // The interceptor's other header changes still reach the client.
            assert_eq!(response.headers()["x-shaped"], "1");
        }
        assert_eq!(response.text().await.unwrap(), "AUDIO");
    }
}
