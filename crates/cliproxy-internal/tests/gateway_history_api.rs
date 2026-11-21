//! The dashboard's history API (`/v8/management/history/*`) over the served management
//! router and a SQLite history store: management auth, paging, filters, totals, events,
//! parameter refusals, and the answers without a store.

use std::path::PathBuf;
use std::sync::Arc;

use cliproxy_internal::config::Config;
use cliproxy_internal::gateway::management::{self, Management};
use cliproxy_internal::persistence::history::{EventEntry, HistoryStore, Level, RequestEntry, UnixMillis};
use cliproxy_internal::providers::Executors;
use cliproxy_internal::providers::claude::ClaudeExecutor;
use cliproxy_internal::runtime::history::Recorder;
use cliproxy_internal::storage::SqliteHistory;
use serde_json::Value;

const KEY: &str = "fake-management-only";

struct Fixture {
    dir: PathBuf,
    base: String,
    rt: Arc<cliproxy_internal::runtime::Runtime>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn fixture(name: &str) -> Fixture {
    // A private auth-dir: without one, credential loading reads the real ~/.cli-proxy-api.
    let dir = std::env::temp_dir().join(format!("history-api-{name}-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("auth")).unwrap();
    let path = dir.join("config.yaml");
    let hash = bcrypt::hash(KEY, 4).unwrap();
    std::fs::write(
        &path,
        format!(
            "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\n",
            dir.join("auth").display()
        ),
    )
    .unwrap();
    let rt = Arc::new(cliproxy_internal::gateway::testing::runtime(
        Config::load(&path).unwrap(),
        vec![],
        Executors {
            claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            meta: Default::default(),
            xai: Default::default(),
        },
    ));
    let state = Management::new(rt.clone(), path);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v8/management", listener.local_addr().unwrap());
    let app = management::router(state);
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    Fixture { dir, base, rt }
}

async fn get(f: &Fixture, path: &str, key: Option<&str>) -> (u16, Value) {
    let mut r = wreq::Client::new().get(format!("{}{path}", f.base));
    if let Some(key) = key {
        r = r.bearer_auth(key);
    }
    let r = r.send().await.unwrap();
    let status = r.status().as_u16();
    let text = r.text().await.unwrap();
    (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

fn entry(at: i64, auth_index: &str, model: &str, failed: bool, input: i64) -> RequestEntry {
    RequestEntry {
        at: UnixMillis(at),
        request_id: format!("req-{at}"),
        provider: "claude".into(),
        model: model.into(),
        auth_index: auth_index.into(),
        failed,
        status: if failed { 529 } else { 200 },
        usage_reported: true,
        input_tokens: input,
        cache_read_tokens: input / 2,
        output_tokens: 1,
        total_tokens: input + 1,
        ..RequestEntry::default()
    }
}

#[tokio::test]
async fn history_routes_page_filter_and_total_the_store() {
    let f = fixture("routes").await;
    let store = Arc::new(SqliteHistory::open_in_memory().unwrap());
    let hour = 3_600_000;
    store
        .append_requests(vec![
            entry(hour, "a1", "opus", false, 100),
            entry(hour + 10, "a2", "opus", true, 0),
            entry(2 * hour + 5, "a1", "sonnet", false, 40),
        ])
        .await
        .unwrap();
    store
        .append_events(vec![
            EventEntry {
                at: UnixMillis(hour),
                level: Level::Info,
                request_id: String::new(),
                line: "[t] [--------] [info ] started".into(),
            },
            EventEntry {
                at: UnixMillis(hour + 1),
                level: Level::Warn,
                request_id: "req-1".into(),
                line: "[t] [req-1   ] [warn ] slow".into(),
            },
        ])
        .await
        .unwrap();
    // Retention 0: the fixture rows are dated 1970, and retention runs at start.
    f.rt.usage_queue().set_history(Recorder::start(store, 0));

    let (status, body) = get(&f, "/history/status", Some(KEY)).await;
    assert_eq!(status, 200);
    assert_eq!(body["enabled"], true);
    assert_eq!(body["backend"], "sqlite");
    assert_eq!(body["retention_days"], 0);

    // Newest first, paged by id in both directions.
    let (status, page) = get(&f, "/history/requests?limit=2", Some(KEY)).await;
    assert_eq!(status, 200, "{page}");
    let requests = page["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(page["more"], true);
    assert_eq!(requests[0]["model"], "sonnet");
    assert_eq!(requests[0]["at"], 2 * hour + 5);
    assert_eq!(requests[0]["timings"], serde_json::json!({}));
    let last = requests[1]["id"].as_i64().unwrap();
    let (_, older) = get(&f, &format!("/history/requests?before={last}"), Some(KEY)).await;
    assert_eq!(older["requests"].as_array().unwrap().len(), 1);
    assert_eq!(older["more"], false);
    let first = older["requests"][0]["id"].as_i64().unwrap();
    let (_, newer) = get(&f, &format!("/history/requests?after={first}"), Some(KEY)).await;
    let ids: Vec<_> = newer["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, [first + 1, first + 2], "after reads oldest first");

    // Filters.
    let (_, failed) = get(&f, "/history/requests?failed=true", Some(KEY)).await;
    assert_eq!(failed["requests"].as_array().unwrap().len(), 1);
    assert_eq!(failed["requests"][0]["status"], 529);
    let (_, a1) = get(
        &f,
        &format!("/history/requests?account=a1&since={}", hour + 1),
        Some(KEY),
    )
    .await;
    assert_eq!(a1["requests"].as_array().unwrap().len(), 1);
    assert_eq!(a1["requests"][0]["model"], "sonnet");

    // Totals per hour, and per model over the whole range.
    let range = format!("since={hour}&until={}", 3 * hour);
    let (status, totals) = get(&f, &format!("/history/totals?{range}"), Some(KEY)).await;
    assert_eq!(status, 200, "{totals}");
    assert_eq!(totals["bucket_ms"], hour);
    let rows = totals["totals"].as_array().unwrap();
    let got: Vec<_> = rows
        .iter()
        .map(|t| {
            (
                t["start"].as_i64().unwrap(),
                t["requests"].as_i64().unwrap(),
                t["failed"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(got, [(hour, 2, 1), (2 * hour, 1, 0)]);
    assert_eq!(rows[0]["input_tokens"], 100);
    assert_eq!(rows[0]["cache_read_tokens"], 50);
    let (_, by_model) = get(
        &f,
        &format!("/history/totals?{range}&bucket={}&group=model", 2 * hour),
        Some(KEY),
    )
    .await;
    assert_eq!(by_model["group"], "model");
    let keys: Vec<_> = by_model["totals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| (t["key"].as_str().unwrap().to_owned(), t["requests"].as_i64().unwrap()))
        .collect();
    assert_eq!(keys, [("opus".to_owned(), 2), ("sonnet".to_owned(), 1)]);

    // Events: newest first, by level.
    let (_, events) = get(&f, "/history/events", Some(KEY)).await;
    let lines: Vec<_> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["line"].as_str().unwrap())
        .collect();
    assert_eq!(lines, ["[t] [req-1   ] [warn ] slow", "[t] [--------] [info ] started"]);
    let (_, warn) = get(&f, "/history/events?level=warn", Some(KEY)).await;
    assert_eq!(warn["events"].as_array().unwrap().len(), 1);
    assert_eq!(warn["events"][0]["level"], "warn");
    assert_eq!(warn["events"][0]["request_id"], "req-1");

    // Refusals name the parameter.
    for (path, needle) in [
        ("/history/requests?limit=0", "limit"),
        ("/history/requests?limit=x", "limit"),
        ("/history/requests?before=1&after=2", "not both"),
        ("/history/requests?before=-1", "before"),
        ("/history/requests?failed=maybe", "failed"),
        ("/history/requests?since=yesterday", "since"),
        ("/history/events?level=loud", "level"),
        ("/history/totals?since=1", "required"),
        ("/history/totals?since=5&until=5", "after since"),
        ("/history/totals?since=0&until=100000000&bucket=1000", "buckets"),
        ("/history/totals?since=0&until=10&bucket=10", "bucket"),
        ("/history/totals?since=0&until=10&group=colour", "group"),
    ] {
        let (status, body) = get(&f, path, Some(KEY)).await;
        assert_eq!(status, 400, "{path}: {body}");
        assert!(body.to_string().contains(needle), "{path}: {body}");
    }
    // Management auth on every route (last: failed attempts lock the client out).
    for path in [
        "/history/status",
        "/history/requests",
        "/history/events",
        "/history/totals",
    ] {
        assert_eq!(get(&f, path, None).await.0, 401, "{path}");
    }
    // A wrong key is refused too (repeated wrong keys lock the client out with 403).
    let (status, _) = get(&f, "/history/requests", Some("wrong")).await;
    assert!(matches!(status, 401 | 403), "{status}");
}

#[tokio::test]
async fn without_a_store_status_says_so_and_data_routes_answer_503() {
    let f = fixture("off").await;
    let (status, body) = get(&f, "/history/status", Some(KEY)).await;
    assert_eq!((status, body), (200, serde_json::json!({"enabled": false})));
    for path in [
        "/history/requests",
        "/history/events",
        "/history/totals?since=0&until=3600000",
    ] {
        let (status, body) = get(&f, path, Some(KEY)).await;
        assert_eq!(status, 503, "{path}");
        assert!(body.to_string().contains("history store disabled"), "{path}: {body}");
    }
}
