//! Go internal/logging/home_app_log_forwarder.go: process-log lines forwarded to
//! Home (`RPUSH` app log) while a healthy Home lifetime owns the forwarder.
//!
//! The process logger calls [`Forwarder::fire`] for every line it writes (Go's
//! logrus hook). Lines queue without blocking (1024 by default, dropped when full)
//! and one task pushes them. A Home that rejects the key or command disables
//! forwarding until the next [`Forwarder::bind`].

use crate::text::GoStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use chrono::{DateTime, FixedOffset};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::home::Client;
use crate::protocol::json_output::Object;

/// Go `defaultHomeAppLogQueueSize`.
const DEFAULT_QUEUE_SIZE: usize = 1024;

/// Go `homeAppLogPayload`, with the client that was current when it was queued.
struct Pending {
    line: String,
    level: &'static str,
    timestamp: String,
    request_id: String,
    client: Client,
}

impl Pending {
    /// Go `json.Marshal(&homeAppLogPayload)`.
    fn json(&self) -> Vec<u8> {
        Object::new()
            .str("line", &self.line)
            .str_opt("level", self.level)
            .str_opt("timestamp", &self.timestamp)
            .str_opt("request_id", &self.request_id)
            .finish()
    }
}

/// Go `HomeAppLogForwarder`.
pub struct Forwarder {
    queue: mpsc::Sender<Pending>,
    enabled: AtomicBool,
    stopped: AtomicBool,
    owner: Mutex<Option<Client>>,
    stop: CancellationToken,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Forwarder {
    /// Go `StartHomeAppLogForwarder`: enabled, unbound, with its sender running.
    pub fn start(queue_size: usize) -> Arc<Self> {
        let (queue, mut rx) = mpsc::channel(if queue_size == 0 {
            DEFAULT_QUEUE_SIZE
        } else {
            queue_size
        });
        let forwarder = Arc::new(Self {
            queue,
            enabled: AtomicBool::new(true),
            stopped: AtomicBool::new(false),
            owner: Mutex::new(None),
            stop: CancellationToken::new(),
            task: Mutex::new(None),
        });
        let task = tokio::spawn({
            let forwarder = forwarder.clone();
            async move {
                loop {
                    tokio::select! {
                        _ = forwarder.stop.cancelled() => return,
                        pending = rx.recv() => match pending {
                            Some(pending) => forwarder.forward(pending).await,
                            None => return,
                        },
                    }
                }
            }
        });
        *forwarder.task.lock().unwrap_or_else(PoisonError::into_inner) = Some(task);
        forwarder
    }

    fn owner(&self) -> Option<Client> {
        self.owner.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Go `Bind`: forwards to `client` from now on, re-enabling a forwarder an
    /// unsupported Home disabled.
    pub fn bind(&self, client: &Client) {
        let mut owner = self.owner.lock().unwrap_or_else(PoisonError::into_inner);
        if self.stopped.load(Ordering::SeqCst) {
            return;
        }
        *owner = Some(client.clone());
        self.enabled.store(true, Ordering::SeqCst);
    }

    /// Go `Deactivate`: unbinds only when `client` is the owner.
    pub fn deactivate(&self, client: &Client) {
        let mut owner = self.owner.lock().unwrap_or_else(PoisonError::into_inner);
        if owner.as_ref().is_some_and(|o| o.ptr_eq(client)) {
            *owner = None;
        }
    }

    /// Go `Stop`: disables forwarding and waits for the sender; queued lines are
    /// dropped.
    pub async fn stop(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        *self.owner.lock().unwrap_or_else(PoisonError::into_inner) = None;
        self.enabled.store(false, Ordering::SeqCst);
        self.stop.cancel();
        let task = self.task.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }

    /// Go `Fire`: queues one formatted line when a healthy owner is bound. `level`
    /// is logrus `Level.String()`, `time` the entry time, `request_id` the raw
    /// `request_id` field.
    pub fn fire(&self, line: &str, level: &'static str, time: DateTime<FixedOffset>, request_id: &str) {
        if !self.enabled.load(Ordering::SeqCst) {
            return;
        }
        let Some(client) = self.owner().filter(Client::heartbeat_ok) else {
            return;
        };
        if line.trim().is_empty() {
            return;
        }
        // Go `appLogRequestID`.
        let request_id = match request_id.trim() {
            "--------" => "",
            id => id,
        };
        let _ = self.queue.try_send(Pending {
            line: line.to_owned(),
            level,
            timestamp: rfc3339_nano(time),
            request_id: request_id.to_owned(),
            client,
        });
    }

    /// Go `forward`: only for the client that is still the owner.
    async fn forward(&self, pending: Pending) {
        let current = self.owner().is_some_and(|o| o.ptr_eq(&pending.client));
        if !self.enabled.load(Ordering::SeqCst) || !current || !pending.client.heartbeat_ok() {
            return;
        }
        if let Err(error) = pending.client.rpush_app_log(&pending.json()).await
            && unsupported(&error.to_string())
        {
            // Go `disableIfCurrentOwner`.
            let owner = self.owner.lock().unwrap_or_else(PoisonError::into_inner);
            if owner.as_ref().is_some_and(|o| o.ptr_eq(&pending.client)) {
                self.enabled.store(false, Ordering::SeqCst);
            }
        }
    }
}

/// Go `isHomeAppLogUnsupported`.
fn unsupported(message: &str) -> bool {
    let message = message.trim().go_lower();
    ["unsupported key", "unknown command", "unsupported command"]
        .iter()
        .any(|needle| message.contains(needle))
}

/// Go `time.Time.Format(time.RFC3339Nano)`: trimmed nanoseconds, `Z` for UTC.
fn rfc3339_nano(t: DateTime<FixedOffset>) -> String {
    let text = t.format("%Y-%m-%dT%H:%M:%S%.9f").to_string();
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if t.offset().local_minus_utc() == 0 {
        format!("{text}Z")
    } else {
        format!("{text}{}", t.format("%:z"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::fake::{self, FakeHome};
    use serde_json::Value;
    use std::time::Duration;

    async fn pushed(home: &FakeHome, count: usize) -> Vec<String> {
        for _ in 0..200 {
            let pushes: Vec<String> = home
                .commands()
                .into_iter()
                .filter(|c| c[0].eq_ignore_ascii_case("rpush"))
                .map(|c| c[2].clone())
                .collect();
            if pushes.len() >= count {
                return pushes;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("expected {count} app-log pushes, got {:?}", home.commands());
    }

    /// A Home whose heartbeat is up, answering every push with `reply`.
    async fn home(reply: &'static str) -> (FakeHome, Client) {
        let home = FakeHome::start(move |args| match args[0].to_ascii_lowercase().as_str() {
            "rpush" => fake::raw(reply),
            _ => fake::raw("+PONG\r\n"),
        })
        .await;
        let client = home.client();
        client.set_heartbeat(true);
        (home, client)
    }

    /// Log forwarding preserves the event instant and safe optional metadata.
    #[tokio::test]
    async fn app_log_payloads_preserve_line_level_instant_and_optional_request_id() {
        let (home, client) = home(":1\r\n").await;
        let forwarder = Forwarder::start(16);
        forwarder.bind(&client);
        let entries = [
            ("line <one>&\n", "info", "2026-10-03T09:01:02Z", "request-1"),
            (
                "warning\n",
                "warning",
                "2026-10-03T11:01:02.123456789+02:00",
                " request-2 ",
            ),
            ("error\n", "error", "2026-10-03T09:01:02.12Z", "--------"),
            ("debug\n", "debug", "2026-10-03T09:01:02Z", ""),
            ("trace\n", "trace", "2026-10-03T04:01:02-05:00", "trace-id"),
        ];
        for (line, level, timestamp, request_id) in entries {
            forwarder.fire(
                line,
                level,
                DateTime::parse_from_rfc3339(timestamp).unwrap(),
                request_id,
            );
        }
        let pushed = pushed(&home, entries.len()).await;
        for (raw, (line, level, timestamp, request_id)) in pushed.iter().zip(entries) {
            let actual: Value = serde_json::from_str(raw).unwrap();
            assert_eq!(actual["line"], line);
            assert_eq!(actual["level"], level);
            assert_eq!(
                DateTime::parse_from_rfc3339(actual["timestamp"].as_str().unwrap()).unwrap(),
                DateTime::parse_from_rfc3339(timestamp).unwrap()
            );
            if request_id.trim().is_empty() || request_id == "--------" {
                assert!(actual.get("request_id").is_none());
            } else {
                assert_eq!(actual["request_id"], request_id.trim());
            }
        }
        assert!(
            home.commands()
                .iter()
                .filter(|c| c[0].eq_ignore_ascii_case("rpush"))
                .all(|c| c[1] == "app-log")
        );
        forwarder.stop().await;
    }
    #[test]
    fn protocol_capability_errors_disable_forwarding_but_operational_errors_do_not() {
        for detail in [
            "ERR Unsupported Key: app-log",
            "ERR unknown command 'RPUSH'",
            "ERR unsupported command RPUSH",
            "  UNKNOWN COMMAND  ",
        ] {
            assert!(unsupported(detail), "{detail}");
        }
        for detail in ["", "ERR denied", "connection refused", "request timed out"] {
            assert!(!unsupported(detail), "{detail}");
        }
    }
    /// Go `TestHomeAppLogForwarder_RebindsOnlyToCurrentOwner`,
    /// `TestHomeAppLogForwarder_SkipsWhenBoundOwnerHeartbeatIsDown`,
    /// `TestHomeAppLogForwarder_UnboundNeverUsesGlobalFallbackClient` and
    /// `TestHomeAppLogForwarder_DropsPreACKAndReconnectGapLogs`: nothing is queued
    /// while unbound or unhealthy, and only the bound owner unbinds itself.
    #[tokio::test]
    async fn only_a_healthy_owner_receives_lines() {
        let (home, client) = home(":1\r\n").await;
        let now = DateTime::parse_from_rfc3339("2026-10-03T09:01:02Z").unwrap();
        let forwarder = Forwarder::start(0);
        forwarder.fire("unbound\n", "info", now, "");
        forwarder.bind(&client);
        client.set_heartbeat(false);
        forwarder.fire("unhealthy\n", "info", now, "");
        client.set_heartbeat(true);
        forwarder.fire("  \n", "info", now, "");
        // Another lifetime's client never unbinds the owner.
        let other = home.client();
        forwarder.deactivate(&other);
        forwarder.fire("bound\n", "info", now, "");
        let got = pushed(&home, 1).await;
        assert_eq!(got.len(), 1);
        assert!(got[0].starts_with(r#"{"line":"bound\n""#), "{got:?}");
        forwarder.deactivate(&client);
        forwarder.fire("deactivated\n", "info", now, "");
        // A queued line for an owner that changed before it was sent is dropped.
        forwarder.bind(&client);
        forwarder.stop().await;
        forwarder.fire("stopped\n", "info", now, "");
        forwarder.bind(&client);
        forwarder.fire("bound after stop\n", "info", now, "");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(pushed(&home, 1).await.len(), 1);
    }

    /// Go `TestHomeAppLogForwarder_DisablesForwardingWhenBoundOwnerDoesNotSupportAppLog`.
    #[tokio::test]
    async fn an_unsupported_home_disables_until_rebound() {
        let (home, client) = home("-ERR unsupported key: cpa:app-log\r\n").await;
        let now = DateTime::parse_from_rfc3339("2026-10-03T09:01:02Z").unwrap();
        let forwarder = Forwarder::start(0);
        forwarder.bind(&client);
        forwarder.fire("first\n", "info", now, "");
        pushed(&home, 1).await;
        // The sender disabled forwarding; later lines are not queued.
        for _ in 0..100 {
            if !forwarder.enabled.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        forwarder.fire("second\n", "info", now, "");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(pushed(&home, 1).await.len(), 1);
        forwarder.bind(&client);
        forwarder.fire("third\n", "info", now, "");
        assert_eq!(pushed(&home, 2).await.len(), 2);
        forwarder.stop().await;
    }

    /// Go `TestHomeAppLogForwarder_DelayedOldOwnerUnsupportedDoesNotDisableNewOwner`: an
    /// unsupported reply from an owner replaced meanwhile leaves forwarding on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_late_unsupported_reply_from_an_old_owner_keeps_forwarding() {
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (started_tx, release_rx) = (Mutex::new(started_tx), Mutex::new(release_rx));
        let old = FakeHome::start(move |args| match args[0].to_ascii_lowercase().as_str() {
            "rpush" => {
                let _ = started_tx.lock().unwrap().send(());
                let _ = release_rx.lock().unwrap().recv();
                fake::raw("-ERR unsupported key\r\n")
            }
            _ => fake::raw("+PONG\r\n"),
        })
        .await;
        let old_client = old.client();
        old_client.set_heartbeat(true);
        let (new, new_client) = home(":1\r\n").await;
        let now = DateTime::parse_from_rfc3339("2026-10-03T09:01:02Z").unwrap();
        let forwarder = Forwarder::start(1);
        forwarder.bind(&old_client);
        forwarder.fire("old owner\n", "info", now, "");
        tokio::task::spawn_blocking(move || started_rx.recv_timeout(Duration::from_secs(5)))
            .await
            .unwrap()
            .expect("the old owner's push started");
        forwarder.bind(&new_client);
        release_tx.send(()).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(forwarder.enabled.load(Ordering::SeqCst), "the new owner stays enabled");
        forwarder.fire("new owner\n", "info", now, "");
        let got = pushed(&new, 1).await;
        assert!(got[0].starts_with(r#"{"line":"new owner\n""#), "{got:?}");
        forwarder.stop().await;
    }

    #[tokio::test]
    async fn other_push_errors_keep_forwarding() {
        let (home, client) = home("-ERR wrong number of arguments for 'rpush' command\r\n").await;
        let now = DateTime::parse_from_rfc3339("2026-10-03T09:01:02Z").unwrap();
        let forwarder = Forwarder::start(0);
        forwarder.bind(&client);
        forwarder.fire("first\n", "info", now, "");
        pushed(&home, 1).await;
        forwarder.fire("second\n", "info", now, "");
        assert_eq!(pushed(&home, 2).await.len(), 2);
        forwarder.stop().await;
    }
}
