//! The history recorder: takes request entries and server log events from the request
//! path and the logger without waiting, and writes them to a [`HistoryStore`] in batches
//! from a background task. A slow or failing store never slows a request: the queue is
//! bounded, and what does not fit is dropped and counted. Retention is applied hourly.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::persistence::history::{EventEntry, HistoryStore, Level, RequestEntry, UnixMillis};

/// Entries waiting for the writer at most. Each is a few hundred bytes to a few KB.
pub const QUEUE: usize = 8192;
/// Entries one write sends at most.
pub const BATCH: usize = 512;
/// How long the writer waits for a batch to fill once it holds an entry.
pub const LINGER: Duration = Duration::from_millis(250);
/// How often retention runs.
pub const PRUNE_EVERY: Duration = Duration::from_secs(3600);
/// Store failures are logged at most this often, so a failing store cannot flood the
/// server log (whose events it would then fail to write).
const WARN_EVERY: Duration = Duration::from_secs(60);

enum Item {
    Request(Box<RequestEntry>),
    Event(EventEntry),
}

/// Counts the dashboard reports about the recorder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Entries dropped because the queue was full or the store refused their batch.
    pub dropped: u64,
    /// Entries written.
    pub written: u64,
}

pub struct Recorder {
    store: Arc<dyn HistoryStore>,
    tx: mpsc::Sender<Item>,
    dropped: AtomicU64,
    written: AtomicU64,
    /// Retention in days; 0 keeps everything.
    retention_days: AtomicI64,
    last_warn: Mutex<Option<std::time::Instant>>,
}

impl Recorder {
    /// Starts the writer and retention tasks on the current Tokio runtime. They stop when
    /// the last `Arc<Recorder>` is dropped.
    pub fn start(store: Arc<dyn HistoryStore>, retention_days: i64) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(QUEUE);
        let recorder = Arc::new(Self {
            store,
            tx,
            dropped: AtomicU64::new(0),
            written: AtomicU64::new(0),
            retention_days: AtomicI64::new(retention_days.max(0)),
            last_warn: Mutex::new(None),
        });
        tokio::spawn(write_loop(Arc::downgrade(&recorder), rx));
        tokio::spawn(prune_loop(Arc::downgrade(&recorder)));
        recorder
    }

    pub fn store(&self) -> &Arc<dyn HistoryStore> {
        &self.store
    }

    pub fn stats(&self) -> Stats {
        Stats {
            dropped: self.dropped.load(Ordering::Relaxed),
            written: self.written.load(Ordering::Relaxed),
        }
    }

    pub fn set_retention_days(&self, days: i64) {
        self.retention_days.store(days.max(0), Ordering::Relaxed);
    }

    pub fn retention_days(&self) -> i64 {
        self.retention_days.load(Ordering::Relaxed)
    }

    /// Records one finished attempt. Never waits.
    pub fn request(&self, entry: RequestEntry) {
        self.offer(Item::Request(Box::new(entry)));
    }

    /// Records one server log event. Never waits; safe to call from the logger.
    pub fn event(&self, entry: EventEntry) {
        self.offer(Item::Event(entry));
    }

    /// A logger hook that records every line the server log writes.
    pub fn log_hook(self: &Arc<Self>) -> crate::gateway::logging::Hook {
        let recorder = Arc::downgrade(self);
        Arc::new(move |e: &crate::gateway::logging::Entry<'_>| {
            let Some(recorder) = recorder.upgrade() else { return };
            recorder.event(EventEntry {
                at: UnixMillis(e.time.timestamp_millis()),
                level: Level::parse(e.level).unwrap_or(Level::Error),
                request_id: e.request_id.to_owned(),
                line: e.line.trim_end_matches(['\r', '\n']).to_owned(),
            });
        })
    }

    fn offer(&self, item: Item) {
        if self.tx.try_send(item).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn warn(&self, what: &str, error: &anyhow::Error) {
        let now = std::time::Instant::now();
        {
            let mut last = self.last_warn.lock().unwrap_or_else(PoisonError::into_inner);
            if last.is_some_and(|at| now.duration_since(at) < WARN_EVERY) {
                return;
            }
            *last = Some(now);
        }
        tracing::warn!("history store ({}): {what}: {error:#}", self.store.backend());
    }

    /// Writes one batch, counting it as written or dropped.
    async fn write(&self, requests: Vec<RequestEntry>, events: Vec<EventEntry>) {
        for (n, result) in [
            (requests.len(), self.store.append_requests(requests).await),
            (events.len(), self.store.append_events(events).await),
        ] {
            if n == 0 {
                continue;
            }
            match result {
                Ok(()) => self.written.fetch_add(n as u64, Ordering::Relaxed),
                Err(e) => {
                    self.dropped.fetch_add(n as u64, Ordering::Relaxed);
                    self.warn("write failed", &e);
                    0
                }
            };
        }
    }
}

async fn write_loop(recorder: std::sync::Weak<Recorder>, mut rx: mpsc::Receiver<Item>) {
    let mut items = Vec::with_capacity(BATCH);
    loop {
        // Wait for a first entry, then let the batch fill for a moment.
        let Some(first) = rx.recv().await else { return };
        items.push(first);
        let deadline = tokio::time::Instant::now() + LINGER;
        while items.len() < BATCH {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(item)) => items.push(item),
                Ok(None) | Err(_) => break,
            }
        }
        let (mut requests, mut events) = (Vec::new(), Vec::new());
        for item in items.drain(..) {
            match item {
                Item::Request(r) => requests.push(*r),
                Item::Event(e) => events.push(e),
            }
        }
        let Some(recorder) = recorder.upgrade() else { return };
        recorder.write(requests, events).await;
    }
}

async fn prune_loop(recorder: std::sync::Weak<Recorder>) {
    let mut tick = tokio::time::interval(PRUNE_EVERY);
    loop {
        tick.tick().await;
        let Some(recorder) = recorder.upgrade() else { return };
        prune_once(&recorder, UnixMillis::now()).await;
    }
}

/// Deletes what is older than the retention window before `now`.
async fn prune_once(recorder: &Recorder, now: UnixMillis) {
    let days = recorder.retention_days();
    if days <= 0 {
        return;
    }
    let before = UnixMillis(now.0.saturating_sub(days.saturating_mul(86_400_000)));
    if let Err(e) = recorder.store.prune(before).await {
        recorder.warn("retention failed", &e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::history::memory::MemoryHistory;
    use crate::persistence::history::{Cursor, EventFilter, RequestFilter};

    fn entry(at: i64) -> RequestEntry {
        RequestEntry {
            at: UnixMillis(at),
            request_id: format!("r{at}"),
            ..RequestEntry::default()
        }
    }

    async fn settle(recorder: &Recorder, total: u64) {
        // Generous: only stops a broken test from hanging.
        for _ in 0..400 {
            let s = recorder.stats();
            if s.written + s.dropped >= total {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("recorder did not settle: {:?}", recorder.stats());
    }

    #[tokio::test]
    async fn entries_reach_the_store_in_order() {
        let store = Arc::new(MemoryHistory::default());
        let recorder = Recorder::start(store.clone(), 0);
        for at in 1..=700 {
            recorder.request(entry(at));
        }
        recorder.event(EventEntry {
            at: UnixMillis(5),
            level: Level::Info,
            request_id: String::new(),
            line: "hello".into(),
        });
        settle(&recorder, 701).await;
        assert_eq!(
            recorder.stats(),
            Stats {
                dropped: 0,
                written: 701
            }
        );
        let page = store
            .requests(RequestFilter::default(), Cursor::Newest, 1000)
            .await
            .unwrap();
        let ats: Vec<_> = page.rows.iter().rev().map(|r| r.entry.at.0).collect();
        assert_eq!(ats, (1..=700).collect::<Vec<_>>());
        let events = store.events(EventFilter::default(), Cursor::Newest, 10).await.unwrap();
        assert_eq!(events.rows[0].entry.line, "hello");
    }

    /// A store whose writes never finish: the request path still never waits, and what
    /// does not fit in the queue is counted as dropped.
    struct Stuck;

    impl HistoryStore for Stuck {
        fn backend(&self) -> &'static str {
            "stuck"
        }
        fn append_requests(&self, _: Vec<RequestEntry>) -> futures_util::future::BoxFuture<'_, anyhow::Result<()>> {
            Box::pin(std::future::pending())
        }
        fn append_events(&self, _: Vec<EventEntry>) -> futures_util::future::BoxFuture<'_, anyhow::Result<()>> {
            Box::pin(std::future::pending())
        }
        fn requests(
            &self,
            _: RequestFilter,
            _: Cursor,
            _: usize,
        ) -> futures_util::future::BoxFuture<
            '_,
            anyhow::Result<crate::persistence::history::Page<crate::persistence::history::StoredRequest>>,
        > {
            unreachable!()
        }
        fn events(
            &self,
            _: EventFilter,
            _: Cursor,
            _: usize,
        ) -> futures_util::future::BoxFuture<
            '_,
            anyhow::Result<crate::persistence::history::Page<crate::persistence::history::StoredEvent>>,
        > {
            unreachable!()
        }
        fn totals(
            &self,
            _: RequestFilter,
            _: i64,
            _: crate::persistence::history::Group,
        ) -> futures_util::future::BoxFuture<'_, anyhow::Result<Vec<crate::persistence::history::Totals>>> {
            unreachable!()
        }
        fn prune(
            &self,
            _: UnixMillis,
        ) -> futures_util::future::BoxFuture<'_, anyhow::Result<crate::persistence::history::Pruned>> {
            Box::pin(std::future::pending())
        }
    }

    #[tokio::test]
    async fn a_stuck_store_drops_what_does_not_fit_without_waiting() {
        let recorder = Recorder::start(Arc::new(Stuck), 0);
        let extra = 100;
        for at in 0..(QUEUE + BATCH + extra) as i64 {
            recorder.request(entry(at));
        }
        // The writer holds at most one batch; the queue holds the rest it can.
        let dropped = recorder.stats().dropped;
        assert!(dropped >= extra as u64, "dropped {dropped}");
        assert!(dropped <= (BATCH + extra) as u64, "dropped {dropped}");
    }

    #[tokio::test]
    async fn retention_deletes_only_what_is_older_than_the_window() {
        let store = Arc::new(MemoryHistory::default());
        let recorder = Recorder::start(store.clone(), 0);
        let day = 86_400_000;
        let now = 100 * day;
        store
            .append_requests(vec![entry(now - 31 * day), entry(now - 29 * day)])
            .await
            .unwrap();
        prune_once(&recorder, UnixMillis(now)).await;
        assert_eq!(
            store
                .requests(RequestFilter::default(), Cursor::Newest, 10)
                .await
                .unwrap()
                .rows
                .len(),
            2,
            "0 keeps everything"
        );
        recorder.set_retention_days(30);
        prune_once(&recorder, UnixMillis(now)).await;
        let left = store
            .requests(RequestFilter::default(), Cursor::Newest, 10)
            .await
            .unwrap();
        assert_eq!(left.rows.len(), 1);
        assert_eq!(left.rows[0].entry.at, UnixMillis(now - 29 * day));
    }

    #[test]
    fn the_log_hook_keeps_the_line_level_and_request() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let store = Arc::new(MemoryHistory::default());
            let recorder = Recorder::start(store.clone(), 0);
            let hook = recorder.log_hook();
            let time = chrono::DateTime::parse_from_rfc3339("2026-11-21T10:00:00+02:00").unwrap();
            hook(&crate::gateway::logging::Entry {
                line: "[2026-11-21 10:00:00] [abcd1234] [warn ] [x.rs:1] slow\n",
                level: "warning",
                time,
                request_id: "req-abcd1234",
            });
            settle(&recorder, 1).await;
            let page = store.events(EventFilter::default(), Cursor::Newest, 10).await.unwrap();
            let e = &page.rows[0].entry;
            assert_eq!(e.level, Level::Warn);
            assert_eq!(e.request_id, "req-abcd1234");
            assert_eq!(e.at, UnixMillis(time.timestamp_millis()));
            assert!(e.line.ends_with("slow"), "{:?}", e.line);
        });
    }
}
