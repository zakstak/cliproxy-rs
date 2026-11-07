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

/// How [`Fixture::build`] sets up a proxy. `providers` is the YAML under `api-keys:`, with
/// `{codex}` and `{control}` standing for the two capture upstreams.
struct Setup<'a> {
    name: &'a str,
    providers: String,
    routing: &'a str,
    caps: &'a str,
    /// Credentials beyond the config's, given the Codex upstream's URL.
    extra: fn(&str) -> Vec<cpa_core::credential::Credential>,
    /// Install after-auth, response and chunk hooks that rewrite whenever they run.
    rewriting_hooks: bool,
}

fn default_providers(keys: &[&str]) -> String {
    let keys: String = keys.iter().map(|k| format!("{{api-key: {k}}}, ")).collect();
    format!(
        "  codex:\n    - base-url: {{codex}}\n      models: [{{name: gpt-native}}]\n      keys: [{}]\n  openai-compatibility:\n    - name: Acme\n      base-url: {{control}}/v1\n      models:\n        - name: up-model\n          alias: control-model\n      keys:\n        - api-key: key-FAKE-control\n",
        keys.trim_end_matches(", ")
    )
}

impl Fixture {
    async fn start() -> Option<Self> {
        Self::start_with("codex-native-plugins", &["key-FAKE-codex"], "").await
    }

    /// One Codex API-key credential per entry of `keys` plus the control provider;
    /// `routing` is extra config.
    async fn start_with(name: &str, keys: &[&str], routing: &str) -> Option<Self> {
        Self::build(Setup {
            name,
            providers: default_providers(keys),
            routing,
            caps: "request_interceptor,response_interceptor,stream_chunk_interceptor,request_lifecycle_plugin",
            extra: |_| Vec::new(),
            rewriting_hooks: true,
        })
        .await
    }

    async fn build(setup: Setup<'_>) -> Option<Self> {
        let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
        let built = cpa_plugin::testing::built_plugins(tmp, "codex_native_plugins")?;
        let dir = cpa_plugin::testing::scratch(tmp, setup.name);
        let (auths, plugins, records) = (dir.join("auths"), dir.join("plugins"), dir.join("records"));
        for d in [&auths, &plugins, &records.join("respond/mutator")] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::copy(built.join("recorder.so"), plugins.join("mutator.so")).unwrap();
        let sse = || Reply::sse(fixture("sse-completed").bytes());
        let codex = CaptureUpstream::start(Script {
            http: (0..40).map(|_| sse()).collect(),
            ..Script::default()
        })
        .await;
        let chat = r#"{"id":"c1","object":"chat.completion","created":1,"model":"up-model","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]}"#;
        let control = CaptureUpstream::start(Script {
            http: (0..40).map(|_| Reply::json(200, chat)).collect(),
            ..Script::default()
        })
        .await;
        let providers = setup
            .providers
            .replace("{codex}", &codex.base_url)
            .replace("{control}", &control.base_url);
        let yaml = format!(
            "auth-dir: {}\naccess:\n  api-keys: [client-key-1]\n{}api-keys:\n{providers}plugins:\n  enabled: true\n  dir: {}\n  configs:\n    mutator:\n      enabled: true\n      label: mutator\n      record: {}\n      caps: {}\n",
            auths.display(),
            setup.routing,
            plugins.display(),
            records.display(),
            setup.caps
        );
        let cfg = Config::parse(&yaml).unwrap();
        let mut credentials = cpa_core::config::credentials::load(&cfg);
        credentials.extend((setup.extra)(&codex.base_url));
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
        if !setup.rewriting_hooks {
            return Some(fixture);
        }
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

/// Which Codex key served the last request, and its outbound `session-id`.
fn served(fx: &Fixture) -> (String, String) {
    let sent = fx.codex.http().pop().expect("a Codex request");
    let key = sent.headers["authorization"].to_str().unwrap().to_owned();
    let session = sent
        .headers
        .get("session-id")
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    (key, session)
}

/// Session affinity binds two sessions to two different Codex credentials; a
/// before-auth rewrite of `Session-Id` cannot move a native request to the other one,
/// and its termination cannot stop it. The control provider still honours termination.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn before_auth_cannot_choose_the_native_session_or_credential() {
    let Some(fx) = Fixture::start_with(
        "codex-native-plugins-affinity",
        &["key-FAKE-a", "key-FAKE-b"],
        "routing:\n  session-affinity: true\n",
    )
    .await
    else {
        return;
    };
    let body = native_body(true);
    let send = |session: String| {
        let fx = &fx;
        let body = body.clone();
        async move {
            let (status, answer) = fx.post("/v1/responses", &[("session-id", &session)], &body).await;
            assert_eq!(status, 200, "{answer}");
            served(fx).0
        }
    };
    // Bind sessions until two land on different credentials.
    let mut bound: Vec<(String, String)> = Vec::new();
    for i in 0..12 {
        let session = format!("session-{i}");
        let key = send(session.clone()).await;
        if bound.iter().all(|(_, k)| *k != key) {
            bound.push((session, key));
        }
        if bound.len() == 2 {
            break;
        }
    }
    let [(first, first_key), (second, second_key)] = <[_; 2]>::try_from(bound).expect("two credentials bound");
    assert_eq!(send(first.clone()).await, first_key, "affinity holds");
    assert_eq!(send(second.clone()).await, second_key, "affinity holds");

    // The plugin points the first session at the second one's binding.
    fx.respond_once(
        "request.intercept_before",
        &json!({"ok": true, "result": {"Headers": {"Session-Id": [second.clone()]}}}),
    );
    assert_eq!(send(first.clone()).await, first_key, "the client's session decides");
    fx.respond_once(
        "request.intercept_before",
        &json!({"ok": true, "result": {"Terminate": true, "StatusCode": 403, "ResponseBody": b64("blocked")}}),
    );
    assert_eq!(
        send(first.clone()).await,
        first_key,
        "termination does not reach the native route"
    );

    // The control (not a Codex model) keeps before-auth termination.
    fx.respond_once(
        "request.intercept_before",
        &json!({"ok": true, "result": {"Terminate": true, "StatusCode": 403, "ResponseBody": b64("blocked")}}),
    );
    let (status, answer) = fx
        .post(
            "/v1/chat/completions",
            &[],
            r#"{"model":"control-model","messages":[{"role":"user","content":"hi"}]}"#,
        )
        .await;
    assert_eq!((status, answer.as_str()), (403, "blocked"));
}

/// Without an explicit session or cache key, the outbound session identity derives from
/// the client's body, not from a before-auth body rewrite.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn before_auth_body_rewrites_do_not_derive_the_native_session() {
    let Some(fx) = Fixture::start_with("codex-native-plugins-derived", &["key-FAKE-codex"], "").await else {
        return;
    };
    let original = r#"{"model":"gpt-native","stream":true,"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"original question"}]}]}"#;
    let rewritten = r#"{"model":"gpt-native","stream":true,"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"rewritten question"}]}]}"#;
    let derived = |body: &'static str| {
        let fx = &fx;
        async move {
            let (status, answer) = fx.post("/v1/responses", &[], body).await;
            assert_eq!(status, 200, "{answer}");
            served(fx).1
        }
    };
    let from_original = derived(original).await;
    let from_rewritten = derived(rewritten).await;
    assert!(!from_original.is_empty(), "a derived session identity is sent");
    assert_ne!(from_original, from_rewritten, "the body decides the derived identity");

    fx.rewrite_before_auth(rewritten);
    let (status, _) = fx.post("/v1/responses", &[], original).await;
    assert_eq!(status, 200);
    let sent = fx.codex.http().pop().unwrap();
    assert_eq!(String::from_utf8_lossy(&sent.body), original);
    assert_eq!(
        sent.headers["session-id"],
        from_original.as_str(),
        "derived from the client's body"
    );
}

const MIXED: &str = "  codex:\n    - base-url: {codex}\n      models: [{name: gpt-mixed}]\n      keys: [{api-key: key-FAKE-codex}]\n  openai-compatibility:\n    - name: Acme\n      base-url: {control}/v1\n      models:\n        - name: up-model\n          alias: gpt-mixed\n      keys:\n        - api-key: key-FAKE-control\n";

/// A model a native Codex credential shares with another provider. Each attempt gets
/// what its route allows: a native Codex attempt the client's request, the other
/// provider the before-auth rewrite; a before-auth termination stops only the other
/// provider's attempt; and the plugin scheduler always sees the client's headers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mixed_pool_applies_before_auth_only_to_attempts_that_are_not_native() {
    let Some(fx) = Fixture::build(Setup {
        name: "codex-native-plugins-mixed",
        providers: MIXED.to_owned(),
        routing: "",
        caps: "request_interceptor,request_lifecycle_plugin,scheduler",
        extra: |_| Vec::new(),
        rewriting_hooks: false,
    })
    .await
    else {
        return;
    };
    let original = r#"{"model":"gpt-mixed","stream":false,"input":"client question"}"#;
    let rewritten = r#"{"model":"gpt-mixed","stream":false,"input":"before-auth body"}"#;
    let picks = || -> Vec<Value> {
        cpa_server::testing::records(&fx.records.join("mutator.jsonl"))
            .into_iter()
            .filter(|r| r["method"] == "scheduler.pick")
            .map(|r| serde_json::from_str(r["request"].as_str().unwrap()).unwrap())
            .collect()
    };
    let rewrite = || {
        fx.respond_once(
            "request.intercept_before",
            &json!({"ok": true, "result": {
                "Headers": {"Session-Id": ["plugin-session"], "X-Plugin": ["1"]},
                "Body": b64(rewritten),
            }}),
        )
    };
    let terminate = || {
        fx.respond_once(
            "request.intercept_before",
            &json!({"ok": true, "result": {"Terminate": true, "StatusCode": 403, "ResponseBody": b64("blocked")}}),
        )
    };
    let client = [("session-id", "client-session")];

    // The plugin scheduler is offered both providers and leaves the choice to the
    // built-in selector, which serves the native Codex credential.
    fx.respond("scheduler.pick", &json!({"ok": true, "result": {"handled": false}}));
    rewrite();
    let (status, answer) = fx.post("/v1/responses", &client, original).await;
    assert_eq!(status, 200, "{answer}");
    let pick = picks().pop().expect("the plugin scheduler was asked");
    let candidate = |provider: &str| {
        pick["Candidates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["Provider"] == provider)
            .map(|c| c["ID"].as_str().unwrap().to_owned())
            .unwrap_or_else(|| panic!("no {provider} candidate: {pick}"))
    };
    let (codex_id, other_id) = (candidate("codex"), candidate("openai-compatible-acme"));
    let sent = fx.codex.http().pop().expect("the native credential served it");
    assert_eq!(
        String::from_utf8_lossy(&sent.body),
        original,
        "native: the client's body"
    );
    assert_eq!(sent.headers["session-id"], "client-session");
    assert!(!sent.headers.contains_key("x-plugin"));
    let pick_auth = |id: &str| {
        fx.respond_once(
            "scheduler.pick",
            &json!({"ok": true, "result": {"handled": true, "auth_id": id}}),
        )
    };

    // The other provider's attempt keeps the rewrite.
    pick_auth(&other_id);
    rewrite();
    let (status, answer) = fx.post("/v1/responses", &client, original).await;
    assert_eq!(status, 200, "{answer}");
    let sent = fx.control.http().pop().expect("the other provider served it");
    let body = String::from_utf8_lossy(&sent.body);
    assert!(
        body.contains("before-auth body"),
        "the other provider keeps the rewrite: {body}"
    );

    // A before-auth termination stops the other provider's attempt only.
    let (codex, control) = (fx.codex.http().len(), fx.control.http().len());
    pick_auth(&codex_id);
    terminate();
    let (status, answer) = fx.post("/v1/responses", &client, original).await;
    assert_eq!(status, 200, "native attempts ignore the termination: {answer}");
    assert_eq!(fx.codex.http().len(), codex + 1);
    pick_auth(&other_id);
    terminate();
    let (status, answer) = fx.post("/v1/responses", &client, original).await;
    assert_eq!((status, answer.as_str()), (403, "blocked"));
    assert_eq!(fx.control.http().len(), control, "a terminated attempt sends nothing");

    // Every pick saw the client's headers, never the plugin's.
    let all = picks();
    assert!(all.len() >= 4, "{} picks", all.len());
    for pick in &all {
        assert_eq!(
            pick["Options"]["Headers"]["Session-Id"],
            json!(["client-session"]),
            "{pick}"
        );
        assert!(pick["Options"]["Headers"].get("X-Plugin").is_none(), "{pick}");
    }
}

/// A model only a Sign in with ChatGPT credential serves cannot be served natively:
/// before-auth output and termination apply as in Go.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_siwc_only_pool_keeps_before_auth() {
    let Some(fx) = Fixture::build(Setup {
        name: "codex-native-plugins-siwc",
        providers: "  openai-compatibility:\n    - name: Acme\n      base-url: {control}/v1\n      models: [{name: up-model, alias: control-model}]\n      keys: [{api-key: key-FAKE-control}]\n".to_owned(),
        routing: "",
        caps: "request_interceptor,request_lifecycle_plugin",
        extra: |codex| {
            let metadata = json!({
                "type": "codex", "access_token": "at-FAKE", "account_id": "acct-FAKE",
                "use_siwc": true, "siwc": {"access_token": "siwc-at-FAKE"},
            });
            let mut c = cpa_core::credential::Credential::from_file(
                Path::new("/fake"),
                Path::new("/fake/codex-siwc.json"),
                metadata.as_object().unwrap().clone(),
            )
            .unwrap();
            c.attributes.insert("base_url".into(), codex.into());
            vec![c]
        },
        rewriting_hooks: false,
    })
    .await
    else {
        return;
    };
    let original = r#"{"model":"gpt-5.5","stream":true,"input":"client question"}"#;
    fx.rewrite_before_auth(r#"{"model":"gpt-5.5","stream":true,"input":"before-auth body"}"#);
    let (status, answer) = fx.post("/v1/responses", &[], original).await;
    assert_eq!(status, 200, "{answer}");
    let sent = fx.codex.http().pop().expect("the SIWC credential served it");
    let body = String::from_utf8_lossy(&sent.body);
    assert!(
        body.contains("before-auth body"),
        "the legacy path keeps the rewrite: {body}"
    );
    fx.respond_once(
        "request.intercept_before",
        &json!({"ok": true, "result": {"Terminate": true, "StatusCode": 403, "ResponseBody": b64("blocked")}}),
    );
    let (status, answer) = fx.post("/v1/responses", &[], original).await;
    assert_eq!((status, answer.as_str()), (403, "blocked"));
    assert!(fx.codex.http().len() == 1, "nothing sent after the termination");
}

/// A Responses model only another provider serves keeps Go's selection: a before-auth
/// `Session-Id` rewrite moves it to the credential that session is bound to.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pure_non_codex_responses_model_keeps_before_auth_selection() {
    let Some(fx) = Fixture::build(Setup {
        name: "codex-native-plugins-non-codex",
        providers: "  openai-compatibility:\n    - name: Acme\n      base-url: {control}/v1\n      models: [{name: up-model, alias: control-model}]\n      keys: [{api-key: key-FAKE-control-a}, {api-key: key-FAKE-control-b}]\n".to_owned(),
        routing: "routing:\n  session-affinity: true\n",
        caps: "request_interceptor,request_lifecycle_plugin",
        extra: |_| Vec::new(),
        rewriting_hooks: false,
    })
    .await
    else {
        return;
    };
    let body = r#"{"model":"control-model","stream":false,"input":"hi"}"#;
    let send = |session: String| {
        let fx = &fx;
        async move {
            let (status, answer) = fx.post("/v1/responses", &[("session-id", &session)], body).await;
            assert_eq!(status, 200, "{answer}");
            let sent = fx.control.http().pop().expect("the control provider served it");
            sent.headers["authorization"].to_str().unwrap().to_owned()
        }
    };
    let mut bound: Vec<(String, String)> = Vec::new();
    for i in 0..12 {
        let session = format!("session-{i}");
        let key = send(session.clone()).await;
        if bound.iter().all(|(_, k)| *k != key) {
            bound.push((session, key));
        }
        if bound.len() == 2 {
            break;
        }
    }
    let [(first, first_key), (second, second_key)] = <[_; 2]>::try_from(bound).expect("two keys bound");
    assert_eq!(send(first.clone()).await, first_key);
    fx.respond_once(
        "request.intercept_before",
        &json!({"ok": true, "result": {"Headers": {"Session-Id": [second.clone()]}}}),
    );
    assert_eq!(
        send(first.clone()).await,
        second_key,
        "Go's selection follows the rewrite"
    );
}
