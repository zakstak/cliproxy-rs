use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use ledger_model::{Activity, ActivityPage, ConfigSnapshot, Outcome, validate_yaml};
use serde::Deserialize;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Sandbox {
    config: ConfigSnapshot,
    cursor: u64,
    fail_next: bool,
}

type Shared = Arc<Mutex<Sandbox>>;

fn app(state: Shared) -> Router {
    Router::new()
        .route("/", get(index))
        .route(
            "/boot.js",
            get(|| async { asset("text/javascript", include_bytes!("../../dist/boot.js")) }),
        )
        .route(
            "/app.css",
            get(|| async { asset("text/css", include_bytes!("../../dist/app.css")) }),
        )
        .route(
            "/app.js",
            get(|| async { asset("text/javascript", include_bytes!("../../dist/app.js")) }),
        )
        .route(
            "/app_bg.wasm",
            get(|| async { asset("application/wasm", include_bytes!("../../dist/app_bg.wasm")) }),
        )
        .route(
            "/host-grotesk.woff2",
            get(|| async { asset("font/woff2", include_bytes!("../../../public/fonts/host-grotesk.woff2")) }),
        )
        .route("/sandbox/activity", get(activity))
        .route("/sandbox/activity/fail-next", post(fail_next))
        .route("/sandbox/config", get(config).put(save_config))
        .route("/sandbox/config/competing-write", post(competing_write))
        .layer(DefaultBodyLimit::max(256 * 1024))
        .with_state(state)
}

fn asset(mime: &'static str, bytes: &'static [u8]) -> Response {
    (
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        bytes,
    )
        .into_response()
}

async fn index() -> Response {
    let mut response = asset("text/html; charset=utf-8", include_bytes!("../../dist/index.html"));
    response.headers_mut().insert(header::CONTENT_SECURITY_POLICY,
        "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; font-src 'self'; connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; form-action 'none'".parse().expect("static CSP"));
    response
}

#[derive(Deserialize)]
struct Cursor {
    after: Option<u64>,
}

async fn activity(State(state): State<Shared>, Query(query): Query<Cursor>) -> Response {
    let mut state = state.lock().expect("sandbox mutex");
    if std::mem::take(&mut state.fail_next) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "The sandbox feed is temporarily unavailable.",
        )
            .into_response();
    }
    state.cursor = state.cursor.saturating_add(1);
    let cursor = state.cursor;
    // ponytail: at most 40 synthetic rows per read; a production port uses the
    // existing /observability/usage/live cursor contract instead of this fixture.
    let oldest = cursor.saturating_sub(39);
    let after = query.after.unwrap_or(0);
    let gap = after > 0 && after < oldest;
    let start = if after == 0 {
        oldest
    } else {
        after.saturating_add(1).max(oldest)
    };
    let records = (start..=cursor).map(sample).collect();
    Json(ActivityPage { cursor, records, gap }).into_response()
}

fn sample(id: u64) -> Activity {
    let (client, provider, model) = match id % 4 {
        0 => ("Amp", "Claude", "claude-opus-5-5"),
        1 => ("Codex CLI", "Codex", "gpt-6.1-sol"),
        2 => ("OpenCode", "Grok", "grok-4"),
        _ => ("API client", "Muse", "muse-ember-1.0"),
    };
    let outcome = match id % 13 {
        0 => Outcome::Failed,
        5 => Outcome::Limited,
        _ => Outcome::Completed,
    };
    Activity {
        id,
        time: format!("14:{:02}:{:02}", (id * 3 / 60) % 60, id * 3 % 60),
        client: client.into(),
        provider: provider.into(),
        model: model.into(),
        outcome,
        duration_ms: 420 + (id % 12) as u32 * 137,
        input_tokens: 1200 + (id % 20) as u32 * 640,
        output_tokens: 180 + (id % 9) as u32 * 80,
        cache_read_tokens: if id.is_multiple_of(4) { 8200 } else { 0 },
    }
}

async fn fail_next(State(state): State<Shared>) -> StatusCode {
    state.lock().expect("sandbox mutex").fail_next = true;
    StatusCode::NO_CONTENT
}

fn snapshot_response(snapshot: ConfigSnapshot) -> Response {
    let etag = snapshot.etag();
    ([(header::ETAG, etag)], Json(snapshot)).into_response()
}

async fn config(State(state): State<Shared>) -> Response {
    snapshot_response(state.lock().expect("sandbox mutex").config.clone())
}

async fn save_config(State(state): State<Shared>, headers: HeaderMap, body: String) -> Response {
    let Some(expected) = headers.get(header::IF_MATCH).and_then(|value| value.to_str().ok()) else {
        return (StatusCode::PRECONDITION_REQUIRED, "A reviewed version is required.").into_response();
    };
    if let Err(error) = validate_yaml(&body) {
        return (StatusCode::BAD_REQUEST, error).into_response();
    }
    let mut state = state.lock().expect("sandbox mutex");
    if expected != state.config.etag() {
        let mut response = snapshot_response(state.config.clone());
        *response.status_mut() = StatusCode::PRECONDITION_FAILED;
        return response;
    }
    state.config.version += 1;
    state.config.yaml = body;
    snapshot_response(state.config.clone())
}

async fn competing_write(State(state): State<Shared>) -> Response {
    let mut state = state.lock().expect("sandbox mutex");
    state.config.version += 1;
    state.config.yaml = format!(
        "{}# Another operator saved revision {}.\n",
        state.config.yaml, state.config.version
    );
    snapshot_response(state.config.clone())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port = std::env::args()
        .nth(1)
        .map(|value| value.parse::<u16>())
        .transpose()?
        .unwrap_or(8847);
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    println!(
        "Leptos sandbox: http://{} (in-memory fixtures only)",
        listener.local_addr()?
    );
    let state = Sandbox {
        cursor: 24,
        ..Sandbox::default()
    };
    axum::serve(listener, app(Arc::new(Mutex::new(state))))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    #[tokio::test]
    async fn stale_review_cannot_overwrite_another_writer() {
        let state = Arc::new(Mutex::new(Sandbox::default()));
        let app = app(state.clone());
        let competing = app
            .clone()
            .oneshot(
                Request::post("/sandbox/config/competing-write")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(competing.status(), StatusCode::OK);
        let before = state.lock().unwrap().config.clone();
        let stale = app
            .oneshot(
                Request::put("/sandbox/config")
                    .header(header::IF_MATCH, "\"sandbox-1\"")
                    .body(Body::from("routing: {strategy: round-robin}\n"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(stale.status(), StatusCode::PRECONDITION_FAILED);
        assert_eq!(state.lock().unwrap().config, before);
    }

    #[tokio::test]
    async fn save_preserves_exact_bytes_and_rejects_missing_version_or_invalid_yaml() {
        let state = Arc::new(Mutex::new(Sandbox::default()));
        let app = app(state.clone());
        let yaml = "# retained comment\nrouting: {strategy: round-robin}\n";
        let missing = app
            .clone()
            .oneshot(Request::put("/sandbox/config").body(Body::from(yaml)).unwrap())
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::PRECONDITION_REQUIRED);
        let invalid = app
            .clone()
            .oneshot(
                Request::put("/sandbox/config")
                    .header(header::IF_MATCH, "\"sandbox-1\"")
                    .body(Body::from("routing: ["))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        assert_eq!(state.lock().unwrap().config.version, 1);
        let saved = app
            .oneshot(
                Request::put("/sandbox/config")
                    .header(header::IF_MATCH, "\"sandbox-1\"")
                    .body(Body::from(yaml))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(saved.status(), StatusCode::OK);
        assert_eq!(saved.headers()[header::ETAG], "\"sandbox-2\"");
        assert_eq!(state.lock().unwrap().config.yaml, yaml);
    }

    #[tokio::test]
    async fn cursors_resume_without_duplicates_and_report_retention_gaps() {
        let state = Arc::new(Mutex::new(Sandbox {
            cursor: 100,
            ..Sandbox::default()
        }));
        let app = app(state);
        let response = app
            .clone()
            .oneshot(Request::get("/sandbox/activity?after=99").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), 65536).await.unwrap();
        let page: ActivityPage = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(page.records.iter().map(|row| row.id).collect::<Vec<_>>(), [100, 101]);
        assert!(!page.gap);
        let response = app
            .oneshot(Request::get("/sandbox/activity?after=1").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), 65536).await.unwrap();
        let page: ActivityPage = serde_json::from_slice(&bytes).unwrap();
        assert!(page.gap);
        assert_eq!(page.records.len(), 40);
    }
}
