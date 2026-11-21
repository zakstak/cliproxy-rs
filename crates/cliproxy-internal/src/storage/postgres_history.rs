//! [`HistoryStore`] in PostgreSQL (`storage.backend: postgres`). The tables are this
//! server's own (`cliproxy_history_requests`, `cliproxy_history_events`), versioned in
//! `cliproxy_history_schema`, in `storage.postgres-schema` (default `public`).
//!
//! One connection worker ([`Pg`]) runs statements one at a time, in order. Each batch is
//! one `INSERT ... SELECT FROM UNNEST(...)` statement, so it is all or none, and ids
//! follow append order.
// ponytail: one writer per database. Ids come from a sequence, and rows from two servers
// appending at once could commit out of id order, so an `after` reader could skip one. Give
// each server its own schema, or add a commit-ordered cursor, before sharing tables.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use futures_util::future::BoxFuture;
use tokio_postgres::Client;
use tokio_postgres::types::ToSql;

use crate::persistence::history::{
    Cursor, EventEntry, EventFilter, Group, HistoryStore, Level, MAX_PAGE, Page, Pruned, RequestEntry, RequestFilter,
    RowId, StoredEvent, StoredRequest, Totals, UnixMillis,
};
use crate::storage::pgconn::{Dsn, Pg, error_text};

const SCHEMA_VERSION: i32 = 1;
/// The request columns as [`read_request`] reads them (`timings` as text: the driver is
/// built without its JSON types).
const REQUEST_COLUMNS: &str = "id, at_ms, request_id, execution_id, provider, model, response_model, endpoint, \
    auth_index, account, user_agent, stream, failed, status, fail_body, usage_reported, input_tokens, output_tokens, \
    reasoning_tokens, cache_read_tokens, cache_creation_tokens, total_tokens, latency_ms, ttft_ms, \
    timings::text AS timings, session_id, reasoning_effort, service_tier";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub struct PostgresHistory {
    pg: Pg,
    requests: String,
    events: String,
}

type Param = Box<dyn ToSql + Sync + Send>;

fn quote(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn db(context: &str) -> impl Fn(tokio_postgres::Error) -> anyhow::Error + '_ {
    move |e| anyhow!("postgres history: {context}: {}", error_text(&e))
}

impl PostgresHistory {
    /// Connects, creates the schema and tables when missing and checks their version.
    /// `dsn` is a `postgres://` URL or a key/value string; `schema` may be empty.
    pub async fn connect(dsn: &str, schema: &str) -> Result<Self> {
        let dsn = dsn.trim();
        if dsn.is_empty() {
            bail!("storage.postgres-dsn is required for storage.backend: postgres");
        }
        let dsn = Dsn::parse(dsn, &|key| std::env::var(key).ok())?;
        let pg = Pg::start(dsn)?;
        let schema = schema.trim().to_owned();
        let table = |name: &str| {
            if schema.is_empty() {
                quote(name)
            } else {
                format!("{}.{}", quote(&schema), quote(name))
            }
        };
        let store = Self {
            pg,
            requests: table("cliproxy_history_requests"),
            events: table("cliproxy_history_events"),
        };
        let version_table = table("cliproxy_history_schema");
        let (requests, events) = (store.requests.clone(), store.events.clone());
        let setup = store.pg.run(move |client| async move {
            if !schema.is_empty() {
                client
                    .batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS {}", quote(&schema)))
                    .await
                    .map_err(db("create schema"))?;
            }
            client
                .batch_execute(&format!(
                    "CREATE TABLE IF NOT EXISTS {version_table} (version INTEGER NOT NULL)"
                ))
                .await
                .map_err(db("create schema version table"))?;
            let found: Option<i32> = client
                .query_opt(&format!("SELECT max(version) FROM {version_table}"), &[])
                .await
                .map_err(db("read schema version"))?
                .and_then(|row| row.get(0));
            match found {
                Some(v) if v > SCHEMA_VERSION => {
                    bail!("postgres history: schema version {v} is newer than this server supports ({SCHEMA_VERSION}); use a newer cliproxy")
                }
                Some(_) => return Ok(()),
                None => {}
            }
            client
                .batch_execute(&format!(
                    "
                    CREATE TABLE IF NOT EXISTS {requests} (
                        id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                        at_ms BIGINT NOT NULL,
                        request_id TEXT NOT NULL,
                        execution_id TEXT NOT NULL,
                        provider TEXT NOT NULL,
                        model TEXT NOT NULL,
                        response_model TEXT NOT NULL,
                        endpoint TEXT NOT NULL,
                        auth_index TEXT NOT NULL,
                        account TEXT NOT NULL,
                        user_agent TEXT NOT NULL,
                        stream BOOLEAN NOT NULL,
                        failed BOOLEAN NOT NULL,
                        status INTEGER NOT NULL,
                        fail_body TEXT NOT NULL,
                        usage_reported BOOLEAN NOT NULL,
                        input_tokens BIGINT NOT NULL,
                        output_tokens BIGINT NOT NULL,
                        reasoning_tokens BIGINT NOT NULL,
                        cache_read_tokens BIGINT NOT NULL,
                        cache_creation_tokens BIGINT NOT NULL,
                        total_tokens BIGINT NOT NULL,
                        latency_ms BIGINT NOT NULL,
                        ttft_ms BIGINT NOT NULL,
                        timings JSONB NOT NULL,
                        session_id TEXT NOT NULL,
                        reasoning_effort TEXT NOT NULL,
                        service_tier TEXT NOT NULL
                    );
                    CREATE INDEX IF NOT EXISTS cliproxy_history_requests_at ON {requests} (at_ms);
                    CREATE INDEX IF NOT EXISTS cliproxy_history_requests_request_id ON {requests} (request_id);
                    CREATE TABLE IF NOT EXISTS {events} (
                        id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                        at_ms BIGINT NOT NULL,
                        level SMALLINT NOT NULL,
                        request_id TEXT NOT NULL,
                        line TEXT NOT NULL
                    );
                    CREATE INDEX IF NOT EXISTS cliproxy_history_events_at ON {events} (at_ms);
                    CREATE INDEX IF NOT EXISTS cliproxy_history_events_request_id ON {events} (request_id);
                    INSERT INTO {version_table} (version) VALUES ({SCHEMA_VERSION});
                    "
                ))
                .await
                .map_err(db("create tables"))?;
            Ok(())
        });
        match tokio::time::timeout(CONNECT_TIMEOUT, setup).await {
            Ok(result) => result?,
            Err(_) => bail!("postgres history: connecting took longer than {CONNECT_TIMEOUT:?}"),
        }
        Ok(store)
    }

    fn run<T: Send + 'static, F, Fut>(&self, f: F) -> BoxFuture<'_, Result<T>>
    where
        F: FnOnce(Arc<Client>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        Box::pin(self.pg.run(f))
    }
}

/// A WHERE clause with numbered placeholders, built field by field.
#[derive(Default)]
struct Where {
    clauses: Vec<String>,
    params: Vec<Param>,
}

impl Where {
    fn push(&mut self, clause: &str, value: Param) {
        self.params.push(value);
        self.clauses
            .push(clause.replace('?', &format!("${}", self.params.len())));
    }

    fn text(&mut self, clause: &str, value: Option<String>) {
        if let Some(v) = value {
            self.push(clause, Box::new(v));
        }
    }

    fn cursor(&mut self, cursor: Cursor) -> &'static str {
        match cursor {
            Cursor::Newest => "ORDER BY id DESC",
            Cursor::Before(RowId(id)) => {
                self.push("id < ?", Box::new(id));
                "ORDER BY id DESC"
            }
            Cursor::After(RowId(id)) => {
                self.push("id > ?", Box::new(id));
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

    fn refs(&self) -> Vec<&(dyn ToSql + Sync)> {
        self.params.iter().map(|p| p.as_ref() as &(dyn ToSql + Sync)).collect()
    }
}

fn request_where(f: RequestFilter, w: &mut Where) {
    if let Some(t) = f.since {
        w.push("at_ms >= ?", Box::new(t.0));
    }
    if let Some(t) = f.until {
        w.push("at_ms < ?", Box::new(t.0));
    }
    w.text("provider = ?", f.provider);
    w.text("auth_index = ?", f.auth_index);
    w.text("model = ?", f.model);
    w.text("request_id = ?", f.request_id);
    if let Some(failed) = f.failed {
        w.push("failed = ?", Box::new(failed));
    }
}

fn read_request(row: &tokio_postgres::Row) -> Result<StoredRequest> {
    let status: i32 = row.try_get("status")?;
    let timings: String = row.try_get("timings")?;
    Ok(StoredRequest {
        id: RowId(row.try_get("id")?),
        entry: RequestEntry {
            at: UnixMillis(row.try_get("at_ms")?),
            request_id: row.try_get("request_id")?,
            execution_id: row.try_get("execution_id")?,
            provider: row.try_get("provider")?,
            model: row.try_get("model")?,
            response_model: row.try_get("response_model")?,
            endpoint: row.try_get("endpoint")?,
            auth_index: row.try_get("auth_index")?,
            account: row.try_get("account")?,
            user_agent: row.try_get("user_agent")?,
            stream: row.try_get("stream")?,
            failed: row.try_get("failed")?,
            status: u16::try_from(status).unwrap_or(0),
            fail_body: row.try_get("fail_body")?,
            usage_reported: row.try_get("usage_reported")?,
            input_tokens: row.try_get("input_tokens")?,
            output_tokens: row.try_get("output_tokens")?,
            reasoning_tokens: row.try_get("reasoning_tokens")?,
            cache_read_tokens: row.try_get("cache_read_tokens")?,
            cache_creation_tokens: row.try_get("cache_creation_tokens")?,
            total_tokens: row.try_get("total_tokens")?,
            latency_ms: row.try_get("latency_ms")?,
            ttft_ms: row.try_get("ttft_ms")?,
            // Written by this module from a map; unreadable text reads as no timings.
            timings: serde_json::from_str(&timings).unwrap_or_default(),
            session_id: row.try_get("session_id")?,
            reasoning_effort: row.try_get("reasoning_effort")?,
            service_tier: row.try_get("service_tier")?,
        },
    })
}

fn read_event(row: &tokio_postgres::Row) -> Result<StoredEvent> {
    let level: i16 = row.try_get("level")?;
    Ok(StoredEvent {
        id: RowId(row.try_get("id")?),
        entry: EventEntry {
            at: UnixMillis(row.try_get("at_ms")?),
            // A level this build does not know reads as the most severe.
            level: Level::from_code(level.into()).unwrap_or(Level::Error),
            request_id: row.try_get("request_id")?,
            line: row.try_get("line")?,
        },
    })
}

/// Runs a page query, reading one more row than the page holds to learn whether more match.
async fn paged<T>(
    client: &Client,
    sql: String,
    w: Where,
    limit: usize,
    read: impl Fn(&tokio_postgres::Row) -> Result<T>,
) -> Result<Page<T>> {
    let limit = limit.min(MAX_PAGE);
    let rows = client
        .query(&format!("{sql} LIMIT {}", limit + 1), &w.refs())
        .await
        .map_err(db("read"))?;
    let more = rows.len() > limit;
    let rows = rows.iter().take(limit).map(read).collect::<Result<Vec<_>>>()?;
    Ok(Page { rows, more })
}

impl HistoryStore for PostgresHistory {
    fn backend(&self) -> &'static str {
        "postgres"
    }

    fn append_requests(&self, batch: Vec<RequestEntry>) -> BoxFuture<'_, Result<()>> {
        // PostgreSQL text cannot hold U+0000.
        let batch: Vec<_> = batch.into_iter().map(RequestEntry::storable).collect();
        let table = self.requests.clone();
        self.run(move |client| async move {
            if batch.is_empty() {
                return Ok(());
            }
            macro_rules! column {
                ($field:ident) => {
                    batch.iter().map(|e| e.$field.clone()).collect::<Vec<_>>()
                };
            }
            let at: Vec<i64> = batch.iter().map(|e| e.at.0).collect();
            let status: Vec<i32> = batch.iter().map(|e| i32::from(e.status)).collect();
            let timings = batch
                .iter()
                .map(|e| serde_json::to_string(&e.timings))
                .collect::<serde_json::Result<Vec<_>>>()?;
            let sql = format!(
                "INSERT INTO {table} (at_ms, request_id, execution_id, provider, model, response_model, endpoint, \
                 auth_index, account, user_agent, stream, failed, status, fail_body, usage_reported, input_tokens, \
                 output_tokens, reasoning_tokens, cache_read_tokens, cache_creation_tokens, total_tokens, latency_ms, \
                 ttft_ms, timings, session_id, reasoning_effort, service_tier) \
                 SELECT a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r, s, t, u, v, w, x::jsonb, y, z, aa \
                 FROM UNNEST($1::bigint[], $2::text[], $3::text[], $4::text[], $5::text[], $6::text[], $7::text[], \
                 $8::text[], $9::text[], $10::text[], $11::boolean[], $12::boolean[], $13::integer[], $14::text[], \
                 $15::boolean[], $16::bigint[], $17::bigint[], $18::bigint[], $19::bigint[], $20::bigint[], \
                 $21::bigint[], $22::bigint[], $23::bigint[], $24::text[], $25::text[], $26::text[], $27::text[]) \
                 WITH ORDINALITY AS u(a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r, s, t, u, v, w, x, y, z, aa, ord) \
                 ORDER BY ord"
            );
            client
                .execute(
                    &sql,
                    &[
                        &at,
                        &column!(request_id),
                        &column!(execution_id),
                        &column!(provider),
                        &column!(model),
                        &column!(response_model),
                        &column!(endpoint),
                        &column!(auth_index),
                        &column!(account),
                        &column!(user_agent),
                        &column!(stream),
                        &column!(failed),
                        &status,
                        &column!(fail_body),
                        &column!(usage_reported),
                        &column!(input_tokens),
                        &column!(output_tokens),
                        &column!(reasoning_tokens),
                        &column!(cache_read_tokens),
                        &column!(cache_creation_tokens),
                        &column!(total_tokens),
                        &column!(latency_ms),
                        &column!(ttft_ms),
                        &timings,
                        &column!(session_id),
                        &column!(reasoning_effort),
                        &column!(service_tier),
                    ],
                )
                .await
                .map_err(db("append requests"))?;
            Ok(())
        })
    }

    fn append_events(&self, batch: Vec<EventEntry>) -> BoxFuture<'_, Result<()>> {
        let batch: Vec<_> = batch.into_iter().map(EventEntry::storable).collect();
        let table = self.events.clone();
        self.run(move |client| async move {
            if batch.is_empty() {
                return Ok(());
            }
            let at: Vec<i64> = batch.iter().map(|e| e.at.0).collect();
            let level: Vec<i16> = batch.iter().map(|e| e.level.code()).collect();
            let request_id: Vec<String> = batch.iter().map(|e| e.request_id.clone()).collect();
            let line: Vec<String> = batch.iter().map(|e| e.line.clone()).collect();
            client
                .execute(
                    &format!(
                        "INSERT INTO {table} (at_ms, level, request_id, line) SELECT a, b, c, d \
                         FROM UNNEST($1::bigint[], $2::smallint[], $3::text[], $4::text[]) \
                         WITH ORDINALITY AS u(a, b, c, d, ord) ORDER BY ord"
                    ),
                    &[&at, &level, &request_id, &line],
                )
                .await
                .map_err(db("append events"))?;
            Ok(())
        })
    }

    fn requests(
        &self,
        filter: RequestFilter,
        cursor: Cursor,
        limit: usize,
    ) -> BoxFuture<'_, Result<Page<StoredRequest>>> {
        let table = self.requests.clone();
        self.run(move |client| async move {
            let mut w = Where::default();
            request_where(filter, &mut w);
            let order = w.cursor(cursor);
            let sql = format!("SELECT {REQUEST_COLUMNS} FROM {table} {} {order}", w.sql());
            paged(&client, sql, w, limit, read_request).await
        })
    }

    fn events(&self, filter: EventFilter, cursor: Cursor, limit: usize) -> BoxFuture<'_, Result<Page<StoredEvent>>> {
        let table = self.events.clone();
        self.run(move |client| async move {
            let mut w = Where::default();
            if let Some(t) = filter.since {
                w.push("at_ms >= ?", Box::new(t.0));
            }
            if let Some(t) = filter.until {
                w.push("at_ms < ?", Box::new(t.0));
            }
            if let Some(level) = filter.min_level {
                w.push("level >= ?", Box::new(level.code()));
            }
            w.text("request_id = ?", filter.request_id);
            let order = w.cursor(cursor);
            let sql = format!(
                "SELECT id, at_ms, level, request_id, line FROM {table} {} {order}",
                w.sql()
            );
            paged(&client, sql, w, limit, read_event).await
        })
    }

    fn totals(&self, filter: RequestFilter, bucket_ms: i64, group: Group) -> BoxFuture<'_, Result<Vec<Totals>>> {
        let table = self.requests.clone();
        self.run(move |client| async move {
            if bucket_ms <= 0 {
                bail!("bucket width must be positive");
            }
            let key = match group {
                Group::None => "''::text",
                Group::Provider => "provider",
                Group::Account => "auth_index",
                Group::Model => "model",
            };
            let mut w = Where::default();
            w.params.push(Box::new(bucket_ms));
            request_where(filter, &mut w);
            // Floor division toward negative infinity, matching `bucket_start`.
            let sql = format!(
                "SELECT (at_ms - (((at_ms % $1) + $1) % $1)) AS start, {key} AS key, COUNT(*)::bigint, \
                 COUNT(*) FILTER (WHERE failed)::bigint, SUM(input_tokens)::bigint, SUM(output_tokens)::bigint, \
                 SUM(reasoning_tokens)::bigint, SUM(cache_read_tokens)::bigint, SUM(cache_creation_tokens)::bigint, \
                 SUM(total_tokens)::bigint FROM {table} {} GROUP BY 1, 2 ORDER BY 1, 2",
                w.sql()
            );
            let rows = client.query(&sql, &w.refs()).await.map_err(db("totals"))?;
            rows.iter()
                .map(|row| {
                    Ok(Totals {
                        start: UnixMillis(row.try_get(0)?),
                        key: row.try_get(1)?,
                        requests: row.try_get(2)?,
                        failed: row.try_get(3)?,
                        input_tokens: row.try_get(4)?,
                        output_tokens: row.try_get(5)?,
                        reasoning_tokens: row.try_get(6)?,
                        cache_read_tokens: row.try_get(7)?,
                        cache_creation_tokens: row.try_get(8)?,
                        total_tokens: row.try_get(9)?,
                    })
                })
                .collect()
        })
    }

    fn prune(&self, before: UnixMillis) -> BoxFuture<'_, Result<Pruned>> {
        let (requests, events) = (self.requests.clone(), self.events.clone());
        self.run(move |client| async move {
            let r = client
                .execute(&format!("DELETE FROM {requests} WHERE at_ms < $1"), &[&before.0])
                .await
                .map_err(db("prune requests"))?;
            let e = client
                .execute(&format!("DELETE FROM {events} WHERE at_ms < $1"), &[&before.0])
                .await
                .map_err(db("prune events"))?;
            Ok(Pruned { requests: r, events: e })
        })
    }
}
