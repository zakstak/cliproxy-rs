//! One config subscriber lifetime (Go `RunConfigSubscriberLifetime`): GET the
//! authoritative config, SUBSCRIBE with the membership arguments, check the ACK, rebuild
//! the command pool and probe it, then receive config and cluster updates until the
//! heartbeat is lost. Reconnection belongs to the caller, which starts each retry on a
//! fresh [`Client::new_lifetime`].

use cpa_core::gostr::GoStr;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::client::{CHANNEL_CLUSTER, CHANNEL_CONFIG, Client, Recovery};
use crate::error::{Error, Result};
use crate::resp::{Conn, Value};

/// A pub/sub reply, as go-redis decodes it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Subscription { kind: String, channel: String, count: i64 },
    Message { channel: String, payload: Vec<u8> },
    Pong,
}

fn text(value: &Value) -> Option<String> {
    match value {
        Value::Bulk(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        Value::Simple(text) => Some(text.clone()),
        _ => None,
    }
}

/// go-redis `PubSub.newMessage`.
pub(crate) fn parse_event(reply: Value) -> Result<Event> {
    let items = match reply {
        Value::Array(items) => items,
        Value::Error(message) => return Err(Error::Server(message)),
        other => {
            return Err(Error::Transport(format!(
                "redis: unsupported pubsub message payload: {other:?}"
            )));
        }
    };
    let kind = items.first().and_then(text).unwrap_or_default();
    let field = |i: usize| items.get(i).and_then(text).unwrap_or_default();
    match kind.as_str() {
        "subscribe" | "unsubscribe" | "psubscribe" | "punsubscribe" | "ssubscribe" | "sunsubscribe" => {
            let count = match items.get(2) {
                Some(Value::Int(n)) => *n,
                _ => 0,
            };
            Ok(Event::Subscription {
                channel: field(1),
                kind,
                count,
            })
        }
        "message" | "smessage" if items.len() == 3 => Ok(Event::Message {
            channel: field(1),
            payload: match &items[2] {
                Value::Bulk(bytes) => bytes.clone(),
                other => text(other).unwrap_or_default().into_bytes(),
            },
        }),
        "pmessage" if items.len() == 4 => Ok(Event::Message {
            channel: field(2),
            payload: match &items[3] {
                Value::Bulk(bytes) => bytes.clone(),
                other => text(other).unwrap_or_default().into_bytes(),
            },
        }),
        "pong" => Ok(Event::Pong),
        _ => Err(Error::Transport(format!("redis: unsupported pubsub message: {kind:?}"))),
    }
}

/// A trimmed payload, as Go's `strings.TrimSpace` sees it.
fn trim_ascii_space(bytes: &[u8]) -> &[u8] {
    bytes.trim_ascii()
}

impl Client {
    /// Reads one event. A reply read at or after `deadline` is a timeout.
    async fn receive(&self, conn: &mut Conn, closed: &CancellationToken, deadline: Instant) -> Result<Event> {
        tokio::select! {
            _ = closed.cancelled() => Err(Error::Transport("redis: client is closed".into())),
            reply = tokio::time::timeout_at(deadline, conn.recv()) => match reply {
                Ok(reply) => {
                    let reply = reply.map_err(Error::from)?;
                    // `timeout_at` polls the read before its timer, so without this check
                    // a peer that always has a frame ready would outrun the deadline.
                    if Instant::now() >= deadline {
                        return Err(Error::Timeout);
                    }
                    parse_event(reply)
                }
                Err(_) => Err(Error::Timeout),
            },
        }
    }

    fn end_lifetime(&self, error: Error) -> Result<()> {
        self.set_heartbeat(false);
        if !self.managed() {
            self.close();
        }
        Err(error)
    }

    /// Runs until the subscription fails or `shutdown` fires. `on_config` receives every
    /// config payload (the initial GET and each published update); an error from the
    /// initial apply ends the lifetime, later errors are logged and ignored. `on_ready`
    /// runs once the subscription is acknowledged and the command pool is live.
    pub async fn run_config_subscriber_lifetime(
        &self,
        shutdown: &CancellationToken,
        mut on_config: impl FnMut(&[u8]) -> std::result::Result<(), String>,
        on_ready: impl FnOnce(),
    ) -> Result<()> {
        if !self.enabled() {
            return Err(Error::Disabled);
        }
        if shutdown.is_cancelled() {
            return self.end_lifetime(Error::Cancelled);
        }
        let live = || !shutdown.is_cancelled();
        self.close_bootstrap_pools();
        if let Err(error) = self.ensure_pools() {
            if live() {
                self.mark_reconnect_failure("connect");
            }
            return self.end_lifetime(error);
        }

        let raw = tokio::select! {
            _ = shutdown.cancelled() => return self.end_lifetime(Error::Cancelled),
            raw = self.get_config() => raw,
        };
        let raw = match raw {
            Ok(raw) => raw,
            Err(error) => {
                if live() {
                    self.mark_reconnect_failure("config fetch");
                }
                return self.end_lifetime(error);
            }
        };
        if let Err(message) = on_config(&raw) {
            return self.end_lifetime(Error::Other(message));
        }

        let pool = match self.subscription_pool() {
            Ok(pool) => pool,
            Err(error) => {
                if live() {
                    self.mark_reconnect_failure("subscribe client");
                }
                return self.end_lifetime(error);
            }
        };
        let (args, receive_timeout) = self.subscription_parameters();
        let subscribed = async {
            let mut conn = pool.checkout().await?;
            let mut command = vec!["subscribe".to_owned()];
            command.extend(args.iter().cloned());
            tokio::time::timeout(self.op_timeout(), conn.send(&command))
                .await
                .map_err(|_| Error::Timeout)??;
            // Home acknowledges only the config channel; the rest are membership args.
            match self
                .receive(&mut conn, pool.closed(), Instant::now() + receive_timeout)
                .await?
            {
                Event::Subscription { kind, channel, count }
                    if kind == "subscribe" && channel == args[0] && count == 1 => {}
                _ => return Err(Error::Other("invalid Home subscription ACK".into())),
            }
            Ok::<_, Error>(conn)
        };
        let conn = tokio::select! {
            _ = shutdown.cancelled() => return self.end_lifetime(Error::Cancelled),
            conn = subscribed => conn,
        };
        let mut conn = match conn {
            Ok(conn) => conn,
            Err(error) => {
                if live() {
                    self.mark_reconnect_failure("subscribe");
                }
                return self.end_lifetime(error);
            }
        };
        // A protocol-one ACK means Home committed this membership already.
        if args.len() > 1 {
            self.mark_takeover_eligible();
        }

        self.promote_subscription();
        let probe = tokio::select! {
            _ = shutdown.cancelled() => return self.end_lifetime(Error::Cancelled),
            probe = self.ping() => probe,
        };
        if let Err(error) = probe {
            if live() {
                self.mark_reconnect_failure("command probe");
            }
            return self.end_lifetime(error);
        }
        self.set_recovery(Recovery::Stable);
        self.reset_reconnect_failures();
        self.set_heartbeat(true);
        on_ready();
        self.follow_subscription(&mut conn, pool.closed(), shutdown, &mut on_config)
            .await
    }

    /// Applies config and cluster updates until the heartbeat is lost, the subscription
    /// fails or `shutdown` fires, then ends the lifetime.
    ///
    /// Liveness has an absolute deadline: the last pong (before the first one, the moment
    /// the lifetime became ready) plus the heartbeat timeout currently configured. Only a
    /// pong moves it. Config, cluster and subscription events do not, so steady pub/sub
    /// traffic cannot hide a Home that stopped sending pongs (Go v8.0.67 `c9dad847`).
    async fn follow_subscription(
        &self,
        conn: &mut Conn,
        closed: &CancellationToken,
        shutdown: &CancellationToken,
        on_config: &mut impl FnMut(&[u8]) -> std::result::Result<(), String>,
    ) -> Result<()> {
        let mut last_pong = Instant::now();
        loop {
            // Read every turn: a config update that changes the heartbeat timeout moves
            // the current deadline without renewing it.
            let (_, heartbeat_timeout) = self.subscription_parameters();
            let event = tokio::select! {
                _ = shutdown.cancelled() => return self.end_lifetime(Error::Cancelled),
                event = self.receive(conn, closed, last_pong + heartbeat_timeout) => event,
            };
            let event = match event {
                Ok(event) => event,
                Err(error) => {
                    if !shutdown.is_cancelled() {
                        if self.heartbeat_ok() {
                            self.mark_takeover_eligible();
                        }
                        if error.is_timeout() {
                            self.mark_subscription_timeout();
                        } else {
                            self.mark_reconnect_failure("subscription");
                        }
                    }
                    return self.end_lifetime(error);
                }
            };
            match event {
                Event::Message { channel, payload } => {
                    let payload = trim_ascii_space(&payload);
                    if payload.is_empty() {
                        continue;
                    }
                    let applied = match channel.trim().go_lower().as_str() {
                        CHANNEL_CONFIG => on_config(payload).is_ok(),
                        CHANNEL_CLUSTER => self.update_cluster_nodes(payload).is_ok(),
                        _ => true,
                    };
                    if !applied {
                        let kind = if channel.trim().eq_ignore_ascii_case(CHANNEL_CLUSTER) {
                            "cluster"
                        } else {
                            "config"
                        };
                        tracing::warn!("failed to apply {kind} update from home control center, ignoring");
                    }
                }
                Event::Pong => {
                    last_pong = Instant::now();
                    self.reset_reconnect_failures();
                }
                Event::Subscription { .. } => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::io::{AsyncWriteExt, DuplexStream};

    use super::*;
    use crate::config::{CredentialConcurrency, HomeConfig};
    use crate::{fake, resp};

    fn bulk(s: &str) -> Value {
        Value::Bulk(s.as_bytes().to_vec())
    }

    #[test]
    fn decodes_go_redis_pubsub_shapes() {
        assert_eq!(
            parse_event(Value::Array(vec![bulk("subscribe"), bulk("config"), Value::Int(1)])).unwrap(),
            Event::Subscription {
                kind: "subscribe".into(),
                channel: "config".into(),
                count: 1
            }
        );
        assert_eq!(
            parse_event(Value::Array(vec![bulk("message"), bulk("cluster"), bulk("{}")])).unwrap(),
            Event::Message {
                channel: "cluster".into(),
                payload: b"{}".to_vec()
            }
        );
        assert_eq!(
            parse_event(Value::Array(vec![bulk("pong"), bulk("")])).unwrap(),
            Event::Pong
        );
        assert_eq!(
            parse_event(Value::Error(
                "ERR wrong number of arguments for 'subscribe' command".into()
            ))
            .unwrap_err(),
            Error::Server("ERR wrong number of arguments for 'subscribe' command".into())
        );
        assert!(parse_event(Value::Simple("OK".into())).is_err());
        assert!(parse_event(Value::Array(vec![bulk("message"), bulk("only-two")])).is_err());
    }

    const SECOND: i64 = 1_000_000_000;

    fn set_heartbeat_timeout(client: &Client, seconds: i64) {
        client
            .set_lifecycle_config(CredentialConcurrency {
                cpa_heartbeat_timeout: seconds * SECOND,
                ..Default::default()
            })
            .unwrap();
    }

    /// A client that never dials: these tests hand it the subscription directly.
    /// Discovery stays on so reconnect failures are counted. With no other node known,
    /// a failure never switches the target.
    fn lifetime(heartbeat_seconds: i64) -> Client {
        let client = Client::with_options(
            HomeConfig {
                enabled: true,
                node_id: "node-1".into(),
                host: "127.0.0.1".into(),
                port: 1,
                disable_cluster_discovery: false,
                ..HomeConfig::default()
            },
            Duration::from_millis(300),
            "11111111-2222-3333-4444-555555555555".into(),
        );
        set_heartbeat_timeout(&client, heartbeat_seconds);
        client
    }

    /// A ready lifetime following a subscription that is one end of an in-memory pipe,
    /// so the paused test clock decides every deadline.
    struct Following {
        home: DuplexStream,
        closed: CancellationToken,
        shutdown: CancellationToken,
        applied: Arc<AtomicUsize>,
        /// The lifetime's result and the time it ended.
        run: tokio::task::JoinHandle<(Result<()>, Instant)>,
    }

    impl Following {
        /// Writes frames as Home would. Errors mean the lifetime already ended.
        async fn send(&mut self, frames: &[Vec<u8>]) {
            for frame in frames {
                let _ = self.home.write_all(frame).await;
            }
        }

        async fn end(self) -> (Result<()>, Instant) {
            self.run.await.unwrap()
        }
    }

    /// Marks `client` ready and follows the subscription. `on_config` sees every config
    /// payload.
    fn follow(client: &Client, mut on_config: impl FnMut(&[u8]) + Send + 'static) -> Following {
        let (local, home) = tokio::io::duplex(1 << 20);
        let (closed, shutdown) = (CancellationToken::new(), CancellationToken::new());
        let applied = Arc::new(AtomicUsize::new(0));
        client.set_heartbeat(true);
        let run = tokio::spawn({
            let (client, closed, shutdown, applied) =
                (client.clone(), closed.clone(), shutdown.clone(), applied.clone());
            async move {
                let mut conn = Conn::new(Box::new(local));
                let mut apply = |raw: &[u8]| {
                    applied.fetch_add(1, Ordering::SeqCst);
                    on_config(raw);
                    Ok(())
                };
                let result = client
                    .follow_subscription(&mut conn, &closed, &shutdown, &mut apply)
                    .await;
                (result, Instant::now())
            }
        });
        Following {
            home,
            closed,
            shutdown,
            applied,
            run,
        }
    }

    /// Lets every task that is ready now run until it waits again.
    async fn settle() {
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
    }

    fn traffic() -> Vec<Vec<u8>> {
        vec![
            fake::message("config", b"port: 2\n"),
            fake::message("cluster", br#"{"ok":true,"nodes":[]}"#),
            resp::encode(&["subscribe", "config"]),
        ]
    }

    /// Config, cluster and subscription events every second do not renew a 3s heartbeat.
    /// A managed lifetime ends at the deadline without closing its client, keeps its
    /// membership eligible for takeover and applies no update read after the deadline.
    #[tokio::test(start_paused = true)]
    async fn ordinary_traffic_does_not_renew_the_heartbeat() {
        let client = lifetime(3);
        client.set_managed_lifetime(true);
        let start = Instant::now();
        let mut following = follow(&client, |_| {});
        for second in 1..=10 {
            tokio::time::sleep_until(start + Duration::from_secs(second)).await;
            if following.run.is_finished() {
                break;
            }
            following.send(&traffic()).await;
        }
        let applied = following.applied.clone();
        let (result, ended) = following.end().await;
        assert!(result.as_ref().is_err_and(Error::is_timeout), "{result:?}");
        assert_eq!(ended - start, Duration::from_secs(3));
        assert_eq!(applied.load(Ordering::SeqCst), 2, "updates read at 1s and 2s");
        assert!(!client.heartbeat_ok());
        assert!(!client.fenced());
        assert_eq!(client.recovery(), Recovery::TakeoverEligible);
    }

    /// Frames that are already buffered when the deadline passes do not win against it,
    /// even though `timeout_at` polls the read before its timer.
    #[tokio::test(start_paused = true)]
    async fn frames_that_are_always_ready_cannot_outrun_an_expired_deadline() {
        let client = lifetime(3);
        client.set_managed_lifetime(true);
        let start = Instant::now();
        let mut following = follow(&client, |_| {});
        settle().await;
        // Queue the frames in one write, so the test task does not yield to the
        // subscriber, then move the clock past the deadline before the subscriber runs
        // again: its read is ready the moment it is polled.
        following.send(&[traffic().concat().repeat(64)]).await;
        tokio::time::advance(Duration::from_secs(4)).await;
        let applied = following.applied.clone();
        let (result, ended) = following.end().await;
        assert!(result.as_ref().is_err_and(Error::is_timeout), "{result:?}");
        assert_eq!(ended - start, Duration::from_secs(4));
        assert_eq!(applied.load(Ordering::SeqCst), 0, "no update read after the deadline");
        assert!(!client.heartbeat_ok());
    }

    /// Each pong renews the deadline from the time it is read and resets reconnect
    /// failures.
    #[tokio::test(start_paused = true)]
    async fn a_pong_renews_the_heartbeat_and_resets_reconnect_failures() {
        let client = lifetime(3);
        client.set_managed_lifetime(true);
        client.mark_reconnect_failure("test");
        assert_eq!(client.reconnect_failures(), 1);
        let start = Instant::now();
        let mut following = follow(&client, |_| {});
        for second in [2, 4, 6] {
            tokio::time::sleep_until(start + Duration::from_secs(second)).await;
            following.send(&traffic()).await;
            following.send(&[fake::pong()]).await;
        }
        settle().await;
        assert_eq!(client.reconnect_failures(), 0);
        assert!(client.heartbeat_ok());
        let (result, ended) = following.end().await;
        assert!(result.as_ref().is_err_and(Error::is_timeout), "{result:?}");
        assert_eq!(ended - start, Duration::from_secs(9), "the last pong at 6s plus 3s");
    }

    /// A config update that changes the heartbeat timeout applies to the current
    /// deadline, which still counts from the last pong (here: the ready time).
    #[tokio::test(start_paused = true)]
    async fn a_heartbeat_timeout_update_moves_the_deadline_without_renewing_it() {
        let client = lifetime(3);
        client.set_managed_lifetime(true);
        let start = Instant::now();
        let mut following = follow(&client, {
            let client = client.clone();
            move |_| set_heartbeat_timeout(&client, 10)
        });
        tokio::time::sleep(Duration::from_secs(1)).await;
        following.send(&[fake::message("config", b"port: 2\n")]).await;
        let applied = following.applied.clone();
        let (result, ended) = following.end().await;
        assert!(result.as_ref().is_err_and(Error::is_timeout), "{result:?}");
        assert_eq!(applied.load(Ordering::SeqCst), 1);
        assert_eq!(ended - start, Duration::from_secs(10), "not 3s, and not 1s + 10s");
    }

    /// A frame the parser rejects ends an unmanaged lifetime as a reconnect failure,
    /// not a heartbeat timeout, and the lifetime closes its own client.
    #[tokio::test(start_paused = true)]
    async fn an_unsupported_frame_is_a_reconnect_failure() {
        let client = lifetime(3);
        let mut following = follow(&client, |_| {});
        following.send(&[b"+OK\r\n".to_vec()]).await;
        let (result, _) = following.end().await;
        let error = result.unwrap_err();
        assert!(!error.is_timeout(), "{error:?}");
        assert_eq!(client.reconnect_failures(), 1);
        assert_eq!(client.recovery(), Recovery::TakeoverEligible);
        assert!(!client.heartbeat_ok());
        assert!(client.fenced());
    }

    /// A closed pool is a reconnect failure; shutdown ends the lifetime without one.
    #[tokio::test(start_paused = true)]
    async fn a_closed_pool_counts_a_failure_and_shutdown_does_not() {
        let client = lifetime(3);
        let following = follow(&client, |_| {});
        settle().await;
        following.closed.cancel();
        let (result, _) = following.end().await;
        assert_eq!(result.unwrap_err().to_string(), "redis: client is closed");
        assert_eq!(client.reconnect_failures(), 1);
        assert!(!client.heartbeat_ok());
        assert!(client.fenced());

        let client = lifetime(3);
        client.set_managed_lifetime(true);
        let following = follow(&client, |_| {});
        settle().await;
        following.shutdown.cancel();
        let (result, _) = following.end().await;
        assert_eq!(result.unwrap_err(), Error::Cancelled);
        assert_eq!(client.reconnect_failures(), 0);
        assert_eq!(client.recovery(), Recovery::Stable);
        assert!(!client.heartbeat_ok());
        assert!(!client.fenced());
    }
}
