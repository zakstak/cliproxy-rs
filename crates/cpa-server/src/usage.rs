//! Go `internal/redisqueue`: the in-memory usage queue that management's
//! `GET /observability/usage/queue` drains. The request path enqueues one JSON record
//! per upstream request (Go `queuedUsageDetail`); records older than the retention
//! window are pruned on every access.
//!
//! RESP clients on the main listener (crate::resp) subscribe to the `usage` and
//! `errors` channels; while any usage subscriber is connected, records go to them
//! instead of the queue, as in Go.
//!
//! The dashboard's live view reads [`LiveFeed`] instead, a cliproxy-rs addition: a
//! bounded copy of every accepted record that readers page through by cursor without
//! removing anything, so it never competes with the queue, its RESP subscribers or
//! other viewers.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use cpa_core::config::Config;

const DEFAULT_RETENTION_SECONDS: i64 = 60;
const MAX_RETENTION_SECONDS: i64 = 3600;

pub struct UsageQueue {
    /// Go `redisqueue.Enabled`: management routes are enabled.
    enabled: AtomicBool,
    /// Go `UsageStatisticsEnabled`: `observability.usage.usage-statistics-enabled`.
    statistics: AtomicBool,
    /// Separate, default-off final-wire observations; not a Go usage setting.
    claude_cache: AtomicBool,
    retention_seconds: AtomicI64,
    items: Mutex<VecDeque<(Instant, Vec<u8>)>>,
    usage_subscribers: std::sync::Arc<Subscribers>,
    error_subscribers: std::sync::Arc<Subscribers>,
    live: Mutex<LiveFeed>,
}

/// Go `usageSubscriberBuffer` / `errorSubscriberBuffer`.
const SUBSCRIBER_BUFFER: usize = 256;
/// Go `usageSupportRefreshPayload`: the first message every usage subscriber gets.
const SUPPORT_REFRESH: &[u8] = br#"{"support_refresh":true}"#;
/// Go `usageRefreshPayload`.
const REFRESH: &[u8] = br#"{"refresh":true}"#;

/// The last subscription ID and the live subscribers by ID.
type SubscriberMap = (u64, std::collections::HashMap<u64, tokio::sync::mpsc::Sender<Vec<u8>>>);

/// Go `queue.subscribers`: bounded channels keyed by subscription.
#[derive(Default)]
struct Subscribers(Mutex<SubscriberMap>);

impl Subscribers {
    fn lock(&self) -> std::sync::MutexGuard<'_, SubscriberMap> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Go `publishToSubscribers`: whether anyone listened. A subscriber whose buffer
    /// is full is dropped, which ends its subscription.
    fn publish(&self, payload: &[u8]) -> bool {
        let mut guard = self.lock();
        if guard.1.is_empty() {
            return false;
        }
        guard.1.retain(|_, tx| tx.try_send(payload.to_vec()).is_ok());
        true
    }

    fn subscribe(self: &std::sync::Arc<Self>, greeting: Option<&[u8]>) -> Subscription {
        let (tx, rx) = tokio::sync::mpsc::channel(SUBSCRIBER_BUFFER);
        if let Some(greeting) = greeting {
            let _ = tx.try_send(greeting.to_vec());
        }
        let mut guard = self.lock();
        guard.0 += 1;
        let id = guard.0;
        guard.1.insert(id, tx);
        Subscription {
            messages: rx,
            owner: std::sync::Arc::downgrade(self),
            id,
        }
    }

    fn clear(&self) {
        self.lock().1.clear();
    }
}

/// One channel subscription; dropping it unsubscribes (Go's `unsubscribe`). `messages`
/// ends when the queue drops the subscriber (full buffer, or the queue was disabled).
pub struct Subscription {
    pub messages: tokio::sync::mpsc::Receiver<Vec<u8>>,
    owner: std::sync::Weak<Subscribers>,
    id: u64,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.upgrade() {
            owner.lock().1.remove(&self.id);
        }
    }
}

impl Default for UsageQueue {
    fn default() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            // Go's package default; startup immediately applies the config value.
            statistics: AtomicBool::new(true),
            claude_cache: AtomicBool::new(false),
            retention_seconds: AtomicI64::new(DEFAULT_RETENTION_SECONDS),
            items: Mutex::default(),
            usage_subscribers: std::sync::Arc::default(),
            error_subscribers: std::sync::Arc::default(),
            live: Mutex::default(),
        }
    }
}

impl UsageQueue {
    /// Applies one published config (Go server_reload.go): disabling the queue drops
    /// everything queued; retention is clamped to 1..=3600 seconds.
    pub fn configure(&self, management_enabled: bool, cfg: &Config) {
        let usage = cfg.document.get("observability").and_then(|o| o.get("usage"));
        let statistics = usage
            .and_then(|u| u.get("usage-statistics-enabled"))
            .and_then(serde_yaml_ng::Value::as_bool)
            .unwrap_or(false);
        self.claude_cache.store(
            usage
                .and_then(|u| u.get("claude-cache-telemetry"))
                .and_then(serde_yaml_ng::Value::as_bool)
                .unwrap_or(false),
            Ordering::SeqCst,
        );
        let retention = usage
            .and_then(|u| u.get("redis-usage-queue-retention-seconds"))
            .and_then(serde_yaml_ng::Value::as_i64)
            .unwrap_or(0);
        self.statistics.store(statistics, Ordering::SeqCst);
        self.retention_seconds.store(
            if retention <= 0 {
                DEFAULT_RETENTION_SECONDS
            } else {
                retention.min(MAX_RETENTION_SECONDS)
            },
            Ordering::SeqCst,
        );
        self.enabled.store(management_enabled, Ordering::SeqCst);
        if !management_enabled {
            self.lock().clear();
            self.usage_subscribers.clear();
            self.error_subscribers.clear();
            self.live_lock().clear();
        }
    }

    /// Whether a usage record would be kept; producers can skip building one.
    pub fn accepts(&self) -> bool {
        self.enabled.load(Ordering::SeqCst) && self.statistics.load(Ordering::SeqCst)
    }

    pub fn wants_claude_cache(&self) -> bool {
        self.accepts() && self.claude_cache.load(Ordering::SeqCst)
    }

    /// Queues one serialized usage record (Go `usageQueuePlugin.HandleUsage` +
    /// `Enqueue`). The dashboard's live feed observes it first, whoever then takes it.
    pub fn enqueue(&self, payload: Vec<u8>) {
        if !self.accepts() || payload.is_empty() {
            return;
        }
        self.live_lock().observe(&payload, Instant::now());
        self.deliver(payload);
    }

    /// Puts back a record that was popped but could not be delivered (Home's usage
    /// forwarder). The live feed saw it when it was first enqueued.
    pub fn requeue(&self, payload: Vec<u8>) {
        if !self.accepts() || payload.is_empty() {
            return;
        }
        self.deliver(payload);
    }

    fn deliver(&self, payload: Vec<u8>) {
        // Go `Enqueue`: connected RESP subscribers take the record instead of the queue.
        if self.usage_subscribers.publish(&payload) {
            return;
        }
        let now = Instant::now();
        let mut items = self.lock();
        self.prune(&mut items, now);
        items.push_back((now, payload));
    }

    /// Go `SubscribeUsage`: usage records as they are produced, after a first
    /// `{"support_refresh":true}`.
    pub fn subscribe_usage(&self) -> Subscription {
        self.usage_subscribers.subscribe(Some(SUPPORT_REFRESH))
    }

    /// Go `SubscribeErrors`.
    pub fn subscribe_errors(&self) -> Subscription {
        self.error_subscribers.subscribe(None)
    }

    /// Go `EnqueueError`: an error event for `errors` subscribers; nothing is queued.
    pub fn enqueue_error(&self, payload: &[u8]) {
        if self.enabled.load(Ordering::SeqCst) && !payload.is_empty() {
            self.error_subscribers.publish(payload);
        }
    }

    /// Whether an error event would reach anyone; producers can skip building one.
    pub fn wants_errors(&self) -> bool {
        self.enabled.load(Ordering::SeqCst) && !self.error_subscribers.lock().1.is_empty()
    }

    /// Go `NotifyUsageRefresh`: tells usage subscribers the credential set changed.
    pub fn notify_usage_refresh(&self) {
        self.usage_subscribers.publish(REFRESH);
    }

    /// Go `PopOldest`: removes and returns up to `count` unexpired records, oldest first.
    pub fn pop_oldest(&self, count: usize) -> Vec<Vec<u8>> {
        if !self.enabled.load(Ordering::SeqCst) || count == 0 {
            return Vec::new();
        }
        let mut items = self.lock();
        self.prune(&mut items, Instant::now());
        let n = count.min(items.len());
        items.drain(..n).map(|(_, payload)| payload).collect()
    }

    fn prune(&self, items: &mut VecDeque<(Instant, Vec<u8>)>, now: Instant) {
        let window = Duration::from_secs(self.retention_seconds.load(Ordering::SeqCst).max(1) as u64);
        while items
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) > window)
        {
            items.pop_front();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<(Instant, Vec<u8>)>> {
        self.items.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// One page of the dashboard's live feed after `after` (see [`LiveFeed::read`]).
    /// Nothing is removed: the queue and other readers are unaffected.
    pub fn live_read(&self, after: Option<u64>, count: usize) -> LivePage {
        self.live_lock().read(after, count, Instant::now())
    }

    fn live_lock(&self) -> std::sync::MutexGuard<'_, LiveFeed> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Records the live feed keeps at most; the dashboard shows at most 2,000.
pub const LIVE_MAX_RECORDS: usize = 2000;
/// Payload bytes the live feed keeps at most, plus the newest record.
// ponytail: the newest record is always kept, so one oversized record can exceed this by
// its own size. Records are server-built and a few KB; cap or drop single records if a
// record type ever grows large.
pub const LIVE_MAX_BYTES: usize = 8 << 20;
/// Records one live read returns at most.
pub const LIVE_MAX_PAGE: usize = 500;
/// The feed records only while someone read it this recently, and empties itself
/// otherwise, so an unwatched server holds no copies.
pub const LIVE_IDLE: Duration = Duration::from_secs(60);

/// The dashboard's live feed: the most recent accepted usage records, each with a
/// sequence number. Every accepted record takes the next number, whether it is kept or
/// not, so a reader can tell continuity from its cursor alone and the server keeps no
/// state per reader. Memory is bounded by [`LIVE_MAX_RECORDS`] and [`LIVE_MAX_BYTES`]
/// whatever the number of readers; a reader that falls behind gets a gap, never
/// back-pressure on the request path.
#[derive(Default)]
struct LiveFeed {
    /// A contiguous run of sequence numbers, oldest first.
    records: VecDeque<(u64, Arc<[u8]>)>,
    bytes: usize,
    /// The last sequence number taken; 0 before the first record.
    last: u64,
    /// The newest sequence number that was not kept (evicted, unwatched or cleared).
    /// A cursor at or after it has missed nothing.
    floor: u64,
    /// When a reader last read; `None` before the first read.
    read_at: Option<Instant>,
}

/// One page of the live feed.
pub struct LivePage {
    /// The records after the reader's cursor, oldest first.
    pub records: Vec<Arc<[u8]>>,
    /// The cursor for the next read.
    pub cursor: u64,
    /// Records between the reader's cursor and this page are no longer available:
    /// they were evicted, arrived while nobody watched, or the cursor is not from this
    /// process.
    pub gap: bool,
    /// More records are waiting after this page.
    pub more: bool,
}

impl LiveFeed {
    fn watched(&self, now: Instant) -> bool {
        self.read_at
            .is_some_and(|at| now.saturating_duration_since(at) <= LIVE_IDLE)
    }

    fn observe(&mut self, payload: &[u8], now: Instant) {
        self.last += 1;
        if !self.watched(now) {
            self.clear();
            return;
        }
        // Evicting from the front keeps the kept run contiguous.
        while !self.records.is_empty()
            && (self.records.len() >= LIVE_MAX_RECORDS || self.bytes + payload.len() > LIVE_MAX_BYTES)
        {
            if let Some((seq, old)) = self.records.pop_front() {
                self.bytes -= old.len();
                self.floor = seq;
            }
        }
        self.bytes += payload.len();
        self.records.push_back((self.last, Arc::from(payload)));
    }

    /// Drops every kept record; cursors from before now see a gap.
    fn clear(&mut self) {
        self.records = VecDeque::new();
        self.bytes = 0;
        self.floor = self.last;
    }

    /// Records after `after`, at most `count`. Without a cursor the reader starts at
    /// the newest record and gets none. A cursor before the floor, or after the last
    /// sequence number (another process), reads from the oldest kept record with `gap`.
    fn read(&mut self, after: Option<u64>, count: usize, now: Instant) -> LivePage {
        self.read_at = Some(now);
        let Some(after) = after else {
            return LivePage {
                records: Vec::new(),
                cursor: self.last,
                gap: false,
                more: false,
            };
        };
        let gap = after < self.floor || after > self.last;
        let from = if gap { self.floor } else { after };
        // Every kept record is newer than the floor, and their numbers are contiguous.
        let skip = self.records.front().map_or(0, |(first, _)| {
            usize::try_from(from.saturating_sub(*first - 1)).unwrap_or(usize::MAX)
        });
        let records: Vec<_> = self.records.iter().skip(skip).take(count).collect();
        let cursor = records.last().map_or(from, |(seq, _)| *seq);
        LivePage {
            records: records.into_iter().map(|(_, payload)| payload.clone()).collect(),
            cursor,
            gap,
            more: self.records.back().is_some_and(|(seq, _)| *seq > cursor),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(yaml: &str) -> Config {
        Config::parse(yaml).unwrap()
    }

    #[test]
    fn queues_only_when_management_and_statistics_are_enabled() {
        let q = UsageQueue::default();
        let on = cfg("observability: {usage: {usage-statistics-enabled: true}}\n");
        q.enqueue(b"{}".to_vec());
        assert!(q.pop_oldest(5).is_empty(), "management disabled");
        q.configure(true, &cfg("{}\n"));
        q.enqueue(b"{}".to_vec());
        assert!(q.pop_oldest(5).is_empty(), "statistics default off");
        q.configure(true, &on);
        assert!(
            !q.wants_claude_cache(),
            "usage statistics alone do not enable cache telemetry"
        );
        for i in 0..3 {
            q.enqueue(format!("{{\"n\":{i}}}").into_bytes());
        }
        assert_eq!(q.pop_oldest(2), [b"{\"n\":0}".to_vec(), b"{\"n\":1}".to_vec()]);
        q.configure(false, &on);
        q.configure(true, &on);
        assert!(q.pop_oldest(5).is_empty(), "disabling clears the queue");
        let telemetry = cfg("observability: {usage: {usage-statistics-enabled: true, claude-cache-telemetry: true}}\n");
        q.configure(true, &telemetry);
        assert!(q.wants_claude_cache());
        q.configure(false, &telemetry);
        assert!(!q.wants_claude_cache(), "telemetry also needs an accepting queue");
    }

    #[test]
    fn retention_prunes_expired_records() {
        let q = UsageQueue::default();
        q.configure(
            true,
            &cfg("observability: {usage: {usage-statistics-enabled: true, redis-usage-queue-retention-seconds: 1}}\n"),
        );
        q.lock()
            .push_back((Instant::now() - Duration::from_secs(5), b"old".to_vec()));
        q.enqueue(b"new".to_vec());
        assert_eq!(q.pop_oldest(5), [b"new".to_vec()]);
        q.configure(true, &cfg("observability: {usage: {usage-statistics-enabled: true, redis-usage-queue-retention-seconds: 99999}}\n"));
        assert_eq!(q.retention_seconds.load(Ordering::SeqCst), 3600);
    }

    /// Go redisqueue `Enqueue`, `publishToSubscribers`, `subscribe`, `clear` and
    /// `NotifyUsageRefresh`.
    #[test]
    fn subscribers_take_records_and_are_dropped_when_full_or_disabled() {
        let on = cfg("observability: {usage: {usage-statistics-enabled: true}}\n");
        let q = UsageQueue::default();
        q.configure(true, &on);
        let mut usage = q.subscribe_usage();
        assert_eq!(usage.messages.try_recv().unwrap(), SUPPORT_REFRESH);
        q.enqueue(b"r1".to_vec());
        assert_eq!(usage.messages.try_recv().unwrap(), b"r1");
        assert!(q.pop_oldest(5).is_empty(), "delivered, not queued");
        q.notify_usage_refresh();
        assert_eq!(usage.messages.try_recv().unwrap(), REFRESH);
        drop(usage);
        q.enqueue(b"r2".to_vec());
        assert_eq!(q.pop_oldest(5), [b"r2".to_vec()], "no subscriber: queued again");

        // A subscriber that stops reading is dropped once its buffer is full.
        let mut slow = q.subscribe_usage();
        for i in 0..SUBSCRIBER_BUFFER {
            q.enqueue(format!("{i}").into_bytes());
        }
        assert_eq!(q.usage_subscribers.lock().1.len(), 0, "dropped at the 256th record");
        let mut received = 0;
        while slow.messages.try_recv().is_ok() {
            received += 1;
        }
        assert_eq!(received, SUBSCRIBER_BUFFER, "the greeting and 255 records, then closed");

        // Errors reach only subscribers; disabling the queue ends every subscription.
        let mut errors = q.subscribe_errors();
        q.enqueue_error(b"e1");
        assert_eq!(errors.messages.try_recv().unwrap(), b"e1");
        q.configure(false, &on);
        assert!(matches!(
            errors.messages.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        ));
    }

    fn on() -> UsageQueue {
        let q = UsageQueue::default();
        q.configure(true, &cfg("observability: {usage: {usage-statistics-enabled: true}}\n"));
        q
    }

    fn texts(page: &LivePage) -> Vec<&[u8]> {
        page.records.iter().map(|r| &r[..]).collect()
    }

    /// Live reads copy records without taking them: the queue, RESP subscribers and
    /// every other reader still get each one.
    #[test]
    fn live_reads_never_divert_the_queue_or_subscribers() {
        let q = on();
        let a = q.live_read(None, 10).cursor;
        let b = q.live_read(None, 10).cursor;
        q.enqueue(b"r1".to_vec());
        q.enqueue(b"r2".to_vec());
        let first = q.live_read(Some(a), 10);
        assert_eq!(texts(&first), [b"r1".as_slice(), b"r2"]);
        assert!(!first.gap && !first.more);
        assert_eq!(
            texts(&q.live_read(Some(b), 10)),
            [b"r1".as_slice(), b"r2"],
            "a second reader"
        );
        assert!(q.live_read(Some(first.cursor), 10).records.is_empty());
        assert_eq!(
            q.pop_oldest(10),
            [b"r1".to_vec(), b"r2".to_vec()],
            "the queue still has both"
        );

        // A RESP subscriber takes records off the queue as in Go; the feed still sees them.
        let mut usage = q.subscribe_usage();
        q.enqueue(b"r3".to_vec());
        assert_eq!(usage.messages.try_recv().unwrap(), SUPPORT_REFRESH);
        assert_eq!(usage.messages.try_recv().unwrap(), b"r3");
        assert_eq!(texts(&q.live_read(Some(first.cursor), 10)), [b"r3".as_slice()]);

        // Home puts back an undelivered record without the feed seeing it twice.
        drop(usage);
        let cursor = q.live_read(None, 10).cursor;
        q.requeue(b"r3".to_vec());
        assert!(q.live_read(Some(cursor), 10).records.is_empty());
        assert_eq!(q.pop_oldest(10), [b"r3".to_vec()]);
    }

    #[test]
    fn live_pages_are_bounded_and_resume_from_the_cursor() {
        let q = on();
        let start = q.live_read(None, 10).cursor;
        for i in 0..5 {
            q.enqueue(format!("{i}").into_bytes());
        }
        let page = q.live_read(Some(start), 2);
        assert_eq!(texts(&page), [b"0".as_slice(), b"1"]);
        assert!(page.more);
        let page = q.live_read(Some(page.cursor), 10);
        assert_eq!(texts(&page), [b"2".as_slice(), b"3", b"4"]);
        assert!(!page.more && !page.gap);
    }

    /// A reader that falls behind loses the oldest records and is told so; memory stays
    /// within the record and byte caps whatever it does.
    #[test]
    fn slow_readers_get_a_gap_not_unbounded_memory() {
        let q = on();
        let slow = q.live_read(None, 1).cursor;
        for i in 0..LIVE_MAX_RECORDS + 3 {
            q.enqueue(format!("{i}").into_bytes());
        }
        assert_eq!(q.live_lock().records.len(), LIVE_MAX_RECORDS);
        let page = q.live_read(Some(slow), 1);
        assert!(page.gap);
        assert_eq!(texts(&page), [b"3".as_slice()], "the oldest kept record");
        assert!(!q.live_read(Some(page.cursor), 1).gap, "continuous from there");

        let big = vec![b'x'; LIVE_MAX_BYTES / 4 + 1];
        for _ in 0..8 {
            q.enqueue(big.clone());
        }
        let live = q.live_lock();
        assert!(live.bytes <= LIVE_MAX_BYTES, "{} bytes kept", live.bytes);
        assert_eq!(live.records.len(), 3);
        assert_eq!(live.bytes, live.records.iter().map(|(_, r)| r.len()).sum::<usize>());
    }

    #[test]
    fn unwatched_feed_keeps_nothing_and_reports_the_gap() {
        let q = on();
        q.enqueue(b"before anyone watched".to_vec());
        assert!(q.live_lock().records.is_empty());
        let cursor = q.live_read(None, 10).cursor;
        q.enqueue(b"seen".to_vec());
        // The last read is older than the idle window: the next record empties the feed.
        q.live_lock().read_at = Instant::now().checked_sub(LIVE_IDLE + Duration::from_secs(1));
        q.enqueue(b"unwatched".to_vec());
        assert!(q.live_lock().records.is_empty() && q.live_lock().bytes == 0);
        let page = q.live_read(Some(cursor), 10);
        assert!(page.gap && page.records.is_empty());
        q.enqueue(b"watched again".to_vec());
        let page = q.live_read(Some(page.cursor), 10);
        assert!(!page.gap);
        assert_eq!(texts(&page), [b"watched again".as_slice()]);
        assert_eq!(q.pop_oldest(10).len(), 4, "the queue got every record");
    }

    #[test]
    fn foreign_cursors_and_disabling_report_gaps() {
        let q = on();
        let cursor = q.live_read(None, 10).cursor;
        q.enqueue(b"a".to_vec());
        let page = q.live_read(Some(9_999), 10);
        assert!(page.gap, "a cursor from another process");
        assert_eq!(texts(&page), [b"a".as_slice()]);
        q.configure(false, &cfg("{}\n"));
        q.configure(true, &cfg("observability: {usage: {usage-statistics-enabled: true}}\n"));
        let page = q.live_read(Some(cursor), 10);
        assert!(page.gap && page.records.is_empty(), "disabling empties the feed");
        q.configure(true, &cfg("{}\n"));
        q.enqueue(b"statistics off".to_vec());
        assert!(q.live_read(Some(page.cursor), 10).records.is_empty());
    }
}
