//! [`HistoryStore`] in an embedded SQLite file: the default backend, with nothing to
//! deploy. One connection behind a mutex, used from Tokio's blocking pool; WAL mode lets
//! the dashboard read while the recorder writes, and `synchronous=FULL` makes each commit
//! durable. The schema version is SQLite's
//! `user_version`, and [`MIGRATIONS`] brings an older file up to date on open.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::future::BoxFuture;
use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags, params, params_from_iter};

use crate::persistence::history::{
    Cursor, EventEntry, EventFilter, Group, HistoryStore, Level, MAX_PAGE, Page, Pruned, RequestEntry, RequestFilter,
    RowId, StoredEvent, StoredRequest, Totals, UnixMillis,
};

/// Each entry upgrades the schema from version `index` to `index + 1`.
const MIGRATIONS: &[&str] = &[
    // 1: requests and events. `AUTOINCREMENT` keeps ids increasing after the newest row
    // is pruned, as the contract requires.
    "
    CREATE TABLE requests (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        at_ms INTEGER NOT NULL,
        request_id TEXT NOT NULL,
        execution_id TEXT NOT NULL,
        provider TEXT NOT NULL,
        model TEXT NOT NULL,
        response_model TEXT NOT NULL,
        endpoint TEXT NOT NULL,
        auth_index TEXT NOT NULL,
        account TEXT NOT NULL,
        user_agent TEXT NOT NULL,
        stream INTEGER NOT NULL,
        failed INTEGER NOT NULL,
        status INTEGER NOT NULL,
        fail_body TEXT NOT NULL,
        usage_reported INTEGER NOT NULL,
        input_tokens INTEGER NOT NULL,
        output_tokens INTEGER NOT NULL,
        reasoning_tokens INTEGER NOT NULL,
        cache_read_tokens INTEGER NOT NULL,
        cache_creation_tokens INTEGER NOT NULL,
        total_tokens INTEGER NOT NULL,
        latency_ms INTEGER NOT NULL,
        ttft_ms INTEGER NOT NULL,
        timings TEXT NOT NULL,
        session_id TEXT NOT NULL,
        reasoning_effort TEXT NOT NULL,
        service_tier TEXT NOT NULL
    );
    CREATE INDEX requests_at ON requests (at_ms);
    CREATE INDEX requests_request_id ON requests (request_id);
    CREATE TABLE events (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        at_ms INTEGER NOT NULL,
        level INTEGER NOT NULL,
        request_id TEXT NOT NULL,
        line TEXT NOT NULL
    );
    CREATE INDEX events_at ON events (at_ms);
    CREATE INDEX events_request_id ON events (request_id);
    ",
];

/// The `requests` columns an insert writes, in its parameter order.
const REQUEST_INSERT_COLUMNS: [&str; 27] = [
    "at_ms",
    "request_id",
    "execution_id",
    "provider",
    "model",
    "response_model",
    "endpoint",
    "auth_index",
    "account",
    "user_agent",
    "stream",
    "failed",
    "status",
    "fail_body",
    "usage_reported",
    "input_tokens",
    "output_tokens",
    "reasoning_tokens",
    "cache_read_tokens",
    "cache_creation_tokens",
    "total_tokens",
    "latency_ms",
    "ttft_ms",
    "timings",
    "session_id",
    "reasoning_effort",
    "service_tier",
];

pub struct SqliteHistory {
    path: PathBuf,
    conn: Arc<Mutex<Connection>>,
}

impl SqliteHistory {
    /// Opens (creating when missing) the database at `path` and migrates it. The file
    /// and its directory are private to the server's user.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("open history database {}", path.display()))?;
        restrict(path);
        Self::init(conn, path.to_path_buf())
    }

    /// A private in-memory database, for tests.
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?, PathBuf::from(":memory:"))
    }

    fn init(mut conn: Connection, path: PathBuf) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // FULL syncs the WAL on every commit, so an append that returned Ok survives a power
        // loss, as the contract promises; NORMAL could lose the last commits. The recorder
        // commits a batch at most every 250 ms, so the sync cost stays small.
        conn.pragma_update(None, "synchronous", "FULL")?;
        migrate(&mut conn).with_context(|| format!("migrate history database {}", path.display()))?;
        Ok(Self {
            path,
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> BoxFuture<'_, Result<T>> {
        let conn = self.conn.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || work(&mut conn.lock().unwrap_or_else(PoisonError::into_inner)))
                .await
                .map_err(|e| anyhow!("history task failed: {e}"))?
        })
    }
}

/// Owner-only permissions on the database file; best effort, as the directory already
/// limits access on a normal install.
fn restrict(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let Ok(start) = usize::try_from(version) else {
        bail!("schema version {version} is invalid");
    };
    if start > MIGRATIONS.len() {
        bail!(
            "schema version {version} is newer than this server supports ({}); use a newer cliproxy",
            MIGRATIONS.len()
        );
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(start) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", (index + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
}

/// A WHERE clause and its parameters, built from the filter field by field.
#[derive(Default)]
struct Where {
    clauses: Vec<&'static str>,
    params: Vec<Value>,
}

impl Where {
    fn push(&mut self, clause: &'static str, value: impl Into<Value>) {
        self.clauses.push(clause);
        self.params.push(value.into());
    }

    fn text(&mut self, clause: &'static str, value: Option<String>) {
        if let Some(v) = value {
            self.push(clause, v);
        }
    }

    fn cursor(&mut self, cursor: Cursor) -> &'static str {
        match cursor {
            Cursor::Newest => "ORDER BY id DESC",
            Cursor::Before(RowId(id)) => {
                self.push("id < ?", id);
                "ORDER BY id DESC"
            }
            Cursor::After(RowId(id)) => {
                self.push("id > ?", id);
                "ORDER BY id ASC"
            }
        }
    }

    fn sql(&self) -> String {
        if self.clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", self.clauses.join(" AND "))
        }
    }
}

fn request_where(f: RequestFilter) -> Where {
    let mut w = Where::default();
    if let Some(t) = f.since {
        w.push("at_ms >= ?", t.0);
    }
    if let Some(t) = f.until {
        w.push("at_ms < ?", t.0);
    }
    w.text("provider = ?", f.provider);
    w.text("auth_index = ?", f.auth_index);
    w.text("model = ?", f.model);
    w.text("request_id = ?", f.request_id);
    if let Some(failed) = f.failed {
        w.push("failed = ?", i64::from(failed));
    }
    w
}

fn event_where(f: EventFilter) -> Where {
    let mut w = Where::default();
    if let Some(t) = f.since {
        w.push("at_ms >= ?", t.0);
    }
    if let Some(t) = f.until {
        w.push("at_ms < ?", t.0);
    }
    if let Some(level) = f.min_level {
        w.push("level >= ?", i64::from(level.code()));
    }
    w.text("request_id = ?", f.request_id);
    w
}

/// Reads one more row than the page holds to learn whether more match.
fn paged<T>(
    conn: &Connection,
    sql: &str,
    params: Vec<Value>,
    limit: usize,
    read: impl Fn(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> Result<Page<T>> {
    let limit = limit.min(MAX_PAGE);
    let mut stmt = conn.prepare(&format!("{sql} LIMIT {}", limit + 1))?;
    let mut rows = stmt
        .query_map(params_from_iter(params), read)?
        .collect::<rusqlite::Result<Vec<T>>>()?;
    let more = rows.len() > limit;
    rows.truncate(limit);
    Ok(Page { rows, more })
}

fn read_request(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredRequest> {
    let status: i64 = row.get("status")?;
    let timings: String = row.get("timings")?;
    Ok(StoredRequest {
        id: RowId(row.get("id")?),
        entry: RequestEntry {
            at: UnixMillis(row.get("at_ms")?),
            request_id: row.get("request_id")?,
            execution_id: row.get("execution_id")?,
            provider: row.get("provider")?,
            model: row.get("model")?,
            response_model: row.get("response_model")?,
            endpoint: row.get("endpoint")?,
            auth_index: row.get("auth_index")?,
            account: row.get("account")?,
            user_agent: row.get("user_agent")?,
            stream: row.get("stream")?,
            failed: row.get("failed")?,
            status: u16::try_from(status).unwrap_or(0),
            fail_body: row.get("fail_body")?,
            usage_reported: row.get("usage_reported")?,
            input_tokens: row.get("input_tokens")?,
            output_tokens: row.get("output_tokens")?,
            reasoning_tokens: row.get("reasoning_tokens")?,
            cache_read_tokens: row.get("cache_read_tokens")?,
            cache_creation_tokens: row.get("cache_creation_tokens")?,
            total_tokens: row.get("total_tokens")?,
            latency_ms: row.get("latency_ms")?,
            ttft_ms: row.get("ttft_ms")?,
            // Written by this module from a map; unreadable text reads as no timings.
            timings: serde_json::from_str(&timings).unwrap_or_default(),
            session_id: row.get("session_id")?,
            reasoning_effort: row.get("reasoning_effort")?,
            service_tier: row.get("service_tier")?,
        },
    })
}

fn read_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredEvent> {
    let level: i64 = row.get(2)?;
    Ok(StoredEvent {
        id: RowId(row.get(0)?),
        entry: EventEntry {
            at: UnixMillis(row.get(1)?),
            // A level this build does not know reads as the most severe.
            level: Level::from_code(level).unwrap_or(Level::Error),
            request_id: row.get(3)?,
            line: row.get(4)?,
        },
    })
}

impl HistoryStore for SqliteHistory {
    fn backend(&self) -> &'static str {
        "sqlite"
    }

    fn append_requests(&self, batch: Vec<RequestEntry>) -> BoxFuture<'_, Result<()>> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            {
                let sql = format!(
                    "INSERT INTO requests ({}) VALUES ({})",
                    REQUEST_INSERT_COLUMNS.join(", "),
                    vec!["?"; REQUEST_INSERT_COLUMNS.len()].join(", ")
                );
                let mut stmt = tx.prepare_cached(&sql)?;
                for e in &batch {
                    let timings = serde_json::to_string(&e.timings)?;
                    stmt.execute(params![
                        e.at.0,
                        e.request_id,
                        e.execution_id,
                        e.provider,
                        e.model,
                        e.response_model,
                        e.endpoint,
                        e.auth_index,
                        e.account,
                        e.user_agent,
                        e.stream,
                        e.failed,
                        e.status,
                        e.fail_body,
                        e.usage_reported,
                        e.input_tokens,
                        e.output_tokens,
                        e.reasoning_tokens,
                        e.cache_read_tokens,
                        e.cache_creation_tokens,
                        e.total_tokens,
                        e.latency_ms,
                        e.ttft_ms,
                        timings,
                        e.session_id,
                        e.reasoning_effort,
                        e.service_tier,
                    ])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
    }

    fn append_events(&self, batch: Vec<EventEntry>) -> BoxFuture<'_, Result<()>> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            {
                let mut stmt =
                    tx.prepare_cached("INSERT INTO events (at_ms, level, request_id, line) VALUES (?, ?, ?, ?)")?;
                for e in &batch {
                    stmt.execute(params![e.at.0, e.level.code(), e.request_id, e.line])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
    }

    fn requests(
        &self,
        filter: RequestFilter,
        cursor: Cursor,
        limit: usize,
    ) -> BoxFuture<'_, Result<Page<StoredRequest>>> {
        self.run(move |conn| {
            let mut w = request_where(filter);
            let order = w.cursor(cursor);
            let sql = format!("SELECT * FROM requests {} {order}", w.sql());
            paged(conn, &sql, w.params, limit, read_request)
        })
    }

    fn events(&self, filter: EventFilter, cursor: Cursor, limit: usize) -> BoxFuture<'_, Result<Page<StoredEvent>>> {
        self.run(move |conn| {
            let mut w = event_where(filter);
            let order = w.cursor(cursor);
            let sql = format!(
                "SELECT id, at_ms, level, request_id, line FROM events {} {order}",
                w.sql()
            );
            paged(conn, &sql, w.params, limit, read_event)
        })
    }

    fn totals(&self, filter: RequestFilter, bucket_ms: i64, group: Group) -> BoxFuture<'_, Result<Vec<Totals>>> {
        self.run(move |conn| {
            if bucket_ms <= 0 {
                bail!("bucket width must be positive");
            }
            let key = match group {
                Group::None => "''",
                Group::Provider => "provider",
                Group::Account => "auth_index",
                Group::Model => "model",
            };
            let w = request_where(filter);
            // Floor division toward negative infinity, matching `bucket_start`.
            let start = "((at_ms - ((at_ms % ?1) + ?1) % ?1) )";
            let sql = format!(
                "SELECT {start} AS start, {key} AS key, COUNT(*), SUM(failed), SUM(input_tokens), \
                 SUM(output_tokens), SUM(reasoning_tokens), SUM(cache_read_tokens), SUM(cache_creation_tokens), \
                 SUM(total_tokens) FROM requests {} GROUP BY start, key ORDER BY start, key",
                renumber(&w.sql(), 2),
            );
            let mut params = vec![Value::Integer(bucket_ms)];
            params.extend(w.params);
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(params_from_iter(params), |row| {
                    Ok(Totals {
                        start: UnixMillis(row.get(0)?),
                        key: row.get(1)?,
                        requests: row.get(2)?,
                        failed: row.get(3)?,
                        input_tokens: row.get(4)?,
                        output_tokens: row.get(5)?,
                        reasoning_tokens: row.get(6)?,
                        cache_read_tokens: row.get(7)?,
                        cache_creation_tokens: row.get(8)?,
                        total_tokens: row.get(9)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    fn prune(&self, before: UnixMillis) -> BoxFuture<'_, Result<Pruned>> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            let requests = tx.execute("DELETE FROM requests WHERE at_ms < ?", [before.0])?;
            let events = tx.execute("DELETE FROM events WHERE at_ms < ?", [before.0])?;
            tx.commit()?;
            Ok(Pruned {
                requests: requests as u64,
                events: events as u64,
            })
        })
    }
}

/// Numbers the anonymous `?` placeholders in `sql` from `first`, so they can follow a
/// numbered parameter. The filter clauses hold no string literals, so every `?` is a
/// placeholder.
fn renumber(sql: &str, first: usize) -> String {
    let mut out = String::with_capacity(sql.len() + 8);
    let mut n = first;
    for c in sql.chars() {
        if c == '?' {
            out.push_str(&format!("?{n}"));
            n += 1;
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::history::conformance;

    #[test]
    fn commits_are_synced_to_disk() {
        let store = SqliteHistory::open_in_memory().unwrap();
        let level: i64 = store
            .conn
            .lock()
            .unwrap()
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .unwrap();
        assert_eq!(level, 2, "synchronous=FULL");
    }

    #[tokio::test]
    async fn sqlite_history_meets_the_contract() {
        conformance::run(&SqliteHistory::open_in_memory().unwrap()).await;
    }

    fn temp_db(name: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!("cpa-sqlite-{}-{n}", std::process::id()))
            .join(name)
    }

    #[tokio::test]
    async fn a_file_database_keeps_rows_across_reopen_and_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_db("history.db");
        {
            let store = SqliteHistory::open(&path).unwrap();
            store
                .append_events(vec![EventEntry {
                    at: UnixMillis(5),
                    level: Level::Warn,
                    request_id: String::new(),
                    line: "kept".into(),
                }])
                .await
                .unwrap();
        }
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let store = SqliteHistory::open(&path).unwrap();
        let page = store.events(EventFilter::default(), Cursor::Newest, 10).await.unwrap();
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0].entry.line, "kept");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_database_from_a_newer_server_is_refused() {
        let path = temp_db("newer.db");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", 99).unwrap();
        }
        let error = SqliteHistory::open(&path).err().expect("refused");
        assert!(
            format!("{error:#}").contains("newer than this server supports"),
            "{error:#}"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn placeholders_are_renumbered_in_order() {
        assert_eq!(renumber("WHERE a = ? AND b < ?", 2), "WHERE a = ?2 AND b < ?3");
        assert_eq!(renumber("", 2), "");
    }
}
