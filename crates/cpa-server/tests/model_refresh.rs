//! Issue #98: on-demand model catalog refresh through the v8 management routes (Go
//! CLIProxyAPI `10417e5b`, after the v8.0.35 reference), and bounded cooldowns that end
//! without a restart. Its own test binary: the refreshes publish into the process-wide
//! models.json and Codex client catalogs, so the tests run one at a time and restore the
//! embedded catalogs before they end. Every source is a loopback mock or a local file,
//! and every credential is fake with a private `auth-dir`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use cpa_common::codex_client::CLIENT_MODELS_JSON;
use cpa_core::credential::Credential;
use cpa_core::registry::{catalog_generation, pinned};
use cpa_server::Runtime;
use cpa_server::model_updater::{Outcome, Report};
use futures_util::poll;
use serde_json::{Value, json};

const EMBEDDED: &str = include_str!("../../cpa-core/src/registry/models.json");
const MANAGEMENT_KEY: &str = "fake-management-only";
const CLIENT_KEY: &str = "fake-client-key";
const ROUTES: [&str; 2] = [
    "/v8/management/routing/model-definitions/refresh",
    "/v8/management/models/refresh",
];

/// The tests share the process-wide catalogs.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A loopback catalog server: `/models.json` and `/codex.json`, each with a status, a
/// body and a hit counter. While `hold_general` is set, a models.json request waits
/// until [`Mock::release`]; it is counted once it waits, so a test that sees the count
/// can release it.
struct Mock {
    general: Mutex<(u16, Vec<u8>)>,
    codex: Mutex<(u16, Vec<u8>)>,
    general_hits: AtomicUsize,
    codex_hits: AtomicUsize,
    hold_general: AtomicBool,
    held: Mutex<Vec<tokio::sync::oneshot::Sender<()>>>,
}

impl Mock {
    fn hits(&self) -> (usize, usize) {
        (
            self.general_hits.load(Ordering::SeqCst),
            self.codex_hits.load(Ordering::SeqCst),
        )
    }

    fn set_general(&self, status: u16, body: impl Into<Vec<u8>>) {
        *self.general.lock().unwrap() = (status, body.into());
    }

    fn set_codex(&self, status: u16, body: impl Into<Vec<u8>>) {
        *self.codex.lock().unwrap() = (status, body.into());
    }

    /// Lets every held request go on (a request whose client went away is gone).
    fn release(&self) {
        for held in self.held.lock().unwrap().drain(..) {
            let _ = held.send(());
        }
    }
}

async fn serve_catalog(State(mock): State<Arc<Mock>>, UrlPath(name): UrlPath<String>) -> Response {
    let general = name == "models.json";
    if general {
        let held = mock.hold_general.load(Ordering::SeqCst).then(|| {
            let (tx, rx) = tokio::sync::oneshot::channel();
            mock.held.lock().unwrap().push(tx);
            rx
        });
        mock.general_hits.fetch_add(1, Ordering::SeqCst);
        if let Some(held) = held {
            let _ = held.await;
        }
    } else {
        mock.codex_hits.fetch_add(1, Ordering::SeqCst);
    }
    let (status, body) = if general {
        mock.general.lock().unwrap().clone()
    } else {
        mock.codex.lock().unwrap().clone()
    };
    (StatusCode::from_u16(status).unwrap(), body).into_response()
}

async fn mock() -> (Arc<Mock>, String) {
    let mock = Arc::new(Mock {
        general: Mutex::new((200, EMBEDDED.as_bytes().to_vec())),
        codex: Mutex::new((200, CLIENT_MODELS_JSON.as_bytes().to_vec())),
        general_hits: AtomicUsize::new(0),
        codex_hits: AtomicUsize::new(0),
        hold_general: AtomicBool::new(false),
        held: Mutex::new(Vec::new()),
    });
    let app = axum::Router::new()
        .route("/{name}", axum::routing::get(serve_catalog))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (mock, base)
}

/// One runtime with a private `auth-dir`, a management key, a client key and the given
/// catalog sources (empty: unset), serving the client and management routes on loopback.
struct Env {
    rt: Arc<Runtime>,
    addr: std::net::SocketAddr,
    dir: PathBuf,
}

impl Env {
    fn config(&self, sources: [&str; 2]) -> cpa_core::config::Config {
        config(&self.dir, sources)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn config(dir: &Path, [catalog, codex]: [&str; 2]) -> cpa_core::config::Config {
    let hash = bcrypt::hash(MANAGEMENT_KEY, 4).unwrap();
    let mut yaml = format!(
        "api-keys:\n  - {CLIENT_KEY}\nmanagement:\n  secret-key: '{hash}'\nauth-dir: '{}'\n",
        dir.join("auth").display()
    );
    if !catalog.is_empty() || !codex.is_empty() {
        yaml.push_str(&format!(
            "models:\n  catalog: '{catalog}'\n  codex-catalog: '{codex}'\n"
        ));
    }
    cpa_core::config::Config::parse(&yaml).unwrap()
}

fn credential(dir: &Path, name: &str) -> Credential {
    let auth = dir.join("auth");
    let meta = json!({"type": "claude", "access_token": "fake-token"});
    Credential::from_file(&auth, &auth.join(name), meta.as_object().unwrap().clone()).unwrap()
}

async fn start_env(name: &str, sources: [&str; 2], local_model: bool, credentials: &[&str]) -> Env {
    let dir = std::env::temp_dir().join(format!("model-refresh-{name}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(dir.join("auth")).unwrap();
    let config = config(&dir, sources);
    let credentials = credentials.iter().map(|n| credential(&dir, n)).collect();
    let executors = cpa_exec::Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:9").unwrap(),
        codex: Default::default(),
        meta: Default::default(),
        xai: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(config, credentials, executors));
    rt.set_local_model(local_model);
    let management = cpa_server::management::Management::new(rt.clone(), dir.join("config.yaml"));
    let app = cpa_server::app(rt.clone(), cpa_server::management::router(management));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    Env { rt, addr, dir }
}

async fn send(env: &Env, method: wreq::Method, path: &str, key: Option<&str>) -> (u16, Value, String) {
    let mut request = wreq::Client::new().request(method, format!("http://{}{path}", env.addr));
    if let Some(key) = key {
        request = request.bearer_auth(key);
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null), text)
}

async fn refresh(env: &Env, route: &str) -> (u16, Value, String) {
    send(env, wreq::Method::POST, route, Some(MANAGEMENT_KEY)).await
}

async fn until(what: &str, done: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// The embedded models.json with one model added to each named section (a copy of the
/// section's first model under a new ID).
fn with_models(extra: &[(&str, &str)]) -> Vec<u8> {
    let mut root: serde_json::Map<String, Value> = serde_json::from_str(EMBEDDED).unwrap();
    for (section, id) in extra {
        let models = root[*section].as_array_mut().unwrap();
        let mut model = models[0].as_object().unwrap().clone();
        model.insert("id".into(), (*id).into());
        models.push(model.into());
    }
    serde_json::to_vec(&root).unwrap()
}

fn static_ids(channel: &str) -> Vec<String> {
    cpa_core::registry::dynamic::static_definitions(channel)
        .unwrap_or_default()
        .iter()
        .map(|m| m.id.clone())
        .collect()
}

/// A valid Codex client catalog distinguishable by its bytes.
fn codex_variant(n: usize) -> Vec<u8> {
    [CLIENT_MODELS_JSON.as_bytes(), &vec![b' '; n]].concat()
}

/// Publishes the embedded catalogs again for the next test in this binary.
async fn restore(env: &Env, mock: &Mock) {
    mock.hold_general.store(false, Ordering::SeqCst);
    mock.release();
    mock.set_general(200, EMBEDDED);
    mock.set_codex(200, CLIENT_MODELS_JSON);
    let (status, body, _) = refresh(env, ROUTES[0]).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        cpa_common::codex_catalog::snapshot().0.as_slice(),
        CLIENT_MODELS_JSON.as_bytes()
    );
}

/// Both routes sit behind the management key; each reads both current sources, reports
/// the retained providers whose models changed (Claude, Codex, Grok and Muse) and the
/// Codex client catalog, answers 502 with the last valid catalog kept when one source
/// fails, and leaves the removed Devin catalog out.
#[tokio::test]
async fn refresh_routes_are_guarded_and_publish_every_retained_catalog() {
    let _serial = SERIAL.lock().await;
    let (mock, base) = mock().await;
    let (general_url, codex_url) = (format!("{base}/models.json"), format!("{base}/codex.json"));
    let env = start_env("routes", [&general_url, &codex_url], false, &["c.json"]).await;
    cpa_server::model_updater::start(&env.rt, false);
    until("the startup reads", || mock.hits() == (1, 1)).await;

    // No key, and a wrong key: refused before anything is read (three of the five
    // failures before a ban).
    for (route, key) in [(ROUTES[0], None), (ROUTES[1], Some("wrong-key")), (ROUTES[1], None)] {
        let (status, _, text) = send(&env, wreq::Method::POST, route, key).await;
        assert_eq!(status, 401, "{route} {key:?}: {text}");
    }
    assert_eq!(mock.hits(), (1, 1), "an unauthorized request read a catalog");

    // A model added to each retained provider's section.
    mock.set_general(
        200,
        with_models(&[
            ("claude", "claude-refresh-98"),
            ("codex-pro", "gpt-refresh-98"),
            ("xai", "grok-refresh-98"),
            ("meta", "muse-refresh-98"),
        ]),
    );
    let (status, body, text) = refresh(&env, ROUTES[0]).await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(
        body,
        json!({
            "catalog": general_url,
            "catalogs": {
                "catalog": {"source": general_url, "refreshed": true, "changed_providers": ["claude", "codex", "xai", "meta"]},
                "codex-catalog": {"source": codex_url, "refreshed": true},
            },
            "changed_providers": ["claude", "codex", "xai", "meta"],
            "codex_catalog": codex_url,
            "ok": true,
            "sources": {"catalog": general_url, "codex-catalog": codex_url},
            "status": "ok",
        })
    );
    // Go's member order: gin.H sorted, the status structs in declaration order.
    assert!(text.starts_with(r#"{"catalog":"#), "{text}");
    assert!(text.contains(r#"{"source":"#), "{text}");
    assert!(!text.contains("devin"), "{text}");
    assert_eq!(mock.hits(), (2, 2));
    for (channel, id) in [
        ("claude", "claude-refresh-98"),
        ("codex", "gpt-refresh-98"),
        ("xai", "grok-refresh-98"),
        ("meta", "muse-refresh-98"),
    ] {
        assert!(static_ids(channel).iter().any(|m| m == id), "{channel}: {id}");
    }
    let credential_id = credential(&env.dir, "c.json").id;
    assert!(
        env.rt
            .registry()
            .client_model(&credential_id, "claude-refresh-98")
            .is_some(),
        "the Claude credential registers the refreshed model"
    );

    // The other route; only Grok's section changed.
    mock.set_general(
        200,
        with_models(&[
            ("claude", "claude-refresh-98"),
            ("codex-pro", "gpt-refresh-98"),
            ("xai", "grok-refresh-98-b"),
            ("meta", "muse-refresh-98"),
        ]),
    );
    let (status, body, text) = refresh(&env, ROUTES[1]).await;
    assert_eq!((status, &body["changed_providers"]), (200, &json!(["xai"])), "{text}");
    // Unchanged content: refreshed, nothing changed.
    let (status, body, text) = refresh(&env, ROUTES[1]).await;
    assert_eq!((status, &body["changed_providers"]), (200, &json!([])), "{text}");
    assert_eq!(
        body["catalogs"]["catalog"],
        json!({"source": general_url, "refreshed": true})
    );

    // The Codex client catalog (Go reports no provider for it).
    let variant = codex_variant(3);
    mock.set_codex(200, variant.clone());
    let (status, body, text) = refresh(&env, ROUTES[0]).await;
    assert_eq!((status, &body["changed_providers"]), (200, &json!([])), "{text}");
    assert_eq!(cpa_common::codex_catalog::snapshot().0.as_slice(), variant.as_slice());

    // An invalid models.json: 502, the last valid catalog stays, the Codex catalog is
    // still read.
    let generation = catalog_generation();
    mock.set_general(200, r#"{"claude":[{"id":"a"},{"id":"a"}]}"#);
    let (status, body, text) = refresh(&env, ROUTES[0]).await;
    assert_eq!(status, 502, "{text}");
    assert_eq!(
        (&body["ok"], &body["error"], body.get("status")),
        (&json!(false), &json!("no valid model catalog source"), None)
    );
    assert_eq!(
        body["catalogs"]["catalog"],
        json!({"source": general_url, "refreshed": false, "error": "no valid model catalog source"})
    );
    assert_eq!(body["catalogs"]["codex-catalog"]["refreshed"], json!(true));
    assert_eq!(catalog_generation(), generation);
    assert!(static_ids("xai").iter().any(|m| m == "grok-refresh-98-b"));

    // A failing Codex source: 502 for that catalog only; its last valid catalog stays.
    mock.set_general(200, EMBEDDED);
    mock.set_codex(500, "");
    let (status, body, text) = refresh(&env, ROUTES[1]).await;
    assert_eq!(status, 502, "{text}");
    assert_eq!(body["catalogs"]["catalog"]["refreshed"], json!(true));
    assert_eq!(
        body["catalogs"]["codex-catalog"],
        json!({"source": codex_url, "refreshed": false, "error": "no valid model catalog source"})
    );
    assert_eq!(cpa_common::codex_catalog::snapshot().0.as_slice(), variant.as_slice());

    // A GET is still the `refresh` channel of the model definitions, as gin routes it.
    let (status, body, text) = send(&env, wreq::Method::GET, ROUTES[0], Some(MANAGEMENT_KEY)).await;
    assert_eq!(
        (status, body),
        (400, json!({"channel": "refresh", "error": "unknown channel"})),
        "{text}"
    );
    restore(&env, &mock).await;
}

fn catalog_outcome(report: &Report) -> &Outcome {
    &report.catalog.outcome
}

/// Concurrent requests share one read per source, which a caller going away does not
/// stop; a config commit that changes the source fences a read still running; and
/// replacing a credential during a read leaves no registration of the old one.
#[tokio::test]
async fn concurrent_refreshes_share_reads_and_replacements_fence_them() {
    let _serial = SERIAL.lock().await;
    let (mock, base) = mock().await;
    let (general_url, codex_url) = (format!("{base}/models.json"), format!("{base}/codex.json"));
    let env = start_env("fences", [&general_url, &codex_url], false, &["old.json"]).await;
    cpa_server::model_updater::start(&env.rt, false);
    until("the startup reads", || mock.hits() == (1, 1)).await;

    // Two callers, the first of which goes away while the read runs.
    mock.hold_general.store(true, Ordering::SeqCst);
    mock.set_general(200, with_models(&[("claude", "claude-shared-98")]));
    let mut first = Box::pin(cpa_server::model_updater::refresh_now(&env.rt));
    let mut second = Box::pin(cpa_server::model_updater::refresh_now(&env.rt));
    assert!(poll!(first.as_mut()).is_pending() && poll!(second.as_mut()).is_pending());
    until("the shared read", || mock.hits().0 == 2).await;
    drop(first);
    mock.release();
    let report = second.await;
    assert_eq!(
        catalog_outcome(&report),
        &Outcome::Refreshed {
            changed: vec!["claude".into()]
        }
    );
    assert_eq!(mock.hits(), (2, 2), "joined requests read again");
    assert!(pinned().lookup("claude-shared-98").is_some());

    // A commit switches models.json to a local file while a read of the mock runs.
    mock.set_general(200, with_models(&[("claude", "claude-stale-98")]));
    let mut stale = Box::pin(cpa_server::model_updater::refresh_now(&env.rt));
    assert!(poll!(stale.as_mut()).is_pending());
    until("the stale read", || mock.hits().0 == 3).await;
    let file = env.dir.join("models.json");
    std::fs::write(&file, with_models(&[("claude", "claude-current-98")])).unwrap();
    let file = file.to_str().unwrap().to_owned();
    assert!(env.rt.publish_config(env.config([&file, &codex_url])));
    until("the new source", || pinned().lookup("claude-current-98").is_some()).await;
    mock.release();
    let report = stale.await;
    assert_eq!(
        catalog_outcome(&report),
        &Outcome::Failed("model catalog source changed during refresh")
    );
    assert!(pinned().lookup("claude-stale-98").is_none(), "stale publication");
    assert!(pinned().lookup("claude-current-98").is_some());

    // Back to the mock (its first read is not held), then a credential replacement
    // while a read runs.
    mock.hold_general.store(false, Ordering::SeqCst);
    mock.set_general(200, EMBEDDED);
    assert!(env.rt.publish_config(env.config([&general_url, &codex_url])));
    until("the mock source", || pinned().lookup("claude-current-98").is_none()).await;
    let hits = mock.hits().0;
    mock.hold_general.store(true, Ordering::SeqCst);
    mock.set_general(200, with_models(&[("claude", "claude-auth-98")]));
    let mut during = Box::pin(cpa_server::model_updater::refresh_now(&env.rt));
    assert!(poll!(during.as_mut()).is_pending());
    until("the held read", || mock.hits().0 == hits + 1).await;
    let (old, new) = (credential(&env.dir, "old.json"), credential(&env.dir, "new.json"));
    let some_model = static_ids("claude")[0].clone();
    assert!(env.rt.registry().client_model(&old.id, &some_model).is_some());
    env.rt.store().reconcile(vec![new.clone()]);
    mock.release();
    let report = during.await;
    assert_eq!(
        catalog_outcome(&report),
        &Outcome::Refreshed {
            changed: vec!["claude".into()]
        }
    );
    let registry = env.rt.registry();
    assert!(registry.client_model(&new.id, "claude-auth-98").is_some());
    assert!(
        registry.client_model(&old.id, "claude-auth-98").is_none(),
        "old credential resurrected"
    );
    assert!(
        registry.client_model(&old.id, &some_model).is_none(),
        "old credential resurrected"
    );
    drop(registry);
    restore(&env, &mock).await;
}

/// The policies decide what a refresh reads: a test runtime's unset sources and
/// `--local-model`'s embedded catalogs read nothing, Home mode skips models.json, and an
/// explicit local file is read whether or not the updaters were started, keeping the
/// last valid catalog when it turns invalid.
#[tokio::test]
async fn policies_decide_what_an_on_demand_refresh_reads() {
    let _serial = SERIAL.lock().await;
    let skipped = |source: &str| json!({"source": source, "refreshed": false, "skipped": true});

    // Unset sources in a test runtime, started or not: `disabled`.
    for start in [false, true] {
        let env = start_env("unset", ["", ""], false, &[]).await;
        if start {
            cpa_server::model_updater::start(&env.rt, false);
        }
        let (status, body, text) = refresh(&env, ROUTES[1]).await;
        assert_eq!(status, 200, "{text}");
        assert_eq!(
            body["catalogs"],
            json!({"catalog": skipped("disabled"), "codex-catalog": skipped("disabled")})
        );
        assert_eq!(
            (&body["catalog"], &body["codex_catalog"]),
            (&json!("disabled"), &json!("disabled"))
        );
    }

    // `--local-model`: the embedded catalogs.
    let env = start_env("local", ["", ""], true, &[]).await;
    cpa_server::model_updater::start(&env.rt, false);
    let (status, body, text) = refresh(&env, ROUTES[0]).await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(
        body["catalogs"],
        json!({"catalog": skipped("embed"), "codex-catalog": skipped("embed")})
    );
    drop(env);

    // Home mode: models.json is disabled whatever its source; the Codex catalog is read.
    let (mock, base) = mock().await;
    let (general_url, codex_url) = (format!("{base}/models.json"), format!("{base}/codex.json"));
    let env = start_env("home", [&general_url, &codex_url], false, &[]).await;
    cpa_server::model_updater::start(&env.rt, true);
    until("the Home startup read", || mock.hits() == (0, 1)).await;
    let (status, body, text) = refresh(&env, ROUTES[0]).await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(body["catalogs"]["catalog"], skipped("disabled"));
    assert_eq!(body["catalogs"]["codex-catalog"]["refreshed"], json!(true));
    assert_eq!(mock.hits(), (0, 2), "Home mode read models.json");
    drop(env);

    // An explicit local file, started and not.
    for start in [false, true] {
        let dir = std::env::temp_dir().join(format!("model-refresh-file-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("models.json");
        let id = format!("claude-file-98-{start}");
        std::fs::write(&file, with_models(&[("claude", &id)])).unwrap();
        let path = file.to_str().unwrap().to_owned();
        let env = start_env("file", [&path, &codex_url], false, &[]).await;
        if start {
            cpa_server::model_updater::start(&env.rt, false);
            until("the startup read", || pinned().lookup(&id).is_some()).await;
        }
        let (status, body, text) = refresh(&env, ROUTES[1]).await;
        assert_eq!(status, 200, "{text}");
        assert_eq!(body["catalogs"]["catalog"]["source"], json!(path));
        assert!(pinned().lookup(&id).is_some());
        std::fs::write(&file, "invalid").unwrap();
        let (status, body, text) = refresh(&env, ROUTES[1]).await;
        assert_eq!(status, 502, "{text}");
        assert_eq!(
            body["catalogs"]["catalog"],
            json!({"source": path, "refreshed": false, "error": "no valid model catalog source"})
        );
        assert!(pinned().lookup(&id).is_some(), "the last valid catalog was dropped");
        std::fs::write(&file, EMBEDDED).unwrap();
        let (status, _, text) = refresh(&env, ROUTES[1]).await;
        assert_eq!(status, 200, "{text}");
        assert!(pinned().lookup(&id).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }
    assert_eq!(
        cpa_common::codex_catalog::snapshot().0.as_slice(),
        CLIENT_MODELS_JSON.as_bytes()
    );
}

/// Issue #98 (Go CLIProxyAPI `69ed202a`): a credential-wide quota whose stated reset
/// the trust bound cut hides the credential's other models only until the bound ends
/// (the failing model is quota cooling, which stays listed, as in Go); the listing shows
/// them again in the same process while the stated reset is still ahead.
/// Listings compare cooldown deadlines with the current time, so no timer or restart is
/// involved. Wall-clock bound of 2 seconds: the cooldown deadlines use `std::time::Instant`.
#[tokio::test]
async fn a_bounded_cooldown_relists_its_models_once_the_bound_ends() {
    use cpa_core::exec::{ExecError, FailureScope};
    use cpa_server::runtime::{Outcome, Selection};
    let _serial = SERIAL.lock().await;
    let env = start_env("cooldown", ["", ""], false, &["c.json"]).await;
    let id = credential(&env.dir, "c.json").id;
    let listed = || async {
        let (status, body, text) = send(&env, wreq::Method::GET, "/v1/models", Some(CLIENT_KEY)).await;
        assert_eq!(status, 200, "{text}");
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let before = listed().await;
    let (model, sibling) = match before.as_slice() {
        [model, sibling, ..] => (model.clone(), sibling.clone()),
        _ => panic!("the credential lists fewer than two models: {before:?}"),
    };

    let bound = Duration::from_secs(2);
    let policy = Arc::new(cpa_server::scheduler::Policy {
        max_trusted_cooldown: bound,
        ..Default::default()
    });
    let admit = |_: &Credential| Some(model.clone());
    let lease = env
        .rt
        .store()
        .select_with(Selection::new("claude", &model), policy, &admit)
        .unwrap();
    let mut error = ExecError::local(429, FailureScope::Credential, "rate limited");
    error.retry_after = Some(Duration::from_secs(3600));
    lease.complete(Outcome::Failure(error));
    let recorded = Instant::now();
    let during = listed().await;
    assert!(
        recorded.elapsed() < bound,
        "the hidden listing was not observed inside the bound"
    );
    assert!(
        !during.contains(&sibling),
        "a credential-wide quota hides the other models"
    );
    assert!(
        during.contains(&model),
        "the failing model is quota cooling and stays listed"
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    while !listed().await.contains(&sibling) {
        assert!(
            Instant::now() < deadline,
            "the model was not listed again after the bound"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(recorded.elapsed() >= bound, "listed again before the bound ended");
    let states = env.rt.store().cooldowns(&id);
    assert!(
        states
            .iter()
            .any(|s| s.recover_in.is_some_and(|r| r > Duration::from_secs(3000))),
        "the stated reset is still ahead: {states:?}"
    );
}
