//! Go's per-credential `RegistrationEpoch` and `CredentialVersion`, which fence token
//! refreshes: a refresh commits only under the registration it started under, and a
//! refresh that meets another credential version drops its result unless a Codex token
//! partition it refreshed still applies ([`crate::runtime::commit_refresh`]).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cpa_core::credential::Credential;

/// What a refresh of a credential starts under.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct Registered {
    /// Go `RegistrationEpoch`: set when the credential's ID enters the set, kept while
    /// it is registered again.
    pub registration: u64,
    /// Go `CredentialVersion`: bumped by every registration that changes the
    /// credential's tokens or keys.
    pub credential_version: u64,
}

/// The source of [`Registered::registration`] numbers.
static REGISTRATIONS: AtomicU64 = AtomicU64::new(1);

/// Credential ID to its [`Registered`].
pub(crate) type Registrations = Arc<Mutex<HashMap<String, Registered>>>;

fn lock(registrations: &Registrations) -> std::sync::MutexGuard<'_, HashMap<String, Registered>> {
    registrations.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Registers a change to the store's set from `before` to `after` (the credentials it
/// replaced, and the set or the credentials replacing them); the store calls it with the
/// change, under its write lock. Go `Register` gives an auth new to the manager the next
/// epoch for its ID; `Update` keeps it, and bumps its credential version when its
/// credentials changed (`updateInternal`, `CredentialsChanged`). A credential that leaves
/// the set loses its entry (Go `Manager.Remove`), so the map holds the set's IDs only.
pub(crate) fn register(registrations: &Registrations, before: &[Arc<Credential>], after: &[Arc<Credential>]) {
    let mut registrations = lock(registrations);
    let previous: HashMap<&str, &Credential> = before.iter().map(|c| (c.id.as_str(), &**c)).collect();
    let mut registered = Vec::new();
    for c in after {
        let previous = previous.get(c.id.as_str());
        if previous.is_some_and(|p| p.revision == c.revision) {
            continue;
        }
        let kept = previous.and_then(|_| registrations.get(&c.id).copied());
        let entry = match (kept, previous) {
            (Some(kept), Some(previous)) => Registered {
                registration: kept.registration,
                credential_version: kept.credential_version
                    + u64::from(crate::runtime::credentials_changed(previous, c)),
            },
            _ => Registered {
                registration: REGISTRATIONS.fetch_add(1, Ordering::Relaxed),
                credential_version: 1,
            },
        };
        registered.push((c.id.clone(), entry));
    }
    let live: std::collections::HashSet<&str> = after.iter().map(|c| c.id.as_str()).collect();
    for id in previous.keys().filter(|id| !live.contains(*id)) {
        registrations.remove(*id);
    }
    registrations.extend(registered);
}

/// What a refresh of `id` starts under.
pub(crate) fn registered(registrations: &Registrations, id: &str) -> Option<Registered> {
    lock(registrations).get(id).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential(id: &str, revision: u64, token: &str) -> Arc<Credential> {
        let dir = std::path::Path::new("/auth");
        let metadata = serde_json::json!({"type": "codex", "access_token": token});
        let mut c = Credential::from_file(dir, &dir.join(id), metadata.as_object().unwrap().clone()).unwrap();
        c.revision = revision;
        Arc::new(c)
    }

    /// A new ID gets a fresh registration; a revision of it keeps the registration and
    /// bumps the credential version only when its tokens change; removal forgets it, and
    /// the same ID registered again later is a new registration.
    #[test]
    fn registrations_follow_go_epochs_and_credential_versions() {
        let regs = Registrations::default();
        let a1 = credential("a.json", 1, "t1");
        register(&regs, &[], std::slice::from_ref(&a1));
        let first = registered(&regs, "a.json").unwrap();
        assert_eq!(first.credential_version, 1);
        let a2 = credential("a.json", 2, "t1");
        register(&regs, std::slice::from_ref(&a1), std::slice::from_ref(&a2));
        assert_eq!(registered(&regs, "a.json"), Some(first), "same tokens");
        let a3 = credential("a.json", 3, "t2");
        register(&regs, std::slice::from_ref(&a2), std::slice::from_ref(&a3));
        let rotated = registered(&regs, "a.json").unwrap();
        assert_eq!(rotated.registration, first.registration);
        assert_eq!(rotated.credential_version, 2);
        register(&regs, std::slice::from_ref(&a3), &[]);
        assert_eq!(registered(&regs, "a.json"), None);
        let again = credential("a.json", 4, "t2");
        register(&regs, &[], std::slice::from_ref(&again));
        assert_ne!(registered(&regs, "a.json").unwrap().registration, first.registration);
    }
}
