//! A separate process gives catalog privacy checks a global tracing subscriber.
//! Thread-local capture can miss callsites first registered by parallel tests.

use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Neither source credentials nor an invalid redirect target may reach DEBUG logs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_reads_log_no_url() {
    use axum::http::{StatusCode, header};
    let capture = Capture::default();
    let writer = capture.clone();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || writer.clone())
            .finish(),
    )
    .unwrap();

    let target = "http://alice:SECRET@[invalid/catalog.json?token=SECRET";
    let app = axum::Router::new().fallback(move || async move { (StatusCode::FOUND, [(header::LOCATION, target)]) });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let dir = std::env::temp_dir().join(format!("model-refresh-privacy-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = cpa_core::config::Config::parse(&format!(
        "auth-dir: '{}'\nmodels:\n  catalog: 'http://bob:SECRET@{addr}/r?token=SECRET'\n",
        dir.display()
    ))
    .unwrap();
    let executors = cpa_exec::Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:9").unwrap(),
        codex: Default::default(),
        meta: Default::default(),
        xai: Default::default(),
    };
    let rt = cpa_server::testing::runtime(config, Vec::new(), executors);
    let report = cpa_server::model_updater::refresh_now(&rt).await;
    assert!(matches!(
        report.catalog.outcome,
        cpa_server::model_updater::Outcome::Failed(_)
    ));
    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("model catalog source failed"), "{logs}");
    assert!(logs.contains("model catalog refresh failed"), "{logs}");
    assert!(!logs.contains("SECRET"), "{logs}");
    server.abort();
    std::fs::remove_dir_all(dir).unwrap();
}
