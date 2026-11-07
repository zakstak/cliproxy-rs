//! Plugin interceptors around the native Codex HTTP route (#23), through the real router
//! with the Go `recorder` plugin. Every hook tries to rewrite: before-auth headers, body
//! and path, after-auth body and headers, the buffered response and each stream chunk.
//! A native Codex request reaches the upstream and the client unchanged, and its plugin
//! lifecycle still completes. A non-Codex control (an OpenAI-compatible provider) keeps
//! every hook. Loopback upstreams, fake keys and a private `auth-dir`.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::codex_contract::fixture;
use cpa_server::testing::codex_capture::{CaptureUpstream, Reply, Script};
use serde_json::{Value, json};

struct Fixture {
    base: String,
    records: PathBuf,
    codex: CaptureUpstream,
    control: CaptureUpstream,
}

fn b64(text: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(text)
}

impl Fixture {
    async fn start() -> Option<Self> {
        let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
        let built = cpa_plugin::testing::built_plugins(tmp, "codex_native_plugins")?;
        let dir = cpa_plugin::testing::scratch(tmp, "codex-native-plugins");
        let (auths, plugins, records) = (dir.join("auths"), dir.join("plugins"), dir.join("records"));
        for d in [&auths, &plugins, &records.join("respond/mutator")] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::copy(built.join("recorder.so"), plugins.join("mutator.so")).unwrap();
        let sse = || Reply::sse(fixture("sse-completed").bytes());
        let codex = CaptureUpstream::start(Script {
            http: vec![sse(), sse()],
            ..Script::default()
        })
        .await;
        let chat = r#"{"id":"c1","object":"chat.completion","created":1,"model":"up-model","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]}"#;
        let control = CaptureUpstream::start(Script {
            http: vec![Reply::json(200, chat)],
            ..Script::default()
        })
        .await;
        let yaml = format!(
            "auth-dir: {}\naccess:\n  api-keys: [client-key-1]\napi-keys:\n  codex:\n    - base-url: {}\n      models: [{{name: gpt-native}}]\n      keys: [{{api-key: key-FAKE-codex}}]\n  openai-compatibility:\n    - name: Acme\n      base-url: {}/v1\n      models:\n        - name: up-model\n          alias: control-model\n      keys:\n        - api-key: key-FAKE-control\nplugins:\n  enabled: true\n  dir: {}\n  configs:\n    mutator:\n      enabled: true\n      label: mutator\n      record: {}\n      caps: request_interceptor,response_interceptor,stream_chunk_interceptor,request_lifecycle_plugin\n",
            auths.display(),
            codex.base_url,
            control.base_url,
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
        let router = cpa_server::router(rt);
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let fixture = Self {
            base,
            records,
            codex,
            control,
        };
        // Hooks that rewrite whenever they run.
        fixture.respond(
            "request.intercept_after",
            &json!({"ok": true, "result": {
                "Headers": {"X-After": ["1"], "Session-Id": ["after-session"]},
                "Body": b64(r#"{"model":"up-model","messages":[{"role":"user","content":"after-auth body"}]}"#),
            }}),
        );
        fixture.respond(
            "response.intercept_after",
            &json!({"ok": true, "result": {"Body": b64(r#"{"rewritten":"response"}"#)}}),
        );
        fixture.respond(
            "response.intercept_stream_chunk",
            &json!({"ok": true, "result": {"Body": b64("data: {\"rewritten\":\"chunk\"}\n\n")}}),
        );
        Some(fixture)
    }

    fn respond(&self, method: &str, envelope: &Value) {
        std::fs::write(
            self.records.join(format!("respond/mutator/{method}.json")),
            envelope.to_string(),
        )
        .unwrap();
    }

    fn respond_once(&self, method: &str, envelope: &Value) {
        std::fs::write(
            self.records.join(format!("respond/mutator/{method}.once.json")),
            envelope.to_string(),
        )
        .unwrap();
    }

    fn calls(&self, method: &str) -> usize {
        cpa_server::testing::records(&self.records.join("mutator.jsonl"))
            .into_iter()
            .filter(|record| record["method"] == method)
            .count()
    }

    async fn post(&self, path: &str, headers: &[(&str, &str)], body: &str) -> (u16, String) {
        let mut request = wreq::Client::new()
            .post(format!("{}{path}", self.base))
            .header("authorization", "Bearer client-key-1")
            .header("content-type", "application/json");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request.body(body.to_owned()).send().await.unwrap();
        let status = response.status().as_u16();
        (status, response.text().await.unwrap())
    }

    /// Before-auth rewrites the next request's headers, body and path.
    fn rewrite_before_auth(&self, body: &str) {
        self.respond_once(
            "request.intercept_before",
            &json!({"ok": true, "result": {
                "Headers": {"Session-Id": ["plugin-session"], "X-Codex-Turn-State": ["plugin-state"]},
                "Body": b64(body),
                "path": "/v1/plugin-path",
            }}),
        );
    }

    async fn lifecycle_completions(&self, at_least: usize) -> usize {
        for _ in 0..100 {
            let done = self.calls("request.complete");
            if done >= at_least {
                return done;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        self.calls("request.complete")
    }
}

fn native_body(stream: bool) -> String {
    let body = String::from_utf8(fixture("http-request-full").bytes().to_vec())
        .unwrap()
        .replacen(r#""model":"gpt-test""#, r#""model":"gpt-native""#, 1);
    if stream {
        body
    } else {
        body.replacen(r#""stream":true"#, r#""stream":false"#, 1)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugin_interceptors_cannot_rewrite_native_codex_traffic() {
    let Some(fx) = Fixture::start().await else {
        return;
    };
    let client = [("session-id", "client-session"), ("user-agent", "OpenAI/Python 2.1.0")];

    // A native stream: before-auth runs (the provider is not known yet), but none of its
    // output reaches the attempt, and no later hook runs.
    let body = native_body(true);
    fx.rewrite_before_auth(r#"{"model":"gpt-native","input":"before-auth body","stream":true}"#);
    let (status, answer) = fx.post("/v1/responses", &client, &body).await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(
        answer,
        String::from_utf8(fixture("sse-completed").bytes().to_vec()).unwrap(),
        "the upstream's events"
    );
    let sent = fx.codex.http().pop().expect("the Codex upstream got the request");
    assert_eq!(String::from_utf8_lossy(&sent.body), body, "the client's body");
    assert_eq!(sent.uri.path(), "/responses");
    assert_eq!(sent.headers["session-id"], "client-session", "the client's header");
    assert!(
        !sent.headers.contains_key("x-codex-turn-state"),
        "no before-auth header"
    );
    assert!(!sent.headers.contains_key("x-after"), "no after-auth header");
    assert_eq!(fx.calls("request.intercept_before"), 1);
    assert_eq!(fx.calls("request.intercept_after"), 0);
    assert_eq!(fx.calls("response.intercept_stream_chunk"), 0);

    // A native buffered request: the response interceptor does not run either.
    let body = native_body(false);
    fx.rewrite_before_auth(r#"{"model":"gpt-native","input":"before-auth body"}"#);
    let (status, answer) = fx.post("/v1/responses", &client, &body).await;
    assert_eq!(status, 200, "{answer}");
    let terminal = String::from_utf8(fixture("sse-completed").frames().pop().unwrap().to_vec()).unwrap();
    let object = &terminal[terminal.find(r#""response":"#).unwrap() + 11..terminal.len() - 1];
    assert_eq!(answer, object, "the terminal response object");
    let sent = fx.codex.http().pop().unwrap();
    // An API-key credential forwards a buffered request unchanged, `stream: false`
    // included; the upstream answered with a stream, which is aggregated.
    assert_eq!(String::from_utf8_lossy(&sent.body), body, "the client's body");
    assert_eq!(sent.headers["session-id"], "client-session");
    assert_eq!(fx.calls("request.intercept_after"), 0);
    assert_eq!(fx.calls("response.intercept_after"), 0);
    // Both native requests still complete their plugin lifecycle.
    assert!(fx.lifecycle_completions(2).await >= 2, "lifecycle completions");

    // The control: a non-Codex provider keeps every hook.
    fx.rewrite_before_auth(r#"{"model":"control-model","messages":[{"role":"user","content":"before-auth body"}]}"#);
    let (status, answer) = fx
        .post(
            "/v1/chat/completions",
            &client,
            r#"{"model":"control-model","messages":[{"role":"user","content":"client body"}]}"#,
        )
        .await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer, r#"{"rewritten":"response"}"#, "the response interceptor ran");
    let sent = fx.control.http().pop().expect("the control upstream got the request");
    let sent_body = String::from_utf8_lossy(&sent.body);
    assert!(sent_body.contains("after-auth body"), "{sent_body}");
    assert_eq!(fx.calls("request.intercept_before"), 3);
    assert_eq!(fx.calls("request.intercept_after"), 1);
    assert_eq!(fx.calls("response.intercept_after"), 1);
    assert!(fx.lifecycle_completions(3).await >= 3);
}
