//! Model catalog sources at startup and on config reload (Go
//! sdk/cliproxy/service_catalogs_test.go TestServiceCatalogStartupAndConfigReload, which
//! observes the Devin catalog; the Devin catalog went with that provider, issue #49, so
//! the Codex client catalog is observed instead). Its own test binary: the updaters
//! publish into the process-wide Codex client catalog.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cpa_common::codex_client::CLIENT_MODELS_JSON;

fn runtime(auth: &Path, sources: [&Path; 2]) -> Arc<cpa_server::Runtime> {
    let executors = cpa_exec::Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:9").unwrap(),
        codex: Default::default(),
        meta: Default::default(),
        xai: Default::default(),
    };
    let rt = cpa_server::testing::runtime(config(auth, sources), Vec::new(), executors);
    Arc::new(rt)
}

fn config(auth: &Path, [catalog, codex]: [&Path; 2]) -> cpa_core::config::Config {
    let yaml = format!(
        "auth-dir: '{}'\nmodels:\n  catalog: '{}'\n  codex-catalog: '{}'\n",
        auth.display(),
        catalog.display(),
        codex.display(),
    );
    cpa_core::config::Config::parse(&yaml).unwrap()
}

/// A valid Codex client catalog distinguishable by its bytes: the embedded one with `n`
/// trailing spaces.
fn variant(n: usize) -> Vec<u8> {
    [CLIENT_MODELS_JSON.as_bytes(), &vec![b' '; n]].concat()
}

async fn await_codex(want: &[u8]) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while cpa_common::codex_catalog::snapshot().0.as_slice() != want {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the configured Codex client catalog was not published"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The tests share the process-wide Codex client catalog.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn service_catalog_startup_and_config_reload() {
    let _serial = SERIAL.lock().await;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let general = root.join("cpa-core/src/registry/models.json");
    let dir = std::env::temp_dir().join(format!("catalog-sources-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let write = |name: &str, data: &[u8]| {
        let path = dir.join(name);
        std::fs::write(&path, data).unwrap();
        path
    };
    let (first, second) = (variant(1), variant(2));
    let first_path = write("first.json", &first);
    let auth = dir.join("auth");

    // Startup reads the configured source (a test runtime still reads explicit ones).
    let rt = runtime(&auth, [&general, &first_path]);
    cpa_server::model_updater::start(&rt, false);
    await_codex(&first).await;

    // A published config with another source switches to it.
    let second_path = write("second.json", &second);
    rt.publish_config(config(&auth, [&general, &second_path]));
    await_codex(&second).await;

    // The same source restarts under a new runtime once the old one is gone.
    drop(rt);
    std::fs::write(&second_path, &first).unwrap();
    let rt = runtime(&auth, [&general, &second_path]);
    cpa_server::model_updater::start(&rt, false);
    await_codex(&first).await;

    // Restore the embedded catalog for anything else in this process.
    rt.publish_config(config(
        &auth,
        [&general, &write("embedded.json", CLIENT_MODELS_JSON.as_bytes())],
    ));
    await_codex(CLIENT_MODELS_JSON.as_bytes()).await;
    let _ = std::fs::remove_dir_all(dir);
}

/// Each config commit reconfigures in commit order (Go `commitConfigUpdate`): switching
/// to B and back to A before anything else runs still reads A again, and a commit with
/// an invalid source is rejected whole.
#[tokio::test]
async fn commits_reconfigure_in_order_and_invalid_sources_are_rejected() {
    let _serial = SERIAL.lock().await;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let general = root.join("cpa-core/src/registry/models.json");
    let dir = std::env::temp_dir().join(format!("catalog-commits-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let (d0, d1, d2) = (variant(3), variant(4), variant(5));
    let (a, b) = (dir.join("a.json"), dir.join("b.json"));
    std::fs::write(&a, &d0).unwrap();
    std::fs::write(&b, &d2).unwrap();
    let auth = dir.join("auth");
    let rt = runtime(&auth, [&general, &a]);
    cpa_server::model_updater::start(&rt, false);
    await_codex(&d0).await;

    std::fs::write(&a, &d1).unwrap();
    rt.publish_config(config(&auth, [&general, &b]));
    rt.publish_config(config(&auth, [&general, &a]));
    await_codex(&d1).await;

    // A relative source fails Go's validation: the whole commit is rejected. Loaded
    // configs are validated already; this is a caller editing the document.
    let mut invalid = config(&auth, [&general, &b]);
    let models = invalid.document.get_mut("models").unwrap().as_mapping_mut().unwrap();
    models.insert("codex-catalog".into(), "relative.json".into());
    assert!(!rt.publish_config(invalid));
    assert_eq!(rt.config().catalog_sources()[1], a.display().to_string());

    // Restore the embedded catalog for the other test.
    let embedded = dir.join("embedded.json");
    std::fs::write(&embedded, CLIENT_MODELS_JSON.as_bytes()).unwrap();
    rt.publish_config(config(&auth, [&general, &embedded]));
    await_codex(CLIENT_MODELS_JSON.as_bytes()).await;
    let _ = std::fs::remove_dir_all(dir);
}
