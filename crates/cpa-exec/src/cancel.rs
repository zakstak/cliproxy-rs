//! The usage outcome of an attempt the client abandons. When the client goes away, the
//! server drops the attempt's pending future or response stream, so none of the
//! executor's error paths run, and the server's tracker records an attempt that ends
//! without a settled outcome as a success. These guards settle the failure first: the
//! tracker's first-wins settlement then publishes it, once, when the attempt's record
//! ends.
//!
//! The record says `context canceled` with status 499: the proxy's convention for a
//! client that left (Go records its canceled request context the same way). It is
//! internal accounting only. Nothing reaches the client or the upstream, and the
//! scheduler, failover and cooldowns never see it, because the attempt has no consumer
//! left to classify a failure.

use cpa_core::exec::{ExecError, UsageSink};

/// The recorded status of an abandoned attempt.
pub(crate) const STATUS: u16 = 499;
/// The recorded failure text of an abandoned attempt. It names no method or URL, which
/// Go's text adds when the cancellation hit the send: a base URL can carry credentials
/// (docs/DIFFERENCES-FROM-GO.md).
pub(crate) const BODY: &str = "context canceled";

/// Awaits `op`, recording the attempt as canceled if `op` is dropped before it settles.
/// What `op` returns is the caller's to settle.
pub(crate) async fn fail_if_canceled<F: std::future::Future>(usage: &UsageSink, op: F) -> F::Output {
    struct Pending<'a>(Option<&'a UsageSink>);
    impl Drop for Pending<'_> {
        fn drop(&mut self) {
            if let Some(usage) = self.0 {
                usage.publish_failure(STATUS, BODY);
            }
        }
    }
    let mut pending = Pending(Some(usage));
    let out = op.await;
    pending.0 = None;
    out
}

/// The usage outcome of a response stream if the consumer drops it before taking the
/// item that ends it. Held by the stream's state, so it covers a drop between polls (a
/// client that leaves while the server writes an earlier event) as well as one during
/// an upstream read.
pub(crate) struct StreamOutcome {
    usage: UsageSink,
    on_drop: OnDrop,
}

enum OnDrop {
    /// Nothing ended the upstream response yet: the client canceled it.
    Canceled,
    /// The upstream failed and the failure waits in the stream for the consumer.
    Failed(Box<ExecError>),
    /// The upstream completed, or the consumer took the failure and settles it.
    Settled,
}

impl StreamOutcome {
    pub(crate) fn new(usage: UsageSink) -> Self {
        Self {
            usage,
            on_drop: OnDrop::Canceled,
        }
    }

    /// The upstream completed (its terminal event was observed) or the consumer took
    /// the item that ends the stream: a later drop leaves the record to the tracker,
    /// which has the usage observed so far.
    pub(crate) fn settled(&mut self) {
        self.on_drop = OnDrop::Settled;
    }

    /// The stream failed with `error` (the upstream's failure, or a failure to translate
    /// the upstream's response), now queued for the consumer: a drop before the consumer
    /// takes it records this failure, not a cancellation.
    pub(crate) fn failed(&mut self, error: &ExecError) {
        self.on_drop = OnDrop::Failed(Box::new(error.clone()));
    }
}

impl Drop for StreamOutcome {
    fn drop(&mut self) {
        match &self.on_drop {
            OnDrop::Canceled => self.usage.publish_failure(STATUS, BODY),
            OnDrop::Failed(error) => crate::openai_http::publish_failure(&self.usage, error),
            OnDrop::Settled => {}
        }
    }
}
