//! The history store contract: what the server records about its own operation (one row
//! per upstream attempt and one per server log event) and the queries the dashboard runs
//! over it. Callers depend on [`HistoryStore`] only; the backends live in
//! [`crate::storage`] (SQLite, PostgreSQL) and [`memory`] (tests).
//!
//! Every backend passes [`conformance::run`], which states the contract as behaviour:
//!
//! - Appends are durable when the returned future resolves `Ok`. A batch is all or none.
//! - Ids are positive, unique and increase in append order within one store; paging is
//!   by id, so a page never skips or repeats a row appended before it was read.
//! - Filters combine with AND. Time bounds are `since <= at < until`.
//! - [`HistoryStore::prune`] deletes rows strictly older than its bound and leaves the
//!   ids of the rest unchanged.
//! - Text is stored and returned as given, except U+0000, which every backend stores as
//!   U+FFFD ([`storable`]): PostgreSQL text cannot hold it, and one entry must not cost
//!   its batch.
//!
//! No row carries a credential or a client key: [`RequestEntry`] has no field for one,
//! and its builders (the usage tracker) leave out key-derived account names.

use std::collections::BTreeMap;
use std::fmt;

use anyhow::Result;
use futures_util::future::BoxFuture;
use serde::Serialize;

/// Milliseconds since the Unix epoch, UTC.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct UnixMillis(pub i64);

impl UnixMillis {
    pub fn now() -> Self {
        Self(chrono::Utc::now().timestamp_millis())
    }
}

/// A row id assigned by the store. Positive; later appends get larger ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct RowId(pub i64);

/// One upstream attempt as the history keeps it. Built by the usage tracker from the
/// same facts as the usage queue's record, without its client key, provider key or
/// client address fields. Token counts follow the record's v2 breakdown: input counts
/// every input token, cache reads and writes included.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RequestEntry {
    /// When the attempt finished.
    pub at: UnixMillis,
    /// The client request's id; several attempts can share one.
    pub request_id: String,
    pub execution_id: String,
    pub provider: String,
    /// The model the client asked for.
    pub model: String,
    /// The model the upstream reported serving; empty when it did not say.
    pub response_model: String,
    /// `METHOD /route/template`.
    pub endpoint: String,
    /// The credential's stable index (`auth_index` in the usage record).
    pub auth_index: String,
    /// The account's display name (email, account or project). Empty when the only
    /// name available was a key.
    pub account: String,
    /// The client's `User-Agent`.
    pub user_agent: String,
    pub stream: bool,
    pub failed: bool,
    /// The status returned to the client for this attempt (200 when it succeeded).
    pub status: u16,
    /// The upstream's failure text after the usage tracker scrubbed request values from
    /// it; empty on success.
    pub fail_body: String,
    /// False when the provider reported no usage, so the token counts are not known
    /// (rather than zero).
    pub usage_reported: bool,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub total_tokens: i64,
    pub latency_ms: i64,
    /// Time to the first token; 0 when none arrived.
    pub ttft_ms: i64,
    /// Milliseconds from when the server accepted the client request to each stage it
    /// timed, by the usage record's `timings` names (`upstream_headers_ms`, ...).
    pub timings: BTreeMap<String, i64>,
    pub session_id: String,
    pub reasoning_effort: String,
    pub service_tier: String,
}

/// `text` with U+0000 replaced by U+FFFD, the form every backend stores.
pub fn storable(text: String) -> String {
    if text.contains('\0') {
        text.replace('\0', "\u{FFFD}")
    } else {
        text
    }
}

impl RequestEntry {
    /// The entry as a backend stores it: see [`storable`]. Every field is named, so a new
    /// text field does not compile until it is placed here.
    pub fn storable(self) -> Self {
        let RequestEntry {
            at,
            request_id,
            execution_id,
            provider,
            model,
            response_model,
            endpoint,
            auth_index,
            account,
            user_agent,
            stream,
            failed,
            status,
            fail_body,
            usage_reported,
            input_tokens,
            output_tokens,
            reasoning_tokens,
            cache_read_tokens,
            cache_creation_tokens,
            total_tokens,
            latency_ms,
            ttft_ms,
            timings,
            session_id,
            reasoning_effort,
            service_tier,
        } = self;
        RequestEntry {
            at,
            request_id: storable(request_id),
            execution_id: storable(execution_id),
            provider: storable(provider),
            model: storable(model),
            response_model: storable(response_model),
            endpoint: storable(endpoint),
            auth_index: storable(auth_index),
            account: storable(account),
            user_agent: storable(user_agent),
            stream,
            failed,
            status,
            fail_body: storable(fail_body),
            usage_reported,
            input_tokens,
            output_tokens,
            reasoning_tokens,
            cache_read_tokens,
            cache_creation_tokens,
            total_tokens,
            latency_ms,
            ttft_ms,
            timings: timings.into_iter().map(|(k, v)| (storable(k), v)).collect(),
            session_id: storable(session_id),
            reasoning_effort: storable(reasoning_effort),
            service_tier: storable(service_tier),
        }
    }
}

/// A stored [`RequestEntry`] with its id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoredRequest {
    pub id: RowId,
    #[serde(flatten)]
    pub entry: RequestEntry,
}

/// Server log severity, ordered from least to most severe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Trace = 0,
    Debug = 1,
    Info = 2,
    Warn = 3,
    Error = 4,
}

impl Level {
    pub fn code(self) -> i16 {
        self as i16
    }

    pub fn from_code(code: i64) -> Option<Self> {
        Some(match code {
            0 => Self::Trace,
            1 => Self::Debug,
            2 => Self::Info,
            3 => Self::Warn,
            4 => Self::Error,
            _ => return None,
        })
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "trace" => Self::Trace,
            "debug" => Self::Debug,
            "info" => Self::Info,
            "warn" | "warning" => Self::Warn,
            "error" => Self::Error,
            _ => return None,
        })
    }
}

impl From<tracing::Level> for Level {
    fn from(level: tracing::Level) -> Self {
        match level {
            tracing::Level::TRACE => Self::Trace,
            tracing::Level::DEBUG => Self::Debug,
            tracing::Level::INFO => Self::Info,
            tracing::Level::WARN => Self::Warn,
            tracing::Level::ERROR => Self::Error,
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        })
    }
}

/// One server log event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EventEntry {
    pub at: UnixMillis,
    pub level: Level,
    /// The request the event belongs to; empty for server-wide events.
    pub request_id: String,
    /// The formatted log line, as the server log prints it.
    pub line: String,
}

impl EventEntry {
    /// The event as a backend stores it: see [`storable`].
    pub fn storable(self) -> Self {
        EventEntry {
            request_id: storable(self.request_id),
            line: storable(self.line),
            ..self
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoredEvent {
    pub id: RowId,
    #[serde(flatten)]
    pub entry: EventEntry,
}

/// Where a page starts. Pages are ordered by id: newest first for `Before`/`Newest`,
/// oldest first for `After`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Cursor {
    /// The newest rows.
    #[default]
    Newest,
    /// Rows with smaller ids, newest first.
    Before(RowId),
    /// Rows with larger ids, oldest first.
    After(RowId),
}

/// At most this many rows per page, whatever the caller asks for.
pub const MAX_PAGE: usize = 1000;

#[derive(Debug, Clone)]
pub struct Page<T> {
    pub rows: Vec<T>,
    /// More rows match beyond this page in its direction.
    pub more: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestFilter {
    pub since: Option<UnixMillis>,
    pub until: Option<UnixMillis>,
    pub provider: Option<String>,
    pub auth_index: Option<String>,
    pub model: Option<String>,
    pub request_id: Option<String>,
    pub failed: Option<bool>,
}

impl RequestFilter {
    /// The filter as a predicate; the reference every backend's SQL must agree with.
    pub fn matches(&self, e: &RequestEntry) -> bool {
        self.since.is_none_or(|t| e.at >= t)
            && self.until.is_none_or(|t| e.at < t)
            && self.provider.as_ref().is_none_or(|p| &e.provider == p)
            && self.auth_index.as_ref().is_none_or(|a| &e.auth_index == a)
            && self.model.as_ref().is_none_or(|m| &e.model == m)
            && self.request_id.as_ref().is_none_or(|r| &e.request_id == r)
            && self.failed.is_none_or(|f| e.failed == f)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventFilter {
    pub since: Option<UnixMillis>,
    pub until: Option<UnixMillis>,
    /// Events at this level or more severe.
    pub min_level: Option<Level>,
    pub request_id: Option<String>,
}

impl EventFilter {
    pub fn matches(&self, e: &EventEntry) -> bool {
        self.since.is_none_or(|t| e.at >= t)
            && self.until.is_none_or(|t| e.at < t)
            && self.min_level.is_none_or(|l| e.level >= l)
            && self.request_id.as_ref().is_none_or(|r| &e.request_id == r)
    }
}

/// What [`HistoryStore::totals`] groups by besides the time bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Group {
    #[default]
    None,
    Provider,
    Account,
    Model,
}

impl Group {
    pub fn key(self, e: &RequestEntry) -> &str {
        match self {
            Self::None => "",
            Self::Provider => &e.provider,
            Self::Account => &e.auth_index,
            Self::Model => &e.model,
        }
    }
}

/// Request counts and token sums for one time bucket and group key.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Totals {
    /// The bucket's start: `at` rounded down to a multiple of the bucket width.
    pub start: UnixMillis,
    /// The group's value; empty for [`Group::None`].
    pub key: String,
    pub requests: i64,
    pub failed: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub total_tokens: i64,
}

impl Totals {
    pub fn add(&mut self, e: &RequestEntry) {
        self.requests += 1;
        self.failed += i64::from(e.failed);
        self.input_tokens += e.input_tokens;
        self.output_tokens += e.output_tokens;
        self.reasoning_tokens += e.reasoning_tokens;
        self.cache_read_tokens += e.cache_read_tokens;
        self.cache_creation_tokens += e.cache_creation_tokens;
        self.total_tokens += e.total_tokens;
    }
}

/// Rows deleted by one [`HistoryStore::prune`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pruned {
    pub requests: u64,
    pub events: u64,
}

/// Floors `at` to a multiple of `width` (both in milliseconds, `width > 0`), toward
/// negative infinity.
pub fn bucket_start(at: UnixMillis, width: i64) -> UnixMillis {
    UnixMillis(at.0.div_euclid(width) * width)
}

/// The history store. Implementations are cheap to share (`Arc<dyn HistoryStore>`); the
/// futures they return own what they need, so callers can spawn them.
pub trait HistoryStore: Send + Sync + 'static {
    /// The backend's name for logs and the dashboard (`sqlite`, `postgres`, `memory`).
    fn backend(&self) -> &'static str;
    fn append_requests(&self, batch: Vec<RequestEntry>) -> BoxFuture<'_, Result<()>>;
    fn append_events(&self, batch: Vec<EventEntry>) -> BoxFuture<'_, Result<()>>;
    /// One page of requests matching `filter`; `limit` is capped at [`MAX_PAGE`].
    fn requests(
        &self,
        filter: RequestFilter,
        cursor: Cursor,
        limit: usize,
    ) -> BoxFuture<'_, Result<Page<StoredRequest>>>;
    fn events(&self, filter: EventFilter, cursor: Cursor, limit: usize) -> BoxFuture<'_, Result<Page<StoredEvent>>>;
    /// Sums over requests matching `filter`, per `bucket_ms`-wide bucket and `group`
    /// key, ordered by bucket start and then key. Buckets without requests are absent.
    /// `bucket_ms` must be positive.
    fn totals(&self, filter: RequestFilter, bucket_ms: i64, group: Group) -> BoxFuture<'_, Result<Vec<Totals>>>;
    /// Deletes requests and events with `at < before`.
    fn prune(&self, before: UnixMillis) -> BoxFuture<'_, Result<Pruned>>;
}

pub mod memory {
    //! An in-memory [`HistoryStore`]: the reference implementation of the contract, used
    //! by tests and wherever no durable backend is configured.

    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    pub struct MemoryHistory {
        inner: Mutex<Inner>,
    }

    #[derive(Default)]
    struct Inner {
        next: i64,
        requests: BTreeMap<i64, RequestEntry>,
        events: BTreeMap<i64, EventEntry>,
    }

    impl Inner {
        fn id(&mut self) -> i64 {
            self.next += 1;
            self.next
        }
    }

    fn page<T: Clone, S>(
        rows: &BTreeMap<i64, T>,
        keep: impl Fn(&T) -> bool,
        cursor: Cursor,
        limit: usize,
        wrap: impl Fn(i64, &T) -> S,
    ) -> Page<S> {
        let limit = limit.min(MAX_PAGE);
        let mut matching: Box<dyn Iterator<Item = (&i64, &T)>> = match cursor {
            Cursor::Newest => Box::new(rows.iter().rev()),
            Cursor::Before(RowId(id)) => Box::new(rows.range(..id).rev()),
            Cursor::After(RowId(id)) => Box::new(rows.range(id.saturating_add(1)..)),
        };
        let mut out = Vec::new();
        let mut more = false;
        for (id, row) in matching.by_ref() {
            if !keep(row) {
                continue;
            }
            if out.len() == limit {
                more = true;
                break;
            }
            out.push(wrap(*id, row));
        }
        Page { rows: out, more }
    }

    impl MemoryHistory {
        fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
            self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }

    impl HistoryStore for MemoryHistory {
        fn backend(&self) -> &'static str {
            "memory"
        }

        fn append_requests(&self, batch: Vec<RequestEntry>) -> BoxFuture<'_, Result<()>> {
            let mut inner = self.lock();
            for entry in batch {
                let id = inner.id();
                inner.requests.insert(id, entry.storable());
            }
            Box::pin(async { Ok(()) })
        }

        fn append_events(&self, batch: Vec<EventEntry>) -> BoxFuture<'_, Result<()>> {
            let mut inner = self.lock();
            for entry in batch {
                let id = inner.id();
                inner.events.insert(id, entry.storable());
            }
            Box::pin(async { Ok(()) })
        }

        fn requests(
            &self,
            filter: RequestFilter,
            cursor: Cursor,
            limit: usize,
        ) -> BoxFuture<'_, Result<Page<StoredRequest>>> {
            let out = page(
                &self.lock().requests,
                |e| filter.matches(e),
                cursor,
                limit,
                |id, e| StoredRequest {
                    id: RowId(id),
                    entry: e.clone(),
                },
            );
            Box::pin(async { Ok(out) })
        }

        fn events(
            &self,
            filter: EventFilter,
            cursor: Cursor,
            limit: usize,
        ) -> BoxFuture<'_, Result<Page<StoredEvent>>> {
            let out = page(
                &self.lock().events,
                |e| filter.matches(e),
                cursor,
                limit,
                |id, e| StoredEvent {
                    id: RowId(id),
                    entry: e.clone(),
                },
            );
            Box::pin(async { Ok(out) })
        }

        fn totals(&self, filter: RequestFilter, bucket_ms: i64, group: Group) -> BoxFuture<'_, Result<Vec<Totals>>> {
            if bucket_ms <= 0 {
                return Box::pin(async { Err(anyhow::anyhow!("bucket width must be positive")) });
            }
            let mut sums: BTreeMap<(i64, String), Totals> = BTreeMap::new();
            for e in self.lock().requests.values().filter(|e| filter.matches(e)) {
                let start = bucket_start(e.at, bucket_ms);
                let key = group.key(e).to_owned();
                sums.entry((start.0, key.clone()))
                    .or_insert_with(|| Totals {
                        start,
                        key,
                        ..Totals::default()
                    })
                    .add(e);
            }
            let out = sums.into_values().collect();
            Box::pin(async { Ok(out) })
        }

        fn prune(&self, before: UnixMillis) -> BoxFuture<'_, Result<Pruned>> {
            let mut inner = self.lock();
            let requests = inner.requests.len();
            inner.requests.retain(|_, e| e.at >= before);
            let events = inner.events.len();
            inner.events.retain(|_, e| e.at >= before);
            let pruned = Pruned {
                requests: (requests - inner.requests.len()) as u64,
                events: (events - inner.events.len()) as u64,
            };
            Box::pin(async move { Ok(pruned) })
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod conformance {
    //! The contract as tests. Every backend runs [`run`] against a fresh, empty store.

    use super::*;

    fn request(at: i64, provider: &str, auth_index: &str, model: &str, failed: bool, tokens: i64) -> RequestEntry {
        RequestEntry {
            at: UnixMillis(at),
            request_id: format!("req-{at}"),
            execution_id: format!("exec-{at}"),
            provider: provider.into(),
            model: model.into(),
            response_model: format!("{model}-served"),
            endpoint: "POST /v1/messages".into(),
            auth_index: auth_index.into(),
            account: format!("{auth_index}@example.test"),
            user_agent: "claude-cli/2.1 (external, cli)".into(),
            stream: at % 2 == 0,
            failed,
            status: if failed { 429 } else { 200 },
            fail_body: if failed {
                "rate limited \u{1F6A6} ünïcode".into()
            } else {
                String::new()
            },
            usage_reported: tokens > 0,
            input_tokens: tokens,
            output_tokens: tokens / 2,
            reasoning_tokens: tokens / 4,
            cache_read_tokens: tokens * 3,
            cache_creation_tokens: tokens / 8,
            total_tokens: tokens + tokens / 2,
            latency_ms: 1200,
            ttft_ms: 300,
            timings: if at % 2 == 0 {
                BTreeMap::from([("upstream_headers_ms".into(), 90), ("client_first_byte_ms".into(), 310)])
            } else {
                BTreeMap::new()
            },
            session_id: "session".into(),
            reasoning_effort: "high".into(),
            service_tier: "default".into(),
        }
    }

    fn event(at: i64, level: Level, request_id: &str) -> EventEntry {
        EventEntry {
            at: UnixMillis(at),
            level,
            request_id: request_id.into(),
            line: format!("[{at}] {level} line with 'quotes' and \"doubles\" ü"),
        }
    }

    /// Runs every contract check against `store`, which must start empty.
    pub async fn run(store: &dyn HistoryStore) {
        let r = |at, p, a, m, f, t| request(at, p, a, m, f, t);
        let fixture = vec![
            r(1_000, "claude", "a1", "opus", false, 100),
            r(2_000, "codex", "a2", "gpt", true, 0),
            r(61_000, "claude", "a1", "sonnet", false, 40),
            r(62_000, "claude", "a2", "opus", false, 8),
            r(125_000, "xai", "a3", "grok", true, 16),
        ];
        assert!(
            store
                .requests(RequestFilter::default(), Cursor::Newest, 10)
                .await
                .unwrap()
                .rows
                .is_empty()
        );
        store.append_requests(fixture[..2].to_vec()).await.unwrap();
        store.append_requests(fixture[2..].to_vec()).await.unwrap();
        store.append_requests(Vec::new()).await.unwrap();

        // Round trip and newest-first order.
        let all = store
            .requests(RequestFilter::default(), Cursor::Newest, 100)
            .await
            .unwrap();
        assert!(!all.more);
        let entries: Vec<_> = all.rows.iter().map(|s| s.entry.clone()).rev().collect();
        assert_eq!(entries, fixture, "rows round-trip unchanged, in append order");
        let ids: Vec<_> = all.rows.iter().map(|s| s.id).collect();
        assert!(ids.windows(2).all(|w| w[0] > w[1]), "ids increase with append order");
        assert!(ids.iter().all(|id| id.0 > 0));

        // Paging in both directions visits every row exactly once.
        let mut seen = Vec::new();
        let mut cursor = Cursor::Newest;
        loop {
            let page = store.requests(RequestFilter::default(), cursor, 2).await.unwrap();
            assert!(page.rows.len() <= 2);
            seen.extend(page.rows.iter().map(|s| s.id));
            let Some(last) = page.rows.last() else { break };
            cursor = Cursor::Before(last.id);
            if !page.more {
                break;
            }
        }
        assert_eq!(seen, ids);
        let oldest = *ids.last().unwrap();
        let forward = store
            .requests(RequestFilter::default(), Cursor::After(oldest), 3)
            .await
            .unwrap();
        assert!(forward.more);
        let forward_ids: Vec<_> = forward.rows.iter().map(|s| s.id).collect();
        let expected: Vec<_> = ids.iter().rev().skip(1).take(3).copied().collect();
        assert_eq!(forward_ids, expected, "After pages oldest first");
        let newest = ids[0];
        let past = store
            .requests(RequestFilter::default(), Cursor::After(newest), 3)
            .await
            .unwrap();
        assert!(past.rows.is_empty() && !past.more, "nothing after the newest row");
        let zero = store
            .requests(RequestFilter::default(), Cursor::Newest, 0)
            .await
            .unwrap();
        assert!(zero.rows.is_empty() && zero.more, "an empty page still reports more");

        // Filters agree with the reference predicate.
        let filters = [
            RequestFilter {
                since: Some(UnixMillis(2_000)),
                until: Some(UnixMillis(62_000)),
                ..Default::default()
            },
            RequestFilter {
                provider: Some("claude".into()),
                ..Default::default()
            },
            RequestFilter {
                auth_index: Some("a2".into()),
                failed: Some(false),
                ..Default::default()
            },
            RequestFilter {
                model: Some("opus".into()),
                since: Some(UnixMillis(1_001)),
                ..Default::default()
            },
            RequestFilter {
                request_id: Some("req-125000".into()),
                ..Default::default()
            },
            RequestFilter {
                failed: Some(true),
                ..Default::default()
            },
            RequestFilter {
                provider: Some("nobody".into()),
                ..Default::default()
            },
        ];
        for filter in filters {
            let got: Vec<_> = store
                .requests(filter.clone(), Cursor::Newest, 100)
                .await
                .unwrap()
                .rows
                .into_iter()
                .map(|s| s.entry)
                .rev()
                .collect();
            let want: Vec<_> = fixture.iter().filter(|e| filter.matches(e)).cloned().collect();
            assert_eq!(got, want, "{filter:?}");
        }

        // Totals per minute, with and without a group.
        let minute = 60_000;
        let totals = store
            .totals(RequestFilter::default(), minute, Group::None)
            .await
            .unwrap();
        let starts: Vec<_> = totals.iter().map(|t| (t.start.0, t.requests, t.failed)).collect();
        assert_eq!(starts, [(0, 2, 1), (60_000, 2, 0), (120_000, 1, 1)]);
        assert_eq!(totals[0].input_tokens, 100);
        assert_eq!(totals[0].cache_read_tokens, 300);
        assert_eq!(totals[1].total_tokens, 60 + 12);
        assert!(totals.iter().all(|t| t.key.is_empty()));
        let by_model = store
            .totals(
                RequestFilter {
                    provider: Some("claude".into()),
                    ..Default::default()
                },
                minute,
                Group::Model,
            )
            .await
            .unwrap();
        let keys: Vec<_> = by_model
            .iter()
            .map(|t| (t.start.0, t.key.as_str(), t.requests))
            .collect();
        assert_eq!(keys, [(0, "opus", 1), (60_000, "opus", 1), (60_000, "sonnet", 1)]);
        let by_account = store
            .totals(RequestFilter::default(), 1_000_000, Group::Account)
            .await
            .unwrap();
        let keys: Vec<_> = by_account.iter().map(|t| (t.key.as_str(), t.requests)).collect();
        assert_eq!(keys, [("a1", 2), ("a2", 2), ("a3", 1)]);
        assert!(store.totals(RequestFilter::default(), 0, Group::None).await.is_err());

        // Events.
        let events = vec![
            event(1_000, Level::Info, ""),
            event(2_000, Level::Debug, "req-2000"),
            event(3_000, Level::Warn, "req-2000"),
            event(4_000, Level::Error, ""),
        ];
        store.append_events(events.clone()).await.unwrap();
        let got: Vec<_> = store
            .events(EventFilter::default(), Cursor::Newest, 10)
            .await
            .unwrap()
            .rows
            .into_iter()
            .map(|s| s.entry)
            .rev()
            .collect();
        assert_eq!(got, events);
        for filter in [
            EventFilter {
                min_level: Some(Level::Warn),
                ..Default::default()
            },
            EventFilter {
                request_id: Some("req-2000".into()),
                since: Some(UnixMillis(2_500)),
                ..Default::default()
            },
            EventFilter {
                until: Some(UnixMillis(2_000)),
                ..Default::default()
            },
        ] {
            let got: Vec<_> = store
                .events(filter.clone(), Cursor::Newest, 10)
                .await
                .unwrap()
                .rows
                .into_iter()
                .map(|s| s.entry)
                .rev()
                .collect();
            let want: Vec<_> = events.iter().filter(|e| filter.matches(e)).cloned().collect();
            assert_eq!(got, want, "{filter:?}");
        }
        let first = store.events(EventFilter::default(), Cursor::Newest, 1).await.unwrap();
        assert!(first.more);
        let rest = store
            .events(EventFilter::default(), Cursor::Before(first.rows[0].id), 10)
            .await
            .unwrap();
        assert_eq!(rest.rows.len(), 3);

        // U+0000 is stored as U+FFFD, without costing the rest of its batch.
        let mut nul = r(1_500, "claude", "a1", "opus", true, 0);
        nul.fail_body = "bad\0body".into();
        nul.timings.insert("x\0y".into(), 1);
        store.append_requests(vec![nul]).await.unwrap();
        let got = store
            .requests(
                RequestFilter {
                    request_id: Some("req-1500".into()),
                    ..Default::default()
                },
                Cursor::Newest,
                1,
            )
            .await
            .unwrap();
        assert_eq!(got.rows[0].entry.fail_body, "bad\u{FFFD}body");
        assert_eq!(got.rows[0].entry.timings.get("x\u{FFFD}y"), Some(&1));
        store
            .append_events(vec![EventEntry {
                at: UnixMillis(1_500),
                level: Level::Error,
                request_id: "r\0".into(),
                line: "nul \0 here".into(),
            }])
            .await
            .unwrap();
        let newest = store.events(EventFilter::default(), Cursor::Newest, 1).await.unwrap();
        assert_eq!(newest.rows[0].entry.line, "nul \u{FFFD} here");
        assert_eq!(newest.rows[0].entry.request_id, "r\u{FFFD}");

        // Pruning removes strictly older rows and keeps the ids of the rest.
        let pruned = store.prune(UnixMillis(61_000)).await.unwrap();
        assert_eq!(pruned, Pruned { requests: 3, events: 5 });
        let left = store
            .requests(RequestFilter::default(), Cursor::Newest, 10)
            .await
            .unwrap();
        assert_eq!(left.rows.iter().map(|s| s.id).collect::<Vec<_>>(), ids[..3]);
        assert_eq!(store.prune(UnixMillis(61_000)).await.unwrap(), Pruned::default());

        // New appends after a prune still get larger ids.
        store
            .append_requests(vec![r(200_000, "meta", "a4", "muse", false, 1)])
            .await
            .unwrap();
        let newest_now = store
            .requests(RequestFilter::default(), Cursor::Newest, 1)
            .await
            .unwrap();
        assert!(newest_now.rows[0].id > ids[0]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_history_meets_the_contract() {
        conformance::run(&memory::MemoryHistory::default()).await;
    }

    #[test]
    fn buckets_floor_toward_negative_infinity() {
        assert_eq!(bucket_start(UnixMillis(59_999), 60_000), UnixMillis(0));
        assert_eq!(bucket_start(UnixMillis(60_000), 60_000), UnixMillis(60_000));
        assert_eq!(bucket_start(UnixMillis(-1), 60_000), UnixMillis(-60_000));
    }

    #[test]
    fn levels_round_trip_through_their_codes_and_names() {
        for level in [Level::Trace, Level::Debug, Level::Info, Level::Warn, Level::Error] {
            assert_eq!(Level::from_code(level.code().into()), Some(level));
            assert_eq!(Level::parse(&level.to_string()), Some(level));
        }
        assert_eq!(Level::from_code(9), None);
        assert_eq!(Level::parse("WARNING"), Some(Level::Warn));
    }
}
