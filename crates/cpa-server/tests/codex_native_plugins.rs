//! Plugin interceptors around the native Codex HTTP route (#23), through the real router
//! with the Go `recorder` plugin. Every hook tries to rewrite: before-auth headers, body
//! and path, after-auth body and headers, the buffered response and each stream chunk.
//! A native Codex request reaches the upstream and the client unchanged, and its plugin
//! lifecycle still completes. A non-Codex control (an OpenAI-compatible provider) keeps
//! every hook. Loopback upstreams, fake keys and a private `auth-dir`.

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
    rt: Arc<cpa_server::Runtime>,
    codex: CaptureUpstream,
    control: CaptureUpstream,
}

fn b64(text: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(text)
}

/// Builds extra credentials from the Codex upstream's URL.
type Extra = Box<dyn Fn(&str) -> Vec<cpa_core::credential::Credential>>;

/// How [`Fixture::build`] sets up a proxy. `providers` is the YAML under `api-keys:`, with
/// `{codex}` and `{control}` standing for the two capture upstreams.
struct Setup<'a> {
    name: &'a str,
    providers: String,
    routing: &'a str,
    caps: &'a str,
    /// Credentials beyond the config's, given the Codex upstream's URL.
    extra: Extra,
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
            extra: Box::new(|_| Vec::new()),
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
            ws: (0..40).map(|_| fixture("sse-completed").frames()).collect(),
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
        let router = cpa_server::router(rt.clone());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let fixture = Self {
            base,
            records,
            rt,
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
        extra: Box::new(|_| Vec::new()),
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

/// A Sign in with ChatGPT credential takes the native route too (#28): a model only it
/// serves gets the client's body, and before-auth output and termination never reach
/// it, where Go applied both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_siwc_only_pool_is_native() {
    let Some(fx) = Fixture::build(Setup {
        name: "codex-native-plugins-siwc",
        providers: "  openai-compatibility:\n    - name: Acme\n      base-url: {control}/v1\n      models: [{name: up-model, alias: control-model}]\n      keys: [{api-key: key-FAKE-control}]\n".to_owned(),
        routing: "",
        caps: "request_interceptor,request_lifecycle_plugin",
        extra: Box::new(|codex| {
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
        }),
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
    assert_eq!(String::from_utf8_lossy(&sent.body), original, "the client's bytes");
    fx.respond_once(
        "request.intercept_before",
        &json!({"ok": true, "result": {"Terminate": true, "StatusCode": 403, "ResponseBody": b64("blocked")}}),
    );
    let (status, answer) = fx.post("/v1/responses", &[], original).await;
    assert_eq!(status, 200, "a native attempt ignores the termination: {answer}");
    assert_eq!(fx.codex.http().len(), 2, "the request went upstream");
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
        extra: Box::new(|_| Vec::new()),
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

/// A fake Home: hands out the queued credentials in order and counts released leases.
struct Home {
    picks: std::sync::Mutex<Vec<cpa_core::credential::Credential>>,
    released: Arc<std::sync::atomic::AtomicUsize>,
}

impl cpa_server::remote::RemoteDispatch for Home {
    fn available(&self) -> bool {
        true
    }

    fn dispatch(
        &self,
        _: cpa_server::remote::RemoteRequest,
    ) -> futures_util::future::BoxFuture<'_, Result<cpa_server::remote::RemoteGrant, cpa_server::remote::RemoteError>>
    {
        let credential = self.picks.lock().unwrap().remove(0);
        let released = self.released.clone();
        Box::pin(async move {
            Ok(cpa_server::remote::RemoteGrant {
                credential,
                end: Box::new(move || {
                    released.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    None
                }),
                cancel: None,
                request_retry: None,
                user_api_key: String::new(),
                private: Default::default(),
            })
        })
    }

    fn models(
        &self,
        _: Vec<(String, String)>,
        _: Vec<(String, String)>,
    ) -> futures_util::future::BoxFuture<'_, Result<Vec<u8>, cpa_server::remote::ModelsError>> {
        unreachable!("no model lists")
    }
}

/// A Codex credential a fake Home hands out: an API key (native) or Sign in with
/// ChatGPT (legacy), pointing at `base`.
fn home_credential(id: &str, siwc: bool, base: &str) -> cpa_core::credential::Credential {
    let mut metadata = json!({"type": "codex", "access_token": "at-FAKE", "account_id": "acct-FAKE"});
    if siwc {
        metadata["use_siwc"] = true.into();
        metadata["siwc"] = json!({"access_token": "siwc-at-FAKE"});
    }
    let mut c = cpa_core::credential::Credential::from_file(
        Path::new("/fake"),
        &Path::new("/fake").join(id),
        metadata.as_object().unwrap().clone(),
    )
    .unwrap();
    c.attributes.insert(
        cpa_core::config::credentials::HOME_AUTH_INDEX.into(),
        format!("idx-{id}"),
    );
    c.attributes.insert("base_url".into(), base.into());
    if !siwc {
        c.attributes.insert("api_key".into(), "key-FAKE-home".into());
    }
    c
}

/// Under Home a Codex pool may be native, so before-auth output is deferred. Every Codex
/// pick is native (#28), Sign in with ChatGPT included: each ignores the rewrite and the
/// termination, where Go gave a Sign in with ChatGPT pick both. Every lease is released
/// and every lifecycle completes. Home runs no after-auth chain.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn home_picks_of_every_codex_profile_are_native() {
    let Some(fx) = Fixture::build(Setup {
        name: "codex-native-plugins-home",
        providers: String::new(),
        routing: "",
        caps: "request_interceptor,request_lifecycle_plugin",
        extra: Box::new(|_| Vec::new()),
        rewriting_hooks: false,
    })
    .await
    else {
        return;
    };
    // An after-auth hook that would rewrite, to show Home still runs none.
    fx.respond(
        "request.intercept_after",
        &json!({"ok": true, "result": {"Body": b64(r#"{"model":"gpt-5.5","input":"after-auth body"}"#)}}),
    );
    let base = fx.codex.base_url.clone();
    let released = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    fx.rt.set_remote_dispatch(Some(Arc::new(Home {
        picks: std::sync::Mutex::new(vec![
            home_credential("native-1.json", false, &base),
            home_credential("siwc-1.json", true, &base),
            home_credential("native-2.json", false, &base),
            home_credential("siwc-2.json", true, &base),
        ]),
        released: released.clone(),
    })));
    let original = r#"{"model":"gpt-5.5","stream":true,"input":"client question"}"#;
    let rewritten = r#"{"model":"gpt-5.5","stream":true,"input":"before-auth body"}"#;
    let terminate = || {
        fx.respond_once(
            "request.intercept_before",
            &json!({"ok": true, "result": {"Terminate": true, "StatusCode": 403, "ResponseBody": b64("blocked")}}),
        )
    };

    // Native pick: the client's body.
    fx.rewrite_before_auth(rewritten);
    let (status, answer) = fx.post("/v1/responses", &[], original).await;
    assert_eq!(status, 200, "{answer}");
    let sent = fx.codex.http().pop().expect("the native pick served it");
    assert_eq!(String::from_utf8_lossy(&sent.body), original);

    // Sign in with ChatGPT pick: native too, so the client's body.
    fx.rewrite_before_auth(rewritten);
    let (status, answer) = fx.post("/v1/responses", &[], original).await;
    assert_eq!(status, 200, "{answer}");
    let sent = fx.codex.http().pop().expect("the SIWC pick served it");
    assert_eq!(String::from_utf8_lossy(&sent.body), original);
    assert_eq!(fx.calls("request.intercept_after"), 0, "Home runs no after-auth chain");

    // Native pick: a termination is ignored.
    terminate();
    let (status, answer) = fx.post("/v1/responses", &[], original).await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(fx.codex.http().len(), 3);

    // Sign in with ChatGPT pick: a termination is ignored too.
    terminate();
    let (status, answer) = fx.post("/v1/responses", &[], original).await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(fx.codex.http().len(), 4);

    assert_eq!(
        released.load(std::sync::atomic::Ordering::SeqCst),
        4,
        "every lease released"
    );
    assert!(fx.lifecycle_completions(4).await >= 4, "every lifecycle completed");
}

/// Upstream requests (path and body) of the credits test.
type Seen = std::sync::Mutex<Vec<(String, String)>>;

/// Codex always over quota; Antigravity over quota until a request asks for credits.
async fn credits_upstream(
    axum::extract::State(seen): axum::extract::State<Arc<Seen>>,
    req: axum::extract::Request,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let path = req.uri().path_and_query().map(|p| p.to_string()).unwrap_or_default();
    let body = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    seen.lock().unwrap().push((path.clone(), body.clone()));
    let json = [("content-type", "application/json")];
    if path.starts_with("/codex") {
        let limit = r#"{"error":{"type":"usage_limit_reached","message":"limit"}}"#;
        return (axum::http::StatusCode::TOO_MANY_REQUESTS, json, limit).into_response();
    }
    if path.contains("loadCodeAssist") {
        // Credits are available, whenever the executor's lookup runs.
        let credits = r#"{"paidTier":{"id":"g1-pro-tier","availableCredits":[{"creditType":"GOOGLE_ONE_AI","creditAmount":100,"minimumCreditAmountForUsage":1}]}}"#;
        return (json, credits).into_response();
    }
    if !body.contains(r#""enabledCreditTypes":["GOOGLE_ONE_AI"]"#) {
        let quota = r#"{"error":{"code":429,"message":"You have exhausted your capacity on this model.","status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"QUOTA_EXHAUSTED"},{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"3600s"}]}}"#;
        return (axum::http::StatusCode::TOO_MANY_REQUESTS, json, quota).into_response();
    }
    let answer = r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"paid with credits"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":4,"totalTokenCount":7},"modelVersion":"claude-sonnet-4-6","responseId":"msg-credits"},"traceId":"t"}"#;
    (
        [("content-type", "text/event-stream")],
        format!("data: {answer}\r\n\r\n"),
    )
        .into_response()
}

/// A Claude model a native Codex credential shares with Antigravity: after both fail on
/// quota, the buffered credits fallback (which runs no after-auth chain, as in Go)
/// still receives the deferred before-auth rewrite, and the native attempt never did.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_credits_fallback_gets_the_deferred_before_auth_rewrite() {
    let seen = Arc::new(Seen::default());
    let upstream = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = axum::Router::new().fallback(credits_upstream).with_state(seen.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        url
    };
    let antigravity = format!("{upstream}/antigravity");
    let Some(fx) = Fixture::build(Setup {
        name: "codex-native-plugins-credits",
        providers: format!(
            "  codex:\n    - base-url: {upstream}/codex\n      models: [{{name: claude-sonnet-4-6}}]\n      keys: [{{api-key: key-FAKE-codex}}]\n"
        ),
        routing: "quota-exceeded:\n  antigravity-credits: true\nrouting:\n  retry:\n    request-retry: 0\n",
        caps: "request_interceptor,request_lifecycle_plugin",
        extra: Box::new(move |_| {
            let mut metadata = serde_json::Map::new();
            for (k, v) in [
                ("type", "antigravity"),
                ("access_token", "ya29.fake-credits"),
                ("expired", "2099-01-01T00:00:00Z"),
                ("project_id", "proj-credits"),
                ("email", "credits@example.com"),
            ] {
                metadata.insert(k.into(), v.into());
            }
            let dir = Path::new("/fixture");
            let mut c = cpa_core::credential::Credential::from_file(dir, &dir.join("ag.json"), metadata).unwrap();
            c.attributes.insert("base_url".into(), antigravity.clone());
            vec![c]
        }),
        rewriting_hooks: false,
    })
    .await
    else {
        return;
    };
    let original = r#"{"model":"claude-sonnet-4-6","stream":false,"input":"client question"}"#;
    fx.rewrite_before_auth(r#"{"model":"claude-sonnet-4-6","stream":false,"input":"before-auth body"}"#);
    let (status, answer) = fx.post("/v1/responses", &[], original).await;
    assert_eq!(status, 200, "{answer}: {:?}", seen.lock().unwrap());
    assert!(answer.contains("paid with credits"), "{answer}");
    let seen = seen.lock().unwrap().clone();
    let codex: Vec<_> = seen.iter().filter(|(p, _)| p.starts_with("/codex")).collect();
    assert_eq!(codex.len(), 1, "the native attempt ran: {seen:?}");
    assert_eq!(codex[0].1, original, "the native attempt got the client's body");
    let credits: Vec<_> = seen
        .iter()
        .filter(|(_, b)| b.contains(r#""enabledCreditTypes":["GOOGLE_ONE_AI"]"#))
        .collect();
    assert_eq!(credits.len(), 1, "the credits fallback ran: {seen:?}");
    assert!(credits[0].1.contains("before-auth body"), "{}", credits[0].1);
    assert!(!credits[0].1.contains("client question"), "{}", credits[0].1);
}

/// The native WebSocket route (#24) with the same hooks: before-auth runs when the turn
/// starts, but its headers, body and path never reach the native turn, and no after-auth,
/// response or chunk hook runs. The upstream gets the client's frame (on a socket, or as
/// an HTTP body without its tag for a credential without `websockets`, #28) and the client
/// gets the upstream's frames. The turn still completes its plugin lifecycle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugin_interceptors_cannot_rewrite_native_websocket_turns() {
    for websockets in [true, false] {
        let key = if websockets {
            "key-FAKE-codex, websockets: true"
        } else {
            "key-FAKE-codex"
        };
        let name = if websockets {
            "codex-native-ws-plugins"
        } else {
            "codex-native-ws-http-plugins"
        };
        let Some(fx) = Fixture::build(Setup {
            name,
            providers: default_providers(&[key]),
            routing: "",
            caps: "request_interceptor,response_interceptor,stream_chunk_interceptor,request_lifecycle_plugin",
            extra: Box::new(|_| Vec::new()),
            rewriting_hooks: true,
        })
        .await
        else {
            return;
        };
        fx.rewrite_before_auth(r#"{"type":"response.create","model":"gpt-native","input":"before-auth body"}"#);
        let mut socket = wreq::Client::new()
            .websocket(format!("{}/v1/responses", fx.base.replacen("http://", "ws://", 1)))
            .header("authorization", "Bearer client-key-1")
            .header("session-id", "client-session")
            .send()
            .await
            .unwrap()
            .into_websocket()
            .await
            .unwrap();
        let frame = String::from_utf8(fixture("ws-create-first").bytes().to_vec())
            .unwrap()
            .replacen(r#""model":"gpt-test""#, r#""model":"gpt-native""#, 1);
        socket
            .send(wreq::ws::message::Message::text(frame.clone()))
            .await
            .unwrap();
        let want: Vec<String> = fixture("sse-completed")
            .frames()
            .into_iter()
            .map(|f| String::from_utf8(f.to_vec()).unwrap())
            .collect();
        let mut got = Vec::new();
        while got.len() < want.len() {
            let next = tokio::time::timeout(Duration::from_secs(5), futures_util::StreamExt::next(&mut socket))
                .await
                .expect("an upstream frame");
            match next {
                Some(Ok(wreq::ws::message::Message::Text(text))) => got.push(text.as_str().to_owned()),
                other => panic!("websockets={websockets}: {other:?} after {got:?}"),
            }
        }
        assert_eq!(got, want, "websockets={websockets}: the upstream's frames");
        let (headers, sent) = if websockets {
            let sockets = fx.codex.sockets();
            assert_eq!(sockets.len(), 1, "the turn ran on an upstream socket");
            assert_eq!(sockets[0].frames, [frame.as_bytes()], "the client's frame");
            assert!(fx.codex.http().is_empty());
            (sockets[0].headers.clone(), None)
        } else {
            let http = fx.codex.http();
            assert_eq!(http.len(), 1, "the turn ran over HTTP");
            assert!(fx.codex.sockets().is_empty());
            (http[0].headers.clone(), Some(http[0].body.clone()))
        };
        if let Some(body) = sent {
            let untagged = cpa_common::codex_body::CodexBody::parse(frame.as_bytes())
                .unwrap()
                .without_member("type");
            assert_eq!(body.as_ref(), untagged.as_slice(), "the client's frame without its tag");
        }
        assert_eq!(headers.get("x-codex-turn-state"), None, "no before-auth header");
        assert_eq!(headers.get("x-after"), None, "no after-auth header");
        assert_ne!(
            headers.get("session-id").and_then(|v| v.to_str().ok()),
            Some("plugin-session"),
            "no before-auth session"
        );
        assert_eq!(fx.calls("request.intercept_before"), 1);
        assert_eq!(fx.calls("request.intercept_after"), 0);
        assert_eq!(fx.calls("response.intercept_stream_chunk"), 0);
        assert_eq!(fx.calls("response.intercept_after"), 0);
        drop(socket);
        assert!(fx.lifecycle_completions(1).await >= 1, "lifecycle completion");
    }
}

/// Plugin request normalizers and translators and response normalizers (the
/// `request.normalize`, `request.translate`, `response.translate`,
/// `response.normalize_before` and `response.normalize_after` hooks) cannot change a
/// Codex translated request or its answer (#27): a Claude request reaches the upstream as
/// the built-in conversion and the client gets the built-in translation of the upstream
/// answer, while none of those hooks is called. The plugin lifecycle still completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugin_translators_cannot_rewrite_codex_translated_traffic() {
    let Some(fx) = Fixture::build(Setup {
        name: "codex-translated-plugins",
        providers: default_providers(&["key-FAKE-codex"]),
        routing: "",
        caps: "request_normalizer,request_translator,response_translator,response_before_translator,response_after_translator,request_lifecycle_plugin",
        extra: Box::new(|_| Vec::new()),
        rewriting_hooks: false,
    })
    .await
    else {
        return;
    };
    for method in [
        "request.normalize",
        "request.translate",
        "response.translate",
        "response.normalize_before",
        "response.normalize_after",
    ] {
        fx.respond(
            method,
            &json!({"ok": true, "result": {"Body": b64(r#"{"plugin":"rewrote this"}"#)}}),
        );
    }
    let claude = r#"{"model":"gpt-native","max_tokens":64,"messages":[{"role":"user","content":"hello"}]}"#;
    let (status, answer) = fx.post("/v1/messages", &[("x-api-key", "client-key-1")], claude).await;
    assert_eq!(status, 200, "{answer}");
    assert!(
        answer.contains("Done."),
        "the built-in translation of the answer: {answer}"
    );
    assert!(!answer.contains("rewrote this"), "{answer}");
    let sent = fx.codex.http().pop().expect("the Codex upstream got the request");
    let body = String::from_utf8_lossy(&sent.body);
    assert!(body.contains(r#""text":"hello""#), "{body}");
    assert!(!body.contains("rewrote this"), "{body}");
    for method in [
        "request.normalize",
        "request.translate",
        "response.translate",
        "response.normalize_before",
        "response.normalize_after",
    ] {
        assert_eq!(fx.calls(method), 0, "{method}");
    }
    assert!(fx.lifecycle_completions(1).await >= 1, "lifecycle completion");
}

/// One host callback from the plugin labelled `caller`, as the plugin makes it.
fn host_call(
    host: &cpa_plugin::Host,
    instance: &Arc<cpa_plugin::client::CallbackInstance>,
    method: &str,
    request: &Value,
) -> Value {
    let bytes =
        cpa_plugin::testing::call_from_plugin(host, "caller", instance, method, &serde_json::to_vec(request).unwrap())
            .unwrap();
    let envelope: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(envelope["ok"], true, "{method}: {envelope}");
    envelope["result"].clone()
}

/// #28 (review r4502650014): a plugin model callback whose entry and exit protocols are
/// both Responses, in either spelling (`openai-response` or `codex`), takes the native
/// route exactly as the executor decides it (`responses_formats`). For each of the four
/// pairs, buffered and streamed, the second plugin's hooks all try to rewrite: before-auth
/// headers, session, body and path; after-auth headers and body; the response and each
/// chunk. The Codex upstream gets the callback's own bytes on `/responses` with none of
/// those headers, and the callback gets the upstream's answer unchanged: the terminal
/// `response` object, or the event lines as sent. No after-auth, response or chunk hook
/// runs. An OpenAI-compatible model through the same callback keeps every hook.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn responses_callbacks_in_either_spelling_take_the_native_route() {
    // Its own scratch directory: the hook counts below read this test's records only.
    let Some(fx) = Fixture::start_with("codex-native-plugins-callbacks", &["key-FAKE-codex"], "").await else {
        return;
    };
    let host = fx.rt.plugins().clone();
    let instance = Arc::new(cpa_plugin::client::CallbackInstance::default());
    let guard = cpa_plugin::testing::callback_context(
        &host,
        "caller",
        instance.clone(),
        cpa_plugin::callbacks::RequestScope::default(),
    );
    let sse = String::from_utf8(fixture("sse-completed").bytes().to_vec()).unwrap();
    let lines: Vec<String> = sse
        .split(['\r', '\n'])
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    let completed = fixture("sse-completed").frames().last().unwrap().to_vec();
    let terminal = cpa_common::codex_body::CodexBody::parse(&completed).unwrap();
    let response_object = String::from_utf8(terminal.get("response").unwrap().raw.to_vec()).unwrap();
    let mut sent = 0;
    for entry in ["openai-response", "codex"] {
        for exit in ["openai-response", "codex"] {
            for stream in [false, true] {
                let name = format!("{entry} -> {exit} stream={stream}");
                // Spacing and member order that a re-serialization would change.
                let body = format!(
                    r#"{{ "model" : "gpt-native", "stream" : {stream}, "input" : [ {{"type":"message","role":"user","content":"callback {entry} {exit}"}} ] }}"#
                );
                fx.rewrite_before_auth(r#"{"model":"gpt-native","input":"before-auth body"}"#);
                let request = json!({"host_callback_id": guard.id(), "entry_protocol": entry, "exit_protocol": exit,
                    "model": "gpt-native", "stream": stream, "body": b64(&body),
                    "headers": {"Session-Id": ["callback-session"]}});
                let (h, i) = (host.clone(), instance.clone());
                let got = tokio::task::spawn_blocking(move || {
                    if !stream {
                        let answer = host_call(&h, &i, "host.model.execute", &request);
                        return vec![
                            String::from_utf8(
                                base64::engine::general_purpose::STANDARD
                                    .decode(answer["body"].as_str().unwrap())
                                    .unwrap(),
                            )
                            .unwrap(),
                        ];
                    }
                    let opened = host_call(&h, &i, "host.model.execute_stream", &request);
                    let read = json!({"stream_id": opened["stream_id"]});
                    let mut chunks = Vec::new();
                    loop {
                        let chunk = host_call(&h, &i, "host.model.stream_read", &read);
                        assert_eq!(chunk["error"], "", "{chunk}");
                        if chunk["done"] == true {
                            break;
                        }
                        chunks.push(
                            String::from_utf8(
                                base64::engine::general_purpose::STANDARD
                                    .decode(chunk["payload"].as_str().unwrap())
                                    .unwrap(),
                            )
                            .unwrap(),
                        );
                    }
                    chunks
                })
                .await
                .unwrap();
                if stream {
                    assert_eq!(got, lines, "{name}: the event lines as sent");
                } else {
                    assert_eq!(
                        got,
                        std::slice::from_ref(&response_object),
                        "{name}: the terminal response object"
                    );
                }
                sent += 1;
                let exchanges = fx.codex.http();
                assert_eq!(exchanges.len(), sent, "{name}: one upstream request");
                let upstream = exchanges.last().unwrap();
                assert_eq!(
                    String::from_utf8_lossy(&upstream.body),
                    body,
                    "{name}: the callback's bytes"
                );
                assert_eq!(upstream.uri.path(), "/responses", "{name}: no before-auth path");
                for header in ["x-after", "x-codex-turn-state"] {
                    assert!(
                        upstream.headers.get(header).is_none(),
                        "{name}: {header} reached the upstream"
                    );
                }
                for value in upstream.headers.get_all("session-id") {
                    assert!(
                        !matches!(value.to_str().unwrap(), "plugin-session" | "after-session"),
                        "{name}: a hook chose the session: {value:?}"
                    );
                }
            }
        }
    }
    assert_eq!(
        fx.calls("request.intercept_after"),
        0,
        "no after-auth hook on a native callback"
    );
    assert_eq!(fx.calls("response.intercept_after"), 0);
    assert_eq!(fx.calls("response.intercept_stream_chunk"), 0);

    // The control: an OpenAI-compatible model through the same callback keeps every hook.
    let request = json!({"host_callback_id": guard.id(), "entry_protocol": "openai", "model": "control-model",
        "body": b64(r#"{"model":"control-model","messages":[{"role":"user","content":"hi"}]}"#)});
    let (h, i) = (host.clone(), instance.clone());
    let answer = tokio::task::spawn_blocking(move || host_call(&h, &i, "host.model.execute", &request))
        .await
        .unwrap();
    let body = base64::engine::general_purpose::STANDARD
        .decode(answer["body"].as_str().unwrap())
        .unwrap();
    assert_eq!(body, br#"{"rewritten":"response"}"#, "the response hook ran");
    let control = fx.control.http().pop().expect("the control request");
    assert!(
        String::from_utf8_lossy(&control.body).contains("after-auth body"),
        "the after-auth hook ran"
    );
    assert_eq!(fx.calls("request.intercept_after"), 1);
    drop(guard);
}
