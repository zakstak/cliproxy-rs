//! Home picks kept by pooled upstream sockets (Go `HomeDispatchSelection` as an
//! `ExecutionLifecycle`, `retainHomeWebsocketSelection` and
//! `retainedHomeSessionSelection`).
//!
//! A downstream WebSocket turn on a Home credential hands its pick to the executor as
//! a [`Pick`]. When the executor keeps the upstream socket for later turns it retains
//! the pick, and after the turn succeeds the pick takes the lease over from the
//! attempt: the credential stays leased while the socket is open. The pick ends, and
//! the lease is released, when the socket is invalidated, replaced or closed, when
//! Home drains it (the socket closes first), or when the downstream connection ends.
//! The next turn of the same session runs on a still-active pick instead of asking
//! Home again.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError, Weak};

use cpa_core::credential::Credential;
use cpa_core::exec::{Retainable, SessionLease};

use crate::remote::Availability;
use crate::runtime::Lease;

/// One attempt's Home pick as the executor sees it.
#[derive(Default)]
pub(crate) struct Pick(Mutex<State>);

#[derive(Default)]
struct State {
    retained: bool,
    ended: bool,
    /// Closes the socket that retained the pick.
    close: Option<Box<dyn FnOnce() + Send>>,
    /// Held once the turn succeeded on a retaining socket.
    lease: Option<Lease>,
    /// Closes the pick when Home drains its lease.
    watcher: Option<tokio::task::AbortHandle>,
    /// Forgets the session's view of the credential when the pick ends.
    forget: Option<Box<dyn FnOnce() + Send>>,
}

impl Retainable for Pick {
    fn retain(&self, close: Box<dyn FnOnce() + Send>) -> bool {
        let mut state = self.state();
        if state.ended {
            return false;
        }
        state.retained = true;
        state.close = Some(close);
        true
    }

    fn end(&self) {
        let (lease, watcher, forget) = {
            let mut state = self.state();
            state.ended = true;
            state.close = None;
            (state.lease.take(), state.watcher.take(), state.forget.take())
        };
        if let Some(watcher) = watcher {
            watcher.abort();
        }
        if let Some(forget) = forget {
            forget();
        }
        drop(lease);
    }
}

impl Pick {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn lease(self: &Arc<Self>) -> SessionLease {
        SessionLease(self.clone())
    }

    /// After a successful turn: the pick keeps `lease` when a socket retained it and
    /// still holds it. Otherwise the lease comes back to the attempt.
    pub(crate) fn keep(self: &Arc<Self>, lease: Lease) -> Option<Lease> {
        let drained = lease.remote_cancelled();
        let mut state = self.state();
        if !state.retained || state.ended {
            return Some(lease);
        }
        state.lease = Some(lease);
        if let Some(drained) = drained {
            let pick: Weak<Self> = Arc::downgrade(self);
            let watcher = tokio::spawn(async move {
                drained.await;
                if let Some(pick) = pick.upgrade() {
                    pick.close();
                }
            });
            state.watcher = Some(watcher.abort_handle());
        }
        None
    }

    /// Go `End` from the request side: closes the retaining socket, then releases the
    /// lease. Idempotent.
    pub(crate) fn close(&self) {
        let (close, lease, watcher, forget) = {
            let mut state = self.state();
            state.ended = true;
            (
                state.close.take(),
                state.lease.take(),
                state.watcher.take(),
                state.forget.take(),
            )
        };
        if let Some(close) = close {
            close();
        }
        if let Some(forget) = forget {
            forget();
        }
        drop(lease);
        if let Some(watcher) = watcher {
            watcher.abort();
        }
    }

    fn release_into(&self, releases: &crate::remote::PendingReleases) {
        if let Some(lease) = self.state().lease.as_mut() {
            lease.release_into(releases);
        }
    }

    /// Lends the kept lease to the session's next turn, which runs on this same pick:
    /// the socket stays bound to it.
    fn lend(&self) -> Option<Lease> {
        let (lease, watcher) = {
            let mut state = self.state();
            if state.ended
                || !state
                    .lease
                    .as_ref()
                    .is_some_and(|lease| !lease.remote_cancel_requested())
            {
                return None;
            }
            (state.lease.take(), state.watcher.take())
        };
        if let Some(watcher) = watcher {
            watcher.abort();
        }
        lease
    }
}

/// Closes a pick unless the turn handed its lease to it; dropped before the attempt's
/// lease, so a socket closes before its credential is released.
pub(crate) struct PickGuard(Option<Arc<Pick>>);

impl PickGuard {
    pub(crate) fn new(pick: Arc<Pick>) -> Self {
        Self(Some(pick))
    }

    /// The pick holds the lease now; it no longer ends with the attempt.
    pub(crate) fn defuse(&mut self) -> Option<Arc<Pick>> {
        self.0.take()
    }
}

impl Drop for PickGuard {
    fn drop(&mut self) {
        if let Some(pick) = self.0.take() {
            pick.close();
        }
    }
}

struct Kept {
    pick: Arc<Pick>,
    credential: String,
    /// Canonical route model.
    route: String,
    /// The client key Home authenticated when it granted the pick.
    user_key: String,
    /// Home's request-retry limit with the pick (Go `selection.requestRetry`).
    request_retry: Option<i64>,
    /// The lease is out with the next turn's attempt.
    lent: bool,
}

/// The pick a downstream WebSocket connection keeps between turns (Go
/// `homeSessionSelections` for one session).
#[derive(Default)]
pub struct SessionHome {
    kept: Mutex<Option<Kept>>,
    /// Home's request-retry limit with the session's latest fresh pick.
    granted_retry: Mutex<Option<i64>>,
    /// The Home credentials this session's kept picks hold, by ID, each with the pick
    /// that owns the entry (Go `homeRuntimeAuths` and `homeRuntimeAuthOwners` for one
    /// session): the turn loop's view of its pin, which the local store does not hold.
    /// An entry goes when its owning pick ends.
    auths: Mutex<HashMap<String, Remembered>>,
}

/// A remembered credential with its runtime state, and the pick that owns the entry.
type Remembered = (Weak<Pick>, Arc<Credential>, Arc<Availability>);

impl SessionHome {
    fn slot(&self) -> std::sync::MutexGuard<'_, Option<Kept>> {
        self.kept.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records the request-retry limit Home sent with a fresh pick, which a kept pick
    /// carries to later turns.
    pub(crate) fn granted(&self, request_retry: Option<i64>) {
        *self.granted_retry.lock().unwrap_or_else(PoisonError::into_inner) = request_retry;
    }

    /// Go `retainHomeWebsocketSelection`: a pick kept for a previous turn ends
    /// (`target_replaced`).
    /// The session remembers a credential with WebSockets enabled until this pick ends
    /// (Go `bindHomeSelectionRuntimeAuth`).
    pub(crate) fn keep(
        self: &Arc<Self>,
        pick: Arc<Pick>,
        credential: &Arc<Credential>,
        availability: Arc<Availability>,
        route: &str,
        user_key: String,
    ) {
        let request_retry = *self.granted_retry.lock().unwrap_or_else(PoisonError::into_inner);
        if cpa_exec::websockets_enabled(credential) {
            self.remember(&pick, credential, availability);
        }
        let previous = self.slot().replace(Kept {
            pick,
            credential: credential.id.clone(),
            route: crate::scheduler::canonical_model(route).to_owned(),
            user_key,
            request_retry,
            lent: false,
        });
        if let Some(previous) = previous {
            previous.pick.close();
        }
    }

    /// Go `retainedHomeSessionSelection`: the kept lease, its client key and Home's
    /// request-retry limit when this
    /// pick may run on it (a first pick of the first round, same route, a credential
    /// neither excluded nor other than the pinned one, not drained). Any other kept
    /// pick ends (`target_changed`); its release joins `releases`.
    pub(crate) fn reuse(
        &self,
        first_pick: bool,
        route: &str,
        excluded: &[String],
        pinned: Option<&str>,
        releases: &crate::remote::PendingReleases,
    ) -> Option<(Lease, String, Option<i64>)> {
        let mut slot = self.slot();
        let kept = slot.take()?;
        kept.pick.release_into(releases);
        let route = crate::scheduler::canonical_model(route);
        let matches = first_pick
            && !kept.lent
            && !route.is_empty()
            && kept.route == route
            && !excluded.contains(&kept.credential)
            && pinned.is_none_or(|id| id.is_empty() || id == kept.credential);
        if matches && let Some(lease) = kept.pick.lend() {
            let (user_key, request_retry) = (kept.user_key.clone(), kept.request_retry);
            *slot = Some(Kept { lent: true, ..kept });
            return Some((lease, user_key, request_retry));
        }
        drop(slot);
        kept.pick.close();
        None
    }

    /// The pick for an attempt on `credential`: the lent one when its lease came back
    /// for this attempt, else a new one. A lent pick the attempt does not take ends.
    pub(crate) fn pick(&self, credential: &str) -> Arc<Pick> {
        let lent = {
            let mut slot = self.slot();
            match slot.take() {
                Some(kept) if kept.lent => Some(kept),
                other => {
                    *slot = other;
                    None
                }
            }
        };
        match lent {
            Some(kept) if kept.credential == credential => kept.pick,
            Some(kept) => {
                kept.pick.close();
                Arc::default()
            }
            None => Arc::default(),
        }
    }

    fn remember(self: &Arc<Self>, pick: &Arc<Pick>, credential: &Arc<Credential>, availability: Arc<Availability>) {
        let id = credential.id.clone();
        let owner = Arc::downgrade(pick);
        self.auths
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.clone(), (owner.clone(), credential.clone(), availability));
        // Go `forgetHomeRuntimeAuth`: only while this pick still owns the entry, so an
        // ended pick never erases a newer pick's credential with the same ID.
        let session = Arc::downgrade(self);
        let forget = move || {
            if let Some(session) = session.upgrade() {
                let mut auths = session.auths.lock().unwrap_or_else(PoisonError::into_inner);
                if auths.get(&id).is_some_and(|(current, _, _)| current.ptr_eq(&owner)) {
                    auths.remove(&id);
                }
            }
        };
        let mut state = pick.state();
        if state.ended {
            drop(state);
            forget();
        } else {
            state.forget = Some(Box::new(forget));
        }
    }

    /// Go `GetExecutionSessionAuthByID`: a Home credential a live kept pick holds. Every
    /// pick ends before it drops (a session's `Kept` holds it, an attempt's `PickGuard`
    /// closes it), so no entry outlives its owner.
    pub(crate) fn auth(&self, id: &str) -> Option<(Arc<Credential>, Arc<Availability>)> {
        self.auths
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .map(|(_, credential, availability)| (credential.clone(), availability.clone()))
    }

    /// The downstream connection ended (Go `CloseExecutionSession`).
    pub(crate) fn close(&self) {
        self.auths.lock().unwrap_or_else(PoisonError::into_inner).clear();
        if let Some(kept) = self.slot().take() {
            kept.pick.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::{ModelsError, RemoteDispatch, RemoteError, RemoteGrant, RemoteRequest};
    use futures_util::future::BoxFuture;

    /// Grants one credential on every pick.
    struct Grant(Credential);

    impl RemoteDispatch for Grant {
        fn available(&self) -> bool {
            true
        }

        fn dispatch(&self, _: RemoteRequest) -> BoxFuture<'_, Result<RemoteGrant, RemoteError>> {
            let credential = self.0.clone();
            Box::pin(async move {
                Ok(RemoteGrant {
                    credential,
                    end: Box::new(|| None),
                    cancel: None,
                    request_retry: None,
                    user_api_key: String::new(),
                    private: Default::default(),
                })
            })
        }

        fn models(
            &self,
            _: Vec<(String, String)>,
            _: Vec<(String, String)>,
        ) -> BoxFuture<'_, Result<Vec<u8>, ModelsError>> {
            Box::pin(async { Err(ModelsError::Unavailable) })
        }
    }

    fn home_credential(id: &str, websockets: &str) -> Credential {
        let mut credential = Credential::relay_session(id);
        credential.attributes.insert("websockets".into(), websockets.into());
        credential
    }

    struct Fixture {
        rt: crate::runtime::Runtime,
        releases: crate::remote::PendingReleases,
    }

    impl Fixture {
        fn new() -> Self {
            let executors = cpa_exec::Executors {
                claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:9").unwrap(),
                codex: Default::default(),
                meta: Default::default(),
                xai: Default::default(),
            };
            Self {
                rt: crate::testing::runtime(Default::default(), Vec::new(), executors),
                releases: Default::default(),
            }
        }

        /// A successful turn on `credential` for route `gpt-5` whose socket retained its
        /// pick, which the session keeps.
        async fn keep(&self, home: &Arc<SessionHome>, credential: Credential) -> Arc<Pick> {
            let selection = crate::runtime::Selection {
                model: "gpt-5".into(),
                ..Default::default()
            };
            let (lease, _, _) = self
                .rt
                .acquire_remote(&Grant(credential), selection, RemoteRequest::default(), &self.releases)
                .await
                .unwrap();
            let credential = lease.credential.clone();
            let pick: Arc<Pick> = Arc::default();
            assert!(pick.retain(Box::new(|| ())));
            assert!(pick.keep(lease).is_none());
            home.keep(pick.clone(), &credential, Default::default(), "gpt-5", String::new());
            pick
        }

        fn reuse(&self, home: &SessionHome, first_pick: bool, route: &str, excluded: &[String]) -> bool {
            home.reuse(first_pick, route, excluded, Some("home-a"), &self.releases)
                .is_some()
        }
    }

    /// Go `TestPickNextViaHomeDoesNotReuseTriedPinnedWebsocketAuth` and
    /// `TestPickNextViaHomeDoesNotReusePinnedWebsocketAuthAfterFirstHomeAttempt`: a live
    /// kept pick serves only the first pick of the first round, for the same route, of
    /// a credential not tried yet. Any other pick ends it, and the session forgets its
    /// credential (Go `forgetHomeRuntimeAuth` from the selection's end), so a later
    /// turn back on the route cannot pin it.
    #[tokio::test]
    async fn a_kept_pick_serves_only_a_first_untried_pick_and_takes_its_credential_along() {
        let fixture = Fixture::new();
        let home = Arc::new(SessionHome::default());
        let tried = ["home-a".to_owned()];
        for (first_pick, route, excluded) in [
            (false, "gpt-5", &[][..]),
            (true, "gpt-5", &tried[..]),
            (true, "claude-x", &[][..]),
        ] {
            fixture.keep(&home, home_credential("home-a", "true")).await;
            assert!(home.auth("home-a").is_some());
            assert!(
                !fixture.reuse(&home, first_pick, route, excluded),
                "{first_pick} {route} {excluded:?}"
            );
            assert!(home.auth("home-a").is_none(), "{first_pick} {route} {excluded:?}");
        }
        // The control: the same kept pick serves a fitting first pick and stays known.
        fixture.keep(&home, home_credential("home-a", "true")).await;
        assert!(fixture.reuse(&home, true, "gpt-5", &[]));
        assert!(home.auth("home-a").is_some());
    }

    /// Go `bindHomeSelectionRuntimeAuth`, `forgetHomeRuntimeAuth`'s owner check,
    /// `TestPickNextViaHomeDoesNotReusePinnedNonWebsocketAuth` and
    /// `TestCloseExecutionSessionClearsHomeRuntimeAuthForSession`: the session sees a
    /// kept credential only while its pick lives, a replaced pick with the same ID does
    /// not erase its successor's entry, a credential without WebSockets is never
    /// remembered, and closing one session forgets only its own.
    #[tokio::test]
    async fn a_remembered_home_credential_lives_with_its_pick() {
        let fixture = Fixture::new();
        let (one, two) = (Arc::new(SessionHome::default()), Arc::new(SessionHome::default()));

        let first = fixture.keep(&one, home_credential("home-a", "true")).await;
        Retainable::end(first.as_ref());
        assert!(one.auth("home-a").is_none(), "the socket's end forgets it");

        let first = fixture.keep(&one, home_credential("home-a", "true")).await;
        let second = fixture.keep(&one, home_credential("home-a", "true")).await;
        assert!(first.state().ended, "target_replaced");
        first.close();
        assert!(
            one.auth("home-a").is_some(),
            "the old pick does not erase its successor"
        );
        second.close();
        assert!(one.auth("home-a").is_none());

        fixture.keep(&one, home_credential("home-b", "false")).await;
        assert!(one.auth("home-b").is_none(), "no WebSockets, no view");

        fixture.keep(&one, home_credential("home-a", "true")).await;
        fixture.keep(&two, home_credential("home-a", "true")).await;
        one.close();
        assert!(one.auth("home-a").is_none());
        assert!(two.auth("home-a").is_some(), "another session keeps its own");
        two.close();
        assert!(two.auth("home-a").is_none());
    }
}
