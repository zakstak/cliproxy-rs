//! Upstream WebSocket observations (Go `EmitWebSocketResponseEvent`) that keep their
//! turn: Go calls the observer synchronously while its executor holds the session lock,
//! so a turn abandoned mid-observation releases its session only once the observer
//! returns.

use futures_util::FutureExt as _;
use futures_util::future::{BoxFuture, Shared};

pub(crate) type Observation = Shared<BoxFuture<'static, ()>>;

/// Awaits `observation`, keeping it in `slot` meanwhile for [`release_after`].
/// A read cancelled mid-observation leaves the slot set; turns end on such a drop and
/// never start another read.
pub(crate) async fn observe(slot: &mut Option<Observation>, observation: Option<BoxFuture<'static, ()>>) {
    let Some(observation) = observation else {
        return;
    };
    let observation = observation.shared();
    *slot = Some(observation.clone());
    observation.await;
    *slot = None;
}

/// Runs `release` and drops `guard` now, or once a running `observation` returned.
pub(crate) fn release_after<G: Send + 'static>(
    observation: Option<Observation>,
    guard: G,
    release: impl FnOnce() + Send + 'static,
) {
    match (observation, tokio::runtime::Handle::try_current()) {
        (Some(observation), Ok(runtime)) => {
            runtime.spawn(async move {
                observation.await;
                release();
                drop(guard);
            });
        }
        // Without a runtime (no caller drops a turn there) nothing can wait for the
        // observation, so the session is released at once.
        _ => {
            release();
            drop(guard);
        }
    }
}
