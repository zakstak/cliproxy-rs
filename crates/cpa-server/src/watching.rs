//! Hot reload of `config.yaml` and the auth directory (Go internal/watcher).
//!
//! Go watches the config file and the top level of the auth directory with fsnotify.
//! A config event reloads after 150 ms without further events (`configReloadDebounce`);
//! auth events apply at once, removals after a 50 ms replace check
//! (`replaceCheckDelay`). Both compare content hashes, so self-writes and
//! unchanged rewrites are no-ops.
//!
//! The watcher sleeps until the operating system reports a change in the config file's
//! folder or the auth folder ([`crate::fs_events`]). It then stats the config file and
//! the top-level `*.json` auth files every 50 ms, hashing only files whose metadata
//! changed or that were modified in the last two seconds (git's racy timestamp rule),
//! until the change has been stable for 150 ms (config) or one tick (auth files) and is
//! applied. While nothing changes it does no work and sets no timer. As in Go, changes
//! made by another machine on a network or FUSE file system raise no event and are seen
//! with the next local change.
use cpa_core::gostr::GoStr;
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use cpa_core::config::{Config, credentials};
use sha2::{Digest, Sha256};

use crate::fs_events::{Events, Targets};
use crate::management::Management;

const TICK: Duration = Duration::from_millis(50);
/// Go `configReloadDebounce`.
const CONFIG_SETTLE: Duration = Duration::from_millis(150);
/// Go `replaceCheckDelay`: one tick.
const AUTH_SETTLE: Duration = Duration::from_millis(50);
/// How often the watcher, while idle, checks whether what the last reload could not read
/// ([`Pending`]) can be read now. A permission change also raises a file event, which
/// checks at once; this covers a transient I/O error that clears without one.
const PENDING_RETRY: Duration = Duration::from_secs(2);
/// Files modified this recently are re-hashed even when their metadata is unchanged:
/// coarse file timestamps cannot tell two quick same-size writes apart.
const RACY: Duration = Duration::from_secs(2);

type Hash = [u8; 32];

/// Content identity of everything the watcher observes.
#[derive(Clone, Default, PartialEq, Eq)]
struct Snapshot {
    /// `None` when the config file cannot be read (missing).
    config: Option<Hash>,
    auth: BTreeMap<PathBuf, Hash>,
}

/// File metadata that changes with any write or replacement.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Stat {
    len: u64,
    ino: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl Stat {
    fn of(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            len: meta.len(),
            ino: meta.ino(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
        }
    }
}

/// Hashes by path, reused while a file's metadata is unchanged and not racy. Metadata
/// that is `None` was forgotten: the file is read again, and its last hash stays known
/// for when it cannot be.
#[derive(Default)]
struct HashCache(BTreeMap<PathBuf, (Option<Stat>, Hash)>);

impl HashCache {
    /// The content hash of `path`, or `None` when it cannot be read.
    fn hash(&mut self, path: &Path, meta: &std::fs::Metadata, now: SystemTime) -> Option<Hash> {
        let stat = Stat::of(meta);
        let racy = meta
            .modified()
            .map_or(true, |m| now.duration_since(m).map_or(true, |age| age < RACY));
        if let Some((Some(cached), hash)) = self.0.get(path)
            && *cached == stat
            && !racy
        {
            return Some(*hash);
        }
        #[cfg(test)]
        if faults::fails(faults::Stage::Look, path) {
            return None;
        }
        let data = std::fs::read(path).ok()?;
        let hash: Hash = Sha256::digest(&data).into();
        self.0.insert(path.to_owned(), (Some(stat), hash));
        Some(hash)
    }

    /// The hash last read from `path`, whatever its metadata says now.
    fn last(&self, path: &Path) -> Option<Hash> {
        self.0.get(path).map(|(_, hash)| *hash)
    }

    /// Reads every file again on its next look, keeping the hashes last read.
    fn forget_metadata(&mut self) {
        for (stat, _) in self.0.values_mut() {
            *stat = None;
        }
    }
}

fn is_auth_json(path: &Path) -> bool {
    // Go matches the extension case-insensitively when scanning.
    path.file_name()
        .is_some_and(|n| n.to_string_lossy().go_lower().ends_with(".json"))
}

/// One look at the config file and the auth folder.
struct Observation {
    snapshot: Snapshot,
    /// What this look could not read, so recorded as last seen: auth files whose read
    /// or metadata failed for a reason other than "not found", and the folder when it
    /// could not be listed in full. A change hidden behind such a failure shows in no
    /// snapshot, so the watcher retries these as it retries what a reload could not read.
    unknown: Pending,
}

fn observe(state: &Management, cache: &mut HashCache) -> Observation {
    #[allow(unused_mut)]
    let mut listing = credentials::list_auth_dir(&state.rt.config().auth_dir);
    #[cfg(test)]
    if faults::fails(faults::Stage::LookListing, &listing.dir) {
        listing.paths.clear();
        listing.failure = Some(std::io::Error::from_raw_os_error(libc::EIO));
    }
    observe_listing(state, cache, listing)
}

/// [`observe`] over a listing already taken of the auth folder.
fn observe_listing(state: &Management, cache: &mut HashCache, listing: credentials::Listing) -> Observation {
    let now = SystemTime::now();
    let mut seen = std::collections::HashSet::new();
    let config = std::fs::metadata(&state.path)
        .ok()
        .and_then(|meta| cache.hash(&state.path, &meta, now));
    seen.insert(state.path.clone());
    let mut auth = BTreeMap::new();
    let mut unknown = Pending::default();
    let complete = listing.complete();
    // The scan's classification (`credentials::read_auth_file`): an entry that is gone
    // or not a regular file is absent; one that cannot be read for now is present with
    // the hash last read from it (zeros when never read), so losing access is no change
    // and nothing reloads, while deleting it is a removal the watcher sees.
    let mut listed = std::collections::HashSet::new();
    for path in listing.paths {
        listed.insert(path.clone());
        let hash = match std::fs::metadata(&path) {
            Ok(meta) if !meta.is_file() => continue,
            Ok(meta) => cache.hash(&path, &meta, now),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => None,
        };
        let hash = hash.or_else(|| {
            unknown.files.insert(path.clone());
            cache.last(&path)
        });
        seen.insert(path.clone());
        auth.insert(path, hash.unwrap_or_default());
    }
    // A folder that exists but cannot be listed in full still holds the files last seen
    // in it that the listing did not reach, as last seen: unknown, not removed. An entry
    // the listing reached is settled by what it found, absent included.
    if !complete {
        unknown.dir = Some(listing.dir.clone());
        for (path, (_, hash)) in &cache.0 {
            // The config file can live in the auth folder too.
            if path.parent() == Some(listing.dir.as_path()) && is_auth_json(path) && !listed.contains(path) {
                seen.insert(path.clone());
                auth.insert(path.clone(), *hash);
            }
        }
    }
    cache.0.retain(|path, _| seen.contains(path));
    Observation {
        snapshot: Snapshot { config, auth },
        unknown,
    }
}

/// Go's per-event log lines for auth files that changed between two snapshots.
fn log_auth_changes(before: &Snapshot, after: &Snapshot) {
    let name = |p: &Path| p.file_name().unwrap_or_default().to_string_lossy().into_owned();
    for (path, hash) in &after.auth {
        let op = match before.auth.get(path) {
            None => "CREATE",
            Some(old) if old != hash => "WRITE",
            Some(_) => continue,
        };
        tracing::info!("auth file changed ({op}): {}, processing incrementally", name(path));
    }
    for path in before.auth.keys().filter(|p| !after.auth.contains_key(*p)) {
        tracing::info!("auth file changed (REMOVE): {}, processing incrementally", name(path));
    }
}

pub fn start(state: &Arc<Management>) -> tokio::task::JoinHandle<()> {
    let first = targets(state);
    let weak = Arc::downgrade(state);
    let state = state.clone();
    tokio::spawn(async move {
        // Off the request threads: reading the file waits for the disk lock, setup can
        // block on a slow network path, and on Windows it waits for the watch thread. A
        // panic in the setup leaves the 2-second poll. Go stores its own serialized
        // config snapshot, independently of management mutations of the published
        // runtime config before their disk event arrives.
        let started = tokio::task::spawn_blocking(move || {
            let snapshot = {
                let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
                let hash = std::fs::read(&state.path)
                    .ok()
                    .map(|data| <Hash>::from(Sha256::digest(data)));
                (state.rt.config(), hash)
            };
            let events = catch_unwind(AssertUnwindSafe(|| Events::new(&first))).unwrap_or_default();
            (snapshot, events)
        })
        .await;
        let Ok(((mut previous_config, initial_hash), mut events)) = started else {
            return;
        };
        let state = weak;
        let mut cache = HashCache::default();
        // The state last reloaded; the first stable observation reloads once, as Go's
        // watcher reloads clients when it starts.
        let mut applied: Option<Snapshot> = None;
        let mut observed: Option<Snapshot> = None;
        let mut since = Instant::now();
        // What the last reload did not apply and the last look could not read, and
        // whether any of it can be read now: then the same observation reloads again,
        // though nothing it records has changed.
        let mut pending = Pending::default();
        let mut unknown = Pending::default();
        let mut retry = false;
        loop {
            let woke = applied.is_some() && applied == observed && !retry;
            if woke {
                if pending.is_empty() && unknown.is_empty() {
                    events.changed().await;
                } else {
                    tokio::select! {
                        () = events.changed() => {}
                        () = tokio::time::sleep(PENDING_RETRY) => {}
                    }
                }
            } else {
                tokio::time::sleep(TICK).await;
            }
            let Some(state) = state.upgrade() else {
                return;
            };
            // After an event, retarget before looking: a file that lands before the new
            // watch exists shows up in this look, and one that lands after raises an
            // event. Cost per wake: a few watch calls, on the blocking pool.
            let wake_targets = woke.then(|| targets(&state));
            // A panic in the work is caught inside the closure, so `events` always comes
            // back and notifications stay on.
            let looked = tokio::task::spawn_blocking({
                let state = state.clone();
                let pending = (!retry).then(|| pending.union(&unknown)).filter(|p| !p.is_empty());
                move || {
                    let snapshot = catch_unwind(AssertUnwindSafe(|| {
                        if let Some(t) = wake_targets {
                            events.retarget(&t);
                        }
                        let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
                        let ready = pending.is_some_and(|p| p.ready());
                        (observe(&state, &mut cache), ready)
                    }));
                    (snapshot.ok(), cache, events)
                }
            })
            .await;
            let snapshot = match looked {
                Ok((snapshot, returned_cache, returned_events)) => {
                    cache = returned_cache;
                    events = returned_events;
                    snapshot.map(|(observation, ready)| {
                        retry |= ready;
                        unknown = observation.unknown;
                        observation.snapshot
                    })
                }
                // Cancelled at shutdown.
                Err(_) => {
                    cache = HashCache::default();
                    events = Events::default();
                    None
                }
            };
            let Some(snapshot) = snapshot else {
                tracing::error!(
                    "config watcher: looking at the config and auth files failed; retrying on the next change"
                );
                continue;
            };
            if observed.as_ref() != Some(&snapshot) {
                observed = Some(snapshot);
                since = Instant::now();
                continue;
            }
            let Some(current) = observed.clone() else { continue };
            if applied.as_ref() == Some(&current) && !retry {
                continue;
            }
            let config_changed = applied.as_ref().map_or(initial_hash, |a| a.config) != current.config;
            // The first observation always reads the file: an edit made after the
            // runtime loaded its config but before `start` hashed the file matches
            // `initial_hash`, yet the runtime has never seen it.
            let read_config = config_changed || applied.is_none();
            if since.elapsed() < if read_config { CONFIG_SETTLE } else { AUTH_SETTLE } {
                continue;
            }
            if let Some(before) = &applied {
                log_auth_changes(before, &current);
                if config_changed && current.config.is_some() {
                    tracing::info!("config file changed, reloading: {}", state.path.display());
                }
            }
            // The reload may move auth-dir, and new auth files need their own watches on
            // macOS: retarget after it, on the same blocking thread. A change that lands
            // before a new watch exists is caught by the extra look `Events` then makes.
            let reloaded = tokio::task::spawn_blocking({
                let state = state.clone();
                let current = current.clone();
                move || {
                    let loaded = catch_unwind(AssertUnwindSafe(|| reload_config(&state, read_config, &current)));
                    let _ = catch_unwind(AssertUnwindSafe(|| {
                        events.retarget(&targets(&state));
                    }));
                    (loaded.map_err(|_| ()), events)
                }
            })
            .await;
            retry = false;
            let (loaded, read) = match reloaded {
                Ok((loaded, returned)) => {
                    events = returned;
                    match loaded {
                        Ok((loaded, read, unread)) => {
                            pending = unread;
                            (Ok(loaded), Some(read))
                        }
                        Err(()) => (Err(()), None),
                    }
                }
                Err(_) => {
                    events = Events::default();
                    (Err(()), None)
                }
            };
            if loaded.is_err() {
                tracing::error!("config watcher: reload failed unexpectedly; retrying on the next change");
            }
            if let Some(before) = &applied {
                let accepted = matches!(loaded, Ok(Ok(Some(_))));
                persist_changes(&state, before, &current, config_changed && accepted);
            }
            match loaded {
                Ok(Ok(Some(next_config))) => {
                    // The first observation's read of the file the runtime started
                    // with is quiet; Go does not reload at startup.
                    if config_changed || next_config.document != previous_config.document {
                        crate::config_diff::log(&previous_config, &next_config);
                        tracing::info!("config successfully reloaded, triggering client reload");
                    }
                    previous_config = Arc::new(next_config);
                }
                Ok(Err(error)) if config_changed || applied.is_none() => match error {
                    ReloadError::Missing(e) => tracing::error!(
                        "failed to read config file for hash check: open {}: {}",
                        state.path.display(),
                        go_errno_text(&e)
                    ),
                    ReloadError::Invalid(e) => tracing::error!("failed to reload config: {e:#}"),
                },
                _ => {}
            }
            // Applied once either way: a failed config load retries on its next change,
            // as Go retries on its next file event, rather than every tick.
            applied = Some(record(&mut cache, current, read));
        }
    })
}

/// What a reload applied: what it read, not what was observed before it, so a write
/// landing in between is reloaded when it is next observed (Go records the parsed
/// config's `SourceHash` and forgets the hash of an incomplete auth read). When the two
/// differ, cached hashes may be the stale ones (a same-size write with the same
/// modification time), so the next observation reads afresh. The hashes last read stay
/// known for the files it then cannot read.
fn record(cache: &mut HashCache, observed: Snapshot, read: Option<Snapshot>) -> Snapshot {
    let read = read.unwrap_or_else(|| observed.clone());
    if read != observed {
        cache.forget_metadata();
        // What the reload read is now the last known content: a look that then fails to
        // read a file reports these bytes, the applied ones, not older ones of its own.
        for (path, hash) in &read.auth {
            cache.0.insert(path.clone(), (None, *hash));
        }
    }
    read
}

/// What the watch should cover: the config file and the auth folder of the published
/// config.
fn targets(state: &Management) -> Targets {
    Targets {
        config: state.path.clone(),
        auth_dir: state.rt.config().auth_dir.clone(),
    }
}

/// Go `persistConfigAsync` / `persistAuthAsync`: with a remote store, what changed
/// between two applied snapshots is pushed in the background. The config goes only
/// after a reload accepted it, and never when empty or missing (Go ignores those
/// events). An auth write goes as "Sync auth <name>" unless the file is empty or not a
/// JSON object; a removal goes as "Remove auth <name>".
// ponytail: Go also skips files whose JSON does not decode into its `Auth` struct or
// whose weight is invalid; here any JSON object is pushed.
fn persist_changes(state: &Management, before: &Snapshot, after: &Snapshot, config_accepted: bool) {
    let Some(store) = state.store.clone() else {
        return;
    };
    let config = config_accepted && std::fs::metadata(&state.path).is_ok_and(|m| m.len() > 0);
    let mut auth = Vec::new();
    for (path, hash) in &after.auth {
        if before.auth.get(path) == Some(hash) {
            continue;
        }
        let object = std::fs::read(path)
            .ok()
            .filter(|data| !data.is_empty())
            .is_some_and(|data| serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&data).is_ok());
        if object {
            auth.push(("Sync", path.clone()));
        }
    }
    for path in before.auth.keys().filter(|p| !after.auth.contains_key(*p)) {
        auth.push(("Remove", path.clone()));
    }
    if !config && auth.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let bound = Duration::from_secs(30);
        if config {
            match tokio::time::timeout(bound, store.persist_config()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::error!("failed to persist config change: {error:#}"),
                Err(_) => tracing::error!("failed to persist config change: context deadline exceeded"),
            }
        }
        for (verb, path) in auth {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let message = format!("{verb} auth {name}");
            match tokio::time::timeout(bound, store.persist_auth_files(message, vec![path])).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::error!("failed to persist auth changes: {error:#}"),
                Err(_) => tracing::error!("failed to persist auth changes: context deadline exceeded"),
            }
        }
    });
}

/// Go's `syscall.Errno` text: strerror in lower case, without Rust's suffix.
fn go_errno_text(e: &std::io::Error) -> String {
    let text = e.to_string();
    let text = text.split(" (os error").next().unwrap_or_default();
    let mut chars = text.chars();
    chars
        .next()
        .map_or_else(String::new, |c| c.to_lowercase().chain(chars).collect())
}

/// Why [`reload`] kept the last good config.
#[derive(Debug)]
pub enum ReloadError {
    /// The config file could not be read (Go logs it and keeps running).
    Missing(std::io::Error),
    /// The config file did not load (Go `failed to reload config`).
    Invalid(anyhow::Error),
}

impl std::fmt::Display for ReloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(e) => write!(f, "{e}"),
            Self::Invalid(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for ReloadError {}

/// Blocking reload. Shared with deterministic tests; no provider calls are made.
///
/// A missing, empty or rejected config keeps the last good config, and the auth
/// directory is reconciled against it anyway: Go handles auth events independently
/// of the config file.
pub fn reload(state: &Management) -> Result<(), ReloadError> {
    reload_config(state, true, &Snapshot::default()).0.map(|_| ())
}

/// Auth files whose current bytes are not known to be applied, because a read failed
/// for a reason other than "not found", at whichever stage it failed: the watcher's
/// look ([`Observation::unknown`]), the reload's scan (unreadable files, a folder it
/// could not list in full), or the freshness check at publication (a scanned
/// credential rejected because its file changed, went or could not be read since the
/// scan). The applied snapshot records these entries as observed, so a lasting denial
/// is no change and nothing reloads every tick; the watcher keeps them here instead and
/// reloads once one can be read, with no new write and no file event needed (#89: a
/// re-login or a revocation hidden behind one transient error is applied then).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Pending {
    files: std::collections::BTreeSet<PathBuf>,
    dir: Option<PathBuf>,
}

impl Pending {
    fn is_empty(&self) -> bool {
        self.files.is_empty() && self.dir.is_none()
    }

    fn union(&self, other: &Pending) -> Pending {
        Pending {
            files: self.files.union(&other.files).cloned().collect(),
            dir: self.dir.clone().or_else(|| other.dir.clone()),
        }
    }

    /// Whether a reload could now read something the last one could not: the folder
    /// lists in full, or a file reads (or is gone).
    fn ready(&self) -> bool {
        self.dir
            .as_deref()
            .is_some_and(|dir| credentials::list_auth_dir(dir).complete())
            || self
                .files
                .iter()
                .any(|path| !matches!(credentials::read_auth_file(path), credentials::AuthFile::Unreadable(_)))
    }
}

/// Return the config actually accepted under the disk lock, not a later runtime
/// snapshot possibly replaced by management (None is an auth-only or empty reload), the
/// hashes of what this reload read, and what it could not read. Entries it did not
/// read are as `observed`.
fn reload_config(
    state: &Management,
    read_config: bool,
    observed: &Snapshot,
) -> (Result<Option<Config>, ReloadError>, Snapshot, Pending) {
    let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
    let mut read = Snapshot {
        config: observed.config,
        auth: BTreeMap::new(),
    };
    // Recorded as the observation records it: the hash of the bytes read, `None` when
    // the read fails.
    let (mut config, config_error, accepted) = match read_config.then(|| std::fs::read(&state.path)) {
        None => ((*state.rt.config()).clone(), None, false),
        Some(Err(error)) => {
            read.config = None;
            let error = if std::fs::metadata(&state.path).is_err() {
                ReloadError::Missing(error)
            } else {
                ReloadError::Invalid(error.into())
            };
            ((*state.rt.config()).clone(), Some(error), false)
        }
        Some(Ok(data)) => {
            read.config = Some(Sha256::digest(&data).into());
            let loaded = (!data.is_empty()).then(|| {
                std::str::from_utf8(&data)
                    .map_err(anyhow::Error::from)
                    .and_then(|text| Config::load_text(&state.path, text))
            });
            match loaded {
                // Go ignores an empty config write.
                None => ((*state.rt.config()).clone(), None, false),
                Some(Ok(config)) => (config, None, true),
                Some(Err(error)) => ((*state.rt.config()).clone(), Some(ReloadError::Invalid(error)), false),
            }
        }
    };
    state.lock_auth_dir(&mut config);
    #[cfg(test)]
    {
        let hook = BEFORE_SCAN
            .lock()
            .unwrap()
            .iter()
            .find(|(dir, _)| *dir == config.auth_dir)
            .map(|(_, hook)| hook.clone());
        if let Some(hook) = hook {
            hook();
        }
    }
    let listing = credentials::list_auth_dir(&config.auth_dir);
    let scan = credentials::scan_listing_with(&config, listing, |path| {
        let file = credentials::read_auth_file(path);
        #[cfg(test)]
        let file = faults::read(faults::Stage::Scan, path, file);
        file
    });
    read.auth = scan.read.clone();
    // A file the scan could not read, or did not reach in a folder it could not list,
    // stays as observed; recording it as missing would reload it on every tick. What the
    // scan could not read is pending, so that it is retried once it can be.
    let unsettled = scan.unsettled();
    for (path, hash) in &observed.auth {
        if unsettled.contains(path) {
            read.auth.entry(path.clone()).or_insert(*hash);
        }
    }
    let mut pending = Pending {
        files: scan.unreadable.iter().cloned().collect(),
        dir: scan.unlisted.clone(),
    };
    let result = accepted.then(|| config.clone());
    match state.publish(config, Some(scan)) {
        // A scanned credential the freshness check rejected (its file changed, went or
        // could not be read since the scan) was not applied, whatever this records.
        Some(rejected) => pending.files.extend(rejected),
        // Unreachable from here today: a config the runtime refuses fails to load
        // first, and the running config was accepted. Nothing was applied.
        None => pending
            .files
            .extend(observed.auth.keys().chain(read.auth.keys()).cloned()),
    }
    let result = match config_error {
        Some(error) => Err(error),
        None => Ok(result),
    };
    (result, read, pending)
}

/// Test-only read faults at each stage that reads an auth file or the auth folder, so
/// a transient error can be placed exactly, with no file event before or after it.
#[cfg(test)]
pub(crate) mod faults {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Stage {
        /// The watcher's read of an auth file.
        Look,
        /// The watcher's listing of the auth folder: fails before any entry.
        LookListing,
        /// The reload's scan read of an auth file.
        Scan,
        /// The freshness check's read when the scan is published.
        Freshness,
    }

    #[allow(clippy::type_complexity)]
    static ARMED: std::sync::Mutex<Vec<(Stage, PathBuf, Arc<AtomicUsize>)>> = std::sync::Mutex::new(Vec::new());

    /// Fails every read of `path` at `stage` until [`disarm`]; returns the hit count.
    pub(crate) fn arm(stage: Stage, path: &Path) -> Arc<AtomicUsize> {
        let hits = Arc::new(AtomicUsize::new(0));
        ARMED.lock().unwrap().push((stage, path.to_owned(), hits.clone()));
        hits
    }

    pub(crate) fn disarm(stage: Stage, path: &Path) {
        ARMED.lock().unwrap().retain(|(s, p, _)| !(*s == stage && p == path));
    }

    pub(crate) fn fails(stage: Stage, path: &Path) -> bool {
        let armed = ARMED.lock().unwrap();
        let hit = armed.iter().find(|(s, p, _)| *s == stage && p == path);
        hit.inspect(|(_, _, hits)| {
            hits.fetch_add(1, Ordering::SeqCst);
        })
        .is_some()
    }

    /// `file`, or an I/O error when a fault is armed for `path` at `stage`.
    pub(crate) fn read(
        stage: Stage,
        path: &Path,
        file: cpa_core::config::credentials::AuthFile,
    ) -> cpa_core::config::credentials::AuthFile {
        if fails(stage, path) {
            cpa_core::config::credentials::AuthFile::Unreadable(std::io::Error::from_raw_os_error(libc::EIO))
        } else {
            file
        }
    }
}

/// Test hook: runs in a reload of the auth folder it names, after the watcher observed
/// the folder and before the reload's scan reads it.
#[cfg(test)]
#[allow(clippy::type_complexity)]
static BEFORE_SCAN: std::sync::Mutex<Vec<(PathBuf, Arc<dyn Fn() + Send + Sync>)>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
mod tests {
    use super::*;
    use cpa_core::credential::Source;

    fn state() -> (Arc<Management>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("cpa-watch-outcome-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.yaml");
        std::fs::write(&path, format!("oauth: {{auth-dir: {}}}\n", dir.display())).unwrap();
        let rt = Arc::new(crate::testing::runtime(
            Config::load(&path).unwrap(),
            Vec::new(),
            cpa_exec::Executors {
                claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:9").unwrap(),
                codex: Default::default(),
                meta: Default::default(),
                xai: Default::default(),
            },
        ));
        (Management::new(rt, path), dir)
    }

    /// Also Go `TestSetConfigAppliesCurrentFileAndNonFileConfigs_Issue6631`: an external
    /// edit is published by the reload that read it, and a config that did not come from
    /// the file (a management or Home publication) still applies.
    #[tokio::test]
    async fn accepted_config_is_captured_and_skipped_reloads_do_not_advance_it() {
        let (state, dir) = state();
        let config = |port| format!("port: {port}\noauth: {{auth-dir: {}}}\n", dir.display());
        std::fs::write(&state.path, config(9001)).unwrap();
        let accepted = reload_config(&state, true, &Snapshot::default()).0.unwrap().unwrap();
        state.rt.publish_config(Config::parse(&config(9002)).unwrap());
        assert_eq!(accepted.port, 9001); // Not the later management publication.
        std::fs::write(&state.path, "").unwrap();
        assert!(reload_config(&state, true, &Snapshot::default()).0.unwrap().is_none());
        assert_eq!(state.rt.config().port, 9002);
        std::fs::write(&state.path, config(9003)).unwrap();
        assert!(reload_config(&state, false, &Snapshot::default()).0.unwrap().is_none()); // Auth-only event.
        assert_eq!(state.rt.config().port, 9002);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// #26: a reload that brings back a Codex `header-defaults.user-agent` (v8 or legacy
    /// spelling) is refused like any invalid config: the running config stays, so the
    /// obsolete identity setting never reaches a request.
    #[tokio::test]
    async fn a_reload_with_a_codex_user_agent_default_keeps_the_running_config() {
        let (state, dir) = state();
        let auth = dir.display();
        std::fs::write(&state.path, format!("port: 9101\noauth: {{auth-dir: {auth}}}\n")).unwrap();
        reload(&state).unwrap();
        for obsolete in [
            format!(
                "port: 9102\noauth: {{auth-dir: {auth}, providers: {{codex: {{header-defaults: {{user-agent: 'codex-tui/0.154.0 (Mac OS 26.5.2; arm64)'}}}}}}}}\n"
            ),
            format!("port: 9102\nauth-dir: {auth}\ncodex-header-defaults: {{user-agent: legacy-agent}}\n"),
        ] {
            std::fs::write(&state.path, &obsolete).unwrap();
            let error = reload(&state).expect_err("refused");
            assert!(matches!(error, ReloadError::Invalid(_)), "{error}");
            assert!(
                error
                    .to_string()
                    .contains("header-defaults.user-agent is no longer supported"),
                "{error}"
            );
            let running = state.rt.config();
            assert_eq!(running.port, 9101, "the last good config keeps running");
            assert!(running.document["oauth"]["providers"]["codex"]["header-defaults"]["user-agent"].is_null());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// #47: a reload whose config has a `plugins` section, enabled or not, is refused
    /// with the migration error: the running config stays.
    #[tokio::test]
    async fn a_reload_with_a_plugins_section_keeps_the_running_config() {
        let (state, dir) = state();
        let auth = dir.display();
        std::fs::write(&state.path, format!("port: 9301\noauth: {{auth-dir: {auth}}}\n")).unwrap();
        reload(&state).unwrap();
        for section in ["plugins: {enabled: true, dir: plugins}", "plugins: {enabled: false}"] {
            std::fs::write(
                &state.path,
                format!("port: 9302\noauth: {{auth-dir: {auth}}}\n{section}\n"),
            )
            .unwrap();
            let error = reload(&state).expect_err("refused");
            assert!(matches!(error, ReloadError::Invalid(_)), "{error}");
            assert!(
                format!("{error:#}").contains(cpa_core::config::PLUGINS_REMOVED),
                "{error:#}"
            );
            assert_eq!(state.rt.config().port, 9301, "the last good config keeps running");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// #28: a reload that marks a `codex-api-key` model `is-compat` is refused like any
    /// invalid config: the running config stays, so the obsolete rewrite control never
    /// reaches a request.
    #[tokio::test]
    async fn a_reload_with_a_codex_is_compat_model_keeps_the_running_config() {
        let (state, dir) = state();
        let auth = dir.display();
        let entry = |compat: &str| {
            format!(
                "auth-dir: {auth}\ncodex-api-key: [{{api-key: k, base-url: https://x.test, models: [{{name: a, alias: b{compat}}}]}}]\n"
            )
        };
        std::fs::write(&state.path, format!("port: 9201\n{}", entry(""))).unwrap();
        reload(&state).unwrap();
        std::fs::write(&state.path, format!("port: 9202\n{}", entry(", is-compat: true"))).unwrap();
        let error = reload(&state).expect_err("refused");
        assert!(matches!(error, ReloadError::Invalid(_)), "{error}");
        assert!(
            error.to_string().contains("is-compat is no longer supported"),
            "{error}"
        );
        assert_eq!(state.rt.config().port, 9201, "the last good config keeps running");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// One watcher reload as `start` makes it: reload what was observed, then record
    /// what was applied.
    /// [`observe`]'s snapshot.
    fn look(state: &Management, cache: &mut HashCache) -> Snapshot {
        observe(state, cache).snapshot
    }

    fn step(state: &Management, cache: &mut HashCache, read_config: bool, observed: &Snapshot) -> Snapshot {
        step_pending(state, cache, read_config, observed).0
    }

    /// [`step`], with what the reload could not read.
    fn step_pending(
        state: &Management,
        cache: &mut HashCache,
        read_config: bool,
        observed: &Snapshot,
    ) -> (Snapshot, Pending) {
        let (_, read, pending) = reload_config(state, read_config, observed);
        (record(cache, observed.clone(), Some(read)), pending)
    }

    /// The watcher records what its reload read, not what it observed before: a write
    /// landing after the read is reloaded when next observed, even when it restores
    /// bytes observed earlier (Go 1631875b
    /// `TestReloadConfigIfChangedReloadsWriteLandingDuringReload_Issue6631` and
    /// `TestReloadConfigIfChangedReloadsAfterWriteBetweenHashAndLoad_Issue6631`), and an
    /// auth file truncated before the scan and finished after it is reloaded (Go
    /// 7484a521 forgets the hash of an incomplete read). Go
    /// `TestReloadConfigIfChangedSerializesConcurrentReloads_Issue6631` has no Rust
    /// interleaving: one watcher task reloads, under the disk lock.
    #[tokio::test]
    async fn the_watcher_records_what_its_reload_read_not_what_it_observed() {
        let (state, dir) = state();
        let mut cache = HashCache::default();
        let config = |port| format!("port: {port}\noauth: {{auth-dir: {}}}\n", dir.display());
        std::fs::write(&state.path, config(9001)).unwrap();
        let observed = look(&state, &mut cache);
        std::fs::write(&state.path, config(9002)).unwrap(); // Lands before the reload reads.
        let applied = step(&state, &mut cache, true, &observed);
        assert_eq!(state.rt.config().port, 9002);
        std::fs::write(&state.path, config(9001)).unwrap(); // Restores the observed bytes.
        let restored = look(&state, &mut cache);
        assert!(restored != applied, "the restore must be reloaded");
        step(&state, &mut cache, true, &restored);
        assert_eq!(state.rt.config().port, 9001);

        let file = dir.join("a.json");
        let account = |email: &str| format!(r#"{{"type":"codex","email":"{email}"}}"#);
        let email = |state: &Management| {
            let creds = state.rt.store().snapshot();
            let cred = creds.iter().find(|c| c.id == "a.json").expect("a.json is loaded");
            cred.str("email").unwrap_or_default().to_owned()
        };
        std::fs::write(&file, account("a@example.com")).unwrap();
        reload(&state).unwrap();
        std::fs::write(&file, account("b@example.com")).unwrap();
        let observed = look(&state, &mut cache);
        std::fs::write(&file, "").unwrap(); // Truncated before the scan.
        let applied = step(&state, &mut cache, false, &observed);
        assert_eq!(email(&state), "a@example.com"); // Kept while incomplete.
        std::fs::write(&file, account("b@example.com")).unwrap(); // Finished after it.
        let finished = look(&state, &mut cache);
        assert!(
            finished == observed && finished != applied,
            "the finished write must be reloaded"
        );
        let applied = step(&state, &mut cache, false, &finished);
        assert_eq!(email(&state), "b@example.com");
        assert!(look(&state, &mut cache) == applied, "nothing left to reload");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A config whose metadata reads as empty but whose contents cannot be read is
    /// recorded as the observation sees it, unreadable, so it does not reload every
    /// tick; it reloads once it changes.
    #[tokio::test]
    async fn an_unreadable_empty_config_converges() {
        let (state, dir) = state();
        let mut cache = HashCache::default();
        let port = state.rt.config().port;
        std::fs::remove_file(&state.path).unwrap();
        // Zero bytes by its metadata, and every read fails (EIO), even for root.
        std::os::unix::fs::symlink("/proc/self/mem", &state.path).unwrap();
        let observed = look(&state, &mut cache);
        assert_eq!(observed.config, None);
        let applied = step(&state, &mut cache, true, &observed);
        assert!(look(&state, &mut cache) == applied, "no reload without a change");
        assert_eq!(state.rt.config().port, port);
        std::fs::remove_file(&state.path).unwrap();
        std::fs::write(
            &state.path,
            format!("port: 9005\noauth: {{auth-dir: {}}}\n", dir.display()),
        )
        .unwrap();
        let changed = look(&state, &mut cache);
        assert!(changed != applied);
        step(&state, &mut cache, true, &changed);
        assert_eq!(state.rt.config().port, 9005);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A cached hash can be stale when a same-size write keeps the metadata: once the reload read something else, the next
    /// observation reads afresh and agrees with it instead of reloading every tick.
    #[tokio::test]
    async fn a_stale_cached_hash_does_not_reload_forever() {
        let (state, dir) = state();
        let mut cache = HashCache::default();
        let file = dir.join("a.json");
        std::fs::write(&file, r#"{"type":"codex","email":"b@example.com"}"#).unwrap();
        let old = SystemTime::now() - Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(old)
            .unwrap();
        // Not racy, and cached for an earlier same-size write with the same metadata.
        let meta = std::fs::metadata(&file).unwrap();
        cache.0.insert(file.clone(), (Some(Stat::of(&meta)), [7; 32]));
        let observed = look(&state, &mut cache);
        assert_eq!(observed.auth[&file], [7; 32]);
        let applied = step(&state, &mut cache, false, &observed);
        assert!(
            state
                .rt
                .store()
                .snapshot()
                .iter()
                .any(|c| c.str("email") == Some("b@example.com"))
        );
        assert!(look(&state, &mut cache) == applied, "converges after one reload");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Go 7484a521: an empty or partial in-place write keeps the credential; valid
    /// JSON without one, or a removed file, deletes it (also the empty, partial and
    /// valid-but-empty files of Go `TestFileSynthesizerRecordsIncompleteFiles`). A file
    /// that is readable and refused (a removed provider, #49, or retired metadata) is
    /// not kept either: only a file whose contents are unknown keeps its credential.
    #[tokio::test]
    async fn incomplete_auth_writes_keep_the_credential_until_the_file_is_valid_json() {
        let (state, dir) = state();
        let file = dir.join("a.json");
        let loaded = |state: &Management| state.rt.store().snapshot().iter().any(|c| c.id == "a.json");
        std::fs::write(&file, r#"{"type":"codex","email":"a@example.com"}"#).unwrap();
        reload(&state).unwrap();
        assert!(loaded(&state));
        for (contents, kept) in [
            ("", true),
            (r#"{"type":"codex""#, true),
            (" \n", true),
            ("null", false),
            ("{}", false),
            (r#"{"type":"gemini","email":"a@example.com"}"#, false),
            (r#"{"type":"codex","email":"a@example.com","quota_probe":null}"#, false),
        ] {
            std::fs::write(&file, r#"{"type":"codex","email":"a@example.com"}"#).unwrap();
            reload(&state).unwrap();
            std::fs::write(&file, contents).unwrap();
            reload(&state).unwrap();
            assert_eq!(loaded(&state), kept, "{contents:?}");
        }
        std::fs::write(&file, r#"{"type":"codex"}"#).unwrap();
        reload(&state).unwrap();
        std::fs::remove_file(&file).unwrap();
        reload(&state).unwrap();
        assert!(!loaded(&state));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Go 5fc580b1 (TestAuthDeleteFileRemovalClassification, the service delete recheck):
    /// a removal is decided from the folder when the reload reads it, so a file removed
    /// and re-created before the reload keeps its credential, and one re-created after a
    /// reload that dropped it comes back.
    #[tokio::test]
    async fn a_removed_and_recreated_file_keeps_its_credential() {
        let (state, dir) = state();
        let file = dir.join("a.json");
        let good = |email: &str| format!(r#"{{"type":"codex","email":"{email}"}}"#);
        let loaded = |state: &Management| {
            state
                .rt
                .store()
                .snapshot()
                .iter()
                .find(|c| c.id == "a.json")
                .and_then(|c| c.str("email").map(str::to_owned))
        };
        std::fs::write(&file, good("a@example.com")).unwrap();
        reload(&state).unwrap();
        assert_eq!(loaded(&state).as_deref(), Some("a@example.com"));
        std::fs::remove_file(&file).unwrap();
        std::fs::write(&file, good("b@example.com")).unwrap();
        reload(&state).unwrap();
        assert_eq!(
            loaded(&state).as_deref(),
            Some("b@example.com"),
            "re-created before the reload"
        );
        std::fs::remove_file(&file).unwrap();
        reload(&state).unwrap();
        assert_eq!(loaded(&state), None, "removed");
        std::fs::write(&file, good("c@example.com")).unwrap();
        reload(&state).unwrap();
        assert_eq!(
            loaded(&state).as_deref(),
            Some("c@example.com"),
            "re-created after the reload"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The editor finishes the file after the scan read it half-written and before the
    /// reload publishes: the old credential is kept (a second read would see valid JSON,
    /// keep nothing and drop the credential until the next reload).
    #[tokio::test]
    async fn a_file_completed_after_the_scan_keeps_its_credential() {
        let (state, dir) = state();
        let file = dir.join("a.json");
        let good = r#"{"type":"codex","email":"a@example.com"}"#;
        std::fs::write(&file, good).unwrap();
        reload(&state).unwrap();
        let before = state.rt.store().get("a.json").unwrap();
        std::fs::write(&file, r#"{"type":"codex""#).unwrap();
        let config = (*state.rt.config()).clone();
        let scan = credentials::scan_auth_dir(&config);
        assert!(scan.incomplete.contains(&file) && scan.credentials.is_empty());
        std::fs::write(&file, good).unwrap();
        state.publish(config, Some(scan));
        let kept = state.rt.store().get("a.json").expect("kept");
        assert_eq!(kept.revision, before.revision);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Go c4c1d43e through the publication the watcher makes: a token refresh committed
    /// after the reload's scan read the file is kept, not replaced by the scanned token.
    #[tokio::test]
    async fn a_reload_scan_does_not_undo_a_refresh_committed_after_it() {
        let (state, dir) = state();
        std::fs::write(dir.join("a.json"), r#"{"type":"codex","access_token":"old"}"#).unwrap();
        reload(&state).unwrap();
        let config = (*state.rt.config()).clone();
        let scan = credentials::scan_auth_dir(&config);
        let store = state.rt.store();
        let revision = store.get("a.json").unwrap().revision;
        let mut patch = cpa_core::credential::MetadataPatch::default();
        patch.set.insert("access_token".into(), "refreshed".into());
        store.apply_patch("a.json", revision, &patch).unwrap();
        state.publish(config, Some(scan));
        assert_eq!(store.get("a.json").unwrap().metadata["access_token"], "refreshed");
        reload(&state).unwrap();
        assert_eq!(store.get("a.json").unwrap().metadata["access_token"], "refreshed");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Go c4c1d43e for upload fallbacks, which are rebuilt from a second read: a scan
    /// taken before an external edit (B) and a persist of the current credential (A)
    /// does not publish B over A, whether the auth-dir is absolute or relative (where
    /// the scan's paths are relative and the fallback's absolute).
    #[tokio::test]
    async fn a_reload_does_not_undo_a_write_to_an_upload_fallback() {
        for relative in [false, true] {
            let (state, dir) = state();
            // A relative auth-dir lives under the working directory: no relative path
            // reaches a temp folder on another Windows drive.
            let local = PathBuf::from(format!("cpa-watch-relative-{}", uuid::Uuid::new_v4()));
            if relative {
                std::fs::create_dir_all(&local).unwrap();
                std::fs::write(&state.path, format!("oauth: {{auth-dir: {}}}\n", local.display())).unwrap();
            }
            reload(&state).unwrap();
            let config = (*state.rt.config()).clone();
            assert_eq!(config.auth_dir.is_relative(), relative);
            // The path an upload records.
            let file = std::path::absolute(config.auth_dir.join("a.json")).unwrap();
            let a = r#"{"type":"gemini","v":"A"}"#;
            std::fs::write(&file, a).unwrap();
            state.fallbacks.lock().unwrap().insert(file.clone());
            reload(&state).unwrap();
            let store = state.rt.store();
            let before = store.get("a.json").unwrap();
            assert_eq!(before.metadata["v"], "A");
            let scan = credentials::scan_auth_dir(&config);
            std::fs::write(&file, r#"{"type":"gemini","v":"B"}"#).unwrap();
            let persisted = file.clone();
            crate::management::BEFORE_RECONCILE.lock().unwrap().push((
                config.auth_dir.clone(),
                Box::new(move || std::fs::write(persisted, a).unwrap()),
            ));
            state.publish(config, Some(scan));
            let after = store.get("a.json").unwrap();
            assert_eq!(
                (&after.metadata["v"], after.revision),
                (&"A".into(), before.revision),
                "relative: {relative}"
            );
            std::fs::remove_dir_all(dir).unwrap();
            if relative {
                std::fs::remove_dir_all(local).unwrap();
            }
        }
    }

    /// A fallback stays tracked while its file still holds it: when the scan read a
    /// synthesizable rewrite that the freshness check rejects, the next reload keeps the
    /// fallback; an accepted rewrite still retires it.
    #[tokio::test]
    async fn a_fallback_whose_replacement_was_rejected_stays_tracked() {
        let (state, dir) = state();
        let file = dir.join("a.json");
        let a = r#"{"type":"gemini","v":"A"}"#;
        std::fs::write(&file, a).unwrap();
        state.fallbacks.lock().unwrap().insert(file.clone());
        reload(&state).unwrap();
        let store = state.rt.store();
        let before = store.get("a.json").unwrap();
        let codex = r#"{"type":"codex","access_token":"B"}"#;
        std::fs::write(&file, codex).unwrap();
        let config = (*state.rt.config()).clone();
        let scan = credentials::scan_auth_dir(&config);
        let persisted = file.clone();
        crate::management::BEFORE_RECONCILE.lock().unwrap().push((
            config.auth_dir.clone(),
            Box::new(move || std::fs::write(persisted, a).unwrap()),
        ));
        state.publish(config, Some(scan));
        let kept = store.get("a.json").unwrap();
        assert_eq!((kept.provider.as_str(), kept.revision), ("gemini", before.revision));
        reload(&state).unwrap();
        assert_eq!(store.get("a.json").unwrap().provider, "gemini");
        assert!(state.fallbacks.lock().unwrap().contains(&file));
        std::fs::write(&file, codex).unwrap();
        reload(&state).unwrap();
        assert_eq!(store.get("a.json").unwrap().provider, "codex");
        assert!(state.fallbacks.lock().unwrap().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// #89 (Go v8.0.67 5fe203d5): an entry that cannot be read (here a symlink loop,
    /// unreadable for every user, root included) is present but unknown, not deleted:
    /// its credential stays, with its revision. The watcher observes the entry with the
    /// hash last read from it, so losing access is no change, and deleting the entry is
    /// a removal it sees and applies. This replaces the earlier policy that dropped it.
    #[tokio::test]
    async fn an_unreadable_file_keeps_its_credential_until_it_is_deleted() {
        let (state, dir) = state();
        let mut cache = HashCache::default();
        let file = dir.join("a.json");
        std::fs::write(&file, r#"{"type":"codex","email":"a@example.com"}"#).unwrap();
        let observed = look(&state, &mut cache);
        let applied = step(&state, &mut cache, false, &observed);
        let before = state.rt.store().get("a.json").expect("loaded");
        std::fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink(&file, &file).unwrap();
        assert!(matches!(
            credentials::read_auth_file(&file),
            credentials::AuthFile::Unreadable(_)
        ));
        let observed = look(&state, &mut cache);
        assert!(observed == applied, "losing access is no change");
        // A reload for another reason (a config edit, a management save) rescans.
        let applied = step(&state, &mut cache, true, &observed);
        let kept = state.rt.store().get("a.json").expect("kept while unreadable");
        assert_eq!(kept.revision, before.revision);
        assert!(look(&state, &mut cache) == applied, "converges");
        std::fs::remove_file(&file).unwrap();
        let observed = look(&state, &mut cache);
        assert!(!observed.auth.contains_key(&file), "the deletion is seen");
        step(&state, &mut cache, false, &observed);
        assert!(state.rt.store().get("a.json").is_none(), "deleted");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A [`state`] whose auth-dir is a folder of its own, `auth`, so that taking its
    /// permissions leaves the config readable; relative to the working directory when
    /// `relative`. Returns the auth folder as configured.
    fn state_with_auth_folder(relative: bool) -> (Arc<Management>, PathBuf, PathBuf) {
        let (state, dir) = state();
        let auth = if relative {
            PathBuf::from(format!("cpa-watch-relative-{}", uuid::Uuid::new_v4()))
        } else {
            dir.join("auth")
        };
        std::fs::create_dir_all(&auth).unwrap();
        std::fs::write(&state.path, format!("oauth: {{auth-dir: {}}}\n", auth.display())).unwrap();
        reload(&state).unwrap();
        assert_eq!(state.rt.config().auth_dir, auth);
        (state, dir, auth)
    }

    /// Sets `path`'s permission bits.
    fn chmod(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Whether `path`'s permissions deny this process: root, also uid 0 in a user
    /// namespace, reads through any mode. Run these tests as the caller's own uid
    /// (`ui/scripts/isolated.sh` keeps it inside a network namespace, as CI's `setpriv`
    /// does); where permissions are not enforced the test says so and checks nothing.
    fn denied(path: &Path) -> bool {
        let denied = if path.is_dir() {
            std::fs::read_dir(path).is_err()
        } else {
            std::fs::File::open(path).is_err()
        };
        if !denied {
            eprintln!("permissions are not enforced for this process (root?); skipping");
        }
        denied
    }

    /// #89 (Go v8.0.67 5fe203d5): an auth file that loses read permission keeps its
    /// credential as it was: revision, a model's 429 cooldown and a 401 block all stay,
    /// through the watcher and through a full rescan. Access restored with the same
    /// bytes changes nothing; a re-login written in its place is applied, clearing the
    /// 401; deleting the unreadable file deletes the credential.
    #[tokio::test]
    async fn an_auth_file_without_read_permission_keeps_its_credential_and_state() {
        use crate::runtime::{Outcome, Selection};
        use cpa_core::exec::{ExecError, FailureScope};
        let (state, dir, auth) = state_with_auth_folder(false);
        let file = auth.join("claude-a.json");
        let doc = |token: &str| {
            serde_json::json!({"type": "claude", "access_token": token, "refresh_token": "fake-rt", "email": "a@example.invalid"})
                .to_string()
        };
        std::fs::write(&file, doc("sk-ant-oat-old-fake")).unwrap();
        let mut cache = HashCache::default();
        let observed = look(&state, &mut cache);
        step(&state, &mut cache, false, &observed);
        let store = state.rt.store();
        let select = |model: &str| store.select(Selection::new("claude", model));
        let mut quota = ExecError::local(429, FailureScope::Model, "rate limited");
        quota.retry_after = Some(Duration::from_secs(3600));
        select("q").unwrap().complete(Outcome::Failure(quota));
        let unauthorized = ExecError::local(
            401,
            FailureScope::Credential,
            "authentication_error: OAuth access token",
        );
        select("m").unwrap().complete(Outcome::Failure(unauthorized));
        // The 401 saves the credential's status to its file; the watcher reloads that.
        let observed = look(&state, &mut cache);
        let applied = step(&state, &mut cache, false, &observed);
        let before = store.get("claude-a.json").unwrap();
        let cooldowns = || {
            let all = store.cooldowns("claude-a.json");
            all.iter().map(|c| (c.model.clone(), c.status)).collect::<Vec<_>>()
        };
        assert_eq!(cooldowns(), [("m".to_owned(), 401), ("q".to_owned(), 429)]);
        let kept = |why: &str| {
            let now = store.get("claude-a.json").unwrap_or_else(|| panic!("{why}: dropped"));
            assert_eq!(now.revision, before.revision, "{why}");
            assert!(select("m").is_none(), "{why}: the 401 block stays");
            assert_eq!(cooldowns(), [("m".to_owned(), 401), ("q".to_owned(), 429)], "{why}");
        };
        chmod(&file, 0o000);
        if !denied(&file) {
            std::fs::remove_dir_all(dir).unwrap();
            return;
        }
        let observed = look(&state, &mut cache);
        assert!(observed == applied, "losing access is no change");
        reload(&state).unwrap();
        kept("full rescan");
        let applied = step(&state, &mut cache, false, &observed);
        kept("watcher reload");
        assert!(look(&state, &mut cache) == applied, "converges");
        chmod(&file, 0o600);
        let observed = look(&state, &mut cache);
        assert!(observed == applied, "the same bytes are no change");
        step(&state, &mut cache, false, &observed);
        reload(&state).unwrap();
        kept("access restored");
        // A re-login replaces the unreadable file.
        chmod(&file, 0o000);
        let staged = auth.join("claude-a.json.tmp");
        std::fs::write(&staged, doc("sk-ant-oat-new-fake")).unwrap();
        std::fs::rename(&staged, &file).unwrap();
        let observed = look(&state, &mut cache);
        step(&state, &mut cache, false, &observed);
        let lease = select("m").expect("the re-login is selectable at once");
        assert_eq!(lease.credential.str("access_token"), Some("sk-ant-oat-new-fake"));
        drop(lease);
        assert!(select("q").is_none(), "the 429 cooldown survives the re-login");
        assert_eq!(cooldowns(), [("q".to_owned(), 429)]);
        chmod(&file, 0o000);
        std::fs::remove_file(&file).unwrap();
        let observed = look(&state, &mut cache);
        step(&state, &mut cache, false, &observed);
        assert!(store.get("claude-a.json").is_none(), "deleted while unreadable");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// #89 (Go v8.0.67 57a12996): an auth folder that exists but cannot be listed keeps
    /// every credential loaded from it, with its revision, and the watcher sees no
    /// change (no removal is logged or pushed to a remote store). A credential replaced
    /// or deleted before access was lost is not brought back, and a file deleted while
    /// the folder was unreadable is dropped once it can be listed again. Both an
    /// absolute and a relative auth-dir, with an upload fallback recorded under its
    /// absolute path.
    #[tokio::test]
    async fn an_auth_folder_without_read_permission_keeps_its_credentials() {
        for relative in [false, true] {
            let (state, dir, auth) = state_with_auth_folder(relative);
            let store = state.rt.store();
            let account = |email: &str| format!(r#"{{"type":"codex","email":"{email}"}}"#);
            std::fs::write(auth.join("a.json"), account("old@example.invalid")).unwrap();
            std::fs::write(auth.join("b.json"), account("b@example.invalid")).unwrap();
            std::fs::write(auth.join("gone.json"), account("gone@example.invalid")).unwrap();
            let upload = std::path::absolute(auth.join("upload.json")).unwrap();
            std::fs::write(&upload, r#"{"type":"gemini","email":"u@example.invalid"}"#).unwrap();
            state.fallbacks.lock().unwrap().insert(upload.clone());
            let mut cache = HashCache::default();
            let observed = look(&state, &mut cache);
            step(&state, &mut cache, false, &observed);
            // Replaced and deleted before access is lost.
            std::fs::write(auth.join("a.json"), account("new@example.invalid")).unwrap();
            std::fs::remove_file(auth.join("gone.json")).unwrap();
            let observed = look(&state, &mut cache);
            let applied = step(&state, &mut cache, false, &observed);
            let ids = || {
                let mut ids: Vec<(String, u64)> = store
                    .snapshot()
                    .iter()
                    .filter(|c| matches!(c.source, Source::File(_)))
                    .map(|c| (c.id.clone(), c.revision))
                    .collect();
                ids.sort();
                ids
            };
            let before = ids();
            assert_eq!(
                before.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
                ["a.json", "b.json", "upload.json"],
                "relative: {relative}"
            );
            // 0o300 lists nothing but still opens files; 0o000 does neither.
            for mode in [0o300, 0o000] {
                chmod(&auth, mode);
                if !denied(&auth) {
                    chmod(&auth, 0o755);
                    std::fs::remove_dir_all(&dir).unwrap();
                    if relative {
                        std::fs::remove_dir_all(&auth).unwrap();
                    }
                    return;
                }
                let observed = look(&state, &mut cache);
                assert!(observed == applied, "relative: {relative}, {mode:o}: no change");
                reload(&state).unwrap();
                assert_eq!(ids(), before, "relative: {relative}, {mode:o}: full rescan");
                let applied = step(&state, &mut cache, true, &observed);
                assert_eq!(ids(), before, "relative: {relative}, {mode:o}: watcher reload");
                assert!(
                    look(&state, &mut cache) == applied,
                    "relative: {relative}, {mode:o}: converges"
                );
                assert!(
                    state.fallbacks.lock().unwrap().contains(&upload),
                    "the fallback stays tracked"
                );
                let a = store.get("a.json").unwrap();
                assert_eq!(
                    a.str("email"),
                    Some("new@example.invalid"),
                    "the replaced account stays replaced"
                );
                assert!(store.get("gone.json").is_none(), "a deleted credential stays deleted");
            }
            chmod(&auth, 0o700);
            std::fs::remove_file(auth.join("b.json")).unwrap();
            let observed = look(&state, &mut cache);
            assert!(observed != applied, "the deletion is seen once the folder lists again");
            step(&state, &mut cache, false, &observed);
            let after = ids();
            assert_eq!(
                after,
                [before[0].clone(), before[2].clone()],
                "relative: {relative}: only b.json is dropped, the rest keep their revisions"
            );
            std::fs::remove_dir_all(&dir).unwrap();
            if relative {
                std::fs::remove_dir_all(&auth).unwrap();
            }
        }
    }

    /// An auth folder that also holds the config file and cannot be listed: the config
    /// is still observed as the config file only, so nothing changes and nothing reloads.
    #[tokio::test]
    async fn an_unlistable_auth_folder_holding_the_config_is_no_change() {
        let (state, dir) = state();
        std::fs::write(dir.join("a.json"), r#"{"type":"codex","email":"a@example.invalid"}"#).unwrap();
        let mut cache = HashCache::default();
        let observed = look(&state, &mut cache);
        let applied = step(&state, &mut cache, true, &observed);
        let before = state.rt.store().get("a.json").unwrap();
        chmod(&dir, 0o300);
        if !denied(&dir) {
            chmod(&dir, 0o755);
            std::fs::remove_dir_all(dir).unwrap();
            return;
        }
        let observed = look(&state, &mut cache);
        let unchanged = observed == applied;
        let applied = step(&state, &mut cache, true, &observed);
        let kept = state.rt.store().get("a.json").map(|c| c.revision);
        let converged = look(&state, &mut cache) == applied;
        chmod(&dir, 0o755);
        assert!(unchanged, "no change");
        assert_eq!(kept, Some(before.revision));
        assert!(converged);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Go c4c1d43e with #89: a scan that read new bytes from a file that is unreadable
    /// by the time it publishes cannot be checked against the file, so the loaded
    /// credential stays; the new bytes are applied once the file can be read again.
    #[tokio::test]
    async fn a_scan_of_a_file_that_became_unreadable_keeps_the_loaded_credential() {
        let (state, dir, auth) = state_with_auth_folder(false);
        let file = auth.join("a.json");
        let account = |email: &str| format!(r#"{{"type":"codex","email":"{email}"}}"#);
        std::fs::write(&file, account("a@example.invalid")).unwrap();
        reload(&state).unwrap();
        let store = state.rt.store();
        let before = store.get("a.json").unwrap();
        std::fs::write(&file, account("b@example.invalid")).unwrap();
        let config = (*state.rt.config()).clone();
        let stale = credentials::scan_auth_dir(&config);
        chmod(&file, 0o000);
        if !denied(&file) {
            std::fs::remove_dir_all(dir).unwrap();
            return;
        }
        state.publish(config, Some(stale));
        assert_eq!(store.get("a.json").unwrap().revision, before.revision);
        chmod(&file, 0o600);
        reload(&state).unwrap();
        assert_eq!(store.get("a.json").unwrap().str("email"), Some("b@example.invalid"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// #89: the credential an unreadable file keeps is the store's own when the scan is
    /// published, not when it was taken: a token refresh committed in between (here at
    /// the last moment before the swap) is kept, not undone by an older copy.
    #[tokio::test]
    async fn a_refresh_committed_after_an_unreadable_scan_is_kept() {
        let (state, dir) = state();
        let file = dir.join("a.json");
        std::fs::write(&file, r#"{"type":"codex","access_token":"old-fake"}"#).unwrap();
        reload(&state).unwrap();
        std::fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink(&file, &file).unwrap();
        let config = (*state.rt.config()).clone();
        let scan = credentials::scan_auth_dir(&config);
        assert!(scan.unreadable.contains(&file), "the loop is unreadable");
        let store = state.rt.store();
        let refreshed = Arc::new(std::sync::Mutex::new(None));
        crate::management::BEFORE_RECONCILE
            .lock()
            .unwrap()
            .push((config.auth_dir.clone(), {
                let (store, refreshed, file) = (store.clone(), refreshed.clone(), file.clone());
                Box::new(move || {
                    // The refresh writes a regular file in the loop's place.
                    std::fs::remove_file(&file).unwrap();
                    let revision = store.get("a.json").unwrap().revision;
                    let mut patch = cpa_core::credential::MetadataPatch::default();
                    patch.set.insert("access_token".into(), "refreshed-fake".into());
                    *refreshed.lock().unwrap() = Some(store.apply_patch("a.json", revision, &patch).unwrap().revision);
                })
            }));
        state.publish(config, Some(scan));
        let now = store.get("a.json").unwrap();
        assert_eq!(now.metadata["access_token"], "refreshed-fake");
        assert_eq!(Some(now.revision), *refreshed.lock().unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Waits up to 5 seconds for `done`.
    async fn wait_until(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "timed out: {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Sol review of 6785ebb8, finding 1: the watcher observed new bytes B and the
    /// reload's read then failed, so the loaded credential A was kept, correctly, but B was
    /// recorded as applied: once the file could be read again, nothing reloaded it. Now
    /// what the reload could not read is pending, apart from the applied snapshot, and the
    /// watcher reloads it once it can be read, with no new write. Here through the
    /// running watcher, for a re-login, a revocation (`{}`) and a removed provider's file;
    /// for the first, a lasting denial also does not reload again and again.
    #[tokio::test]
    async fn a_write_seen_before_its_read_failed_is_applied_once_readable() {
        let account = |token: &str, email: &str| {
            serde_json::json!({"type": "codex", "access_token": token, "email": email}).to_string()
        };
        let cases = [
            (
                "re-login",
                account("new-fake", "new@example.invalid"),
                Some("new@example.invalid"),
            ),
            ("revocation", "{}".to_owned(), None),
            (
                "removed provider",
                r#"{"type":"gemini","email":"g@example.invalid"}"#.to_owned(),
                None,
            ),
        ];
        for (i, (case, replacement, want)) in cases.into_iter().enumerate() {
            let (state, dir, auth) = state_with_auth_folder(false);
            let file = auth.join("a.json");
            let probe = auth.join("probe");
            std::fs::write(&probe, "").unwrap();
            chmod(&probe, 0o000);
            if !denied(&probe) {
                std::fs::remove_dir_all(dir).unwrap();
                return;
            }
            std::fs::remove_file(&probe).unwrap();
            std::fs::write(&file, account("old-fake", "old@example.invalid")).unwrap();
            let store = state.rt.store();
            let email = || {
                store
                    .get("a.json")
                    .map(|c| c.str("email").unwrap_or_default().to_owned())
            };
            let watcher = start(&state);
            wait_until(&format!("{case}: A loaded"), || {
                email().as_deref() == Some("old@example.invalid")
            })
            .await;
            let before = store.get("a.json").unwrap().revision;
            // Denies the reload's read once, after the watcher observed the new bytes.
            let scans = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            BEFORE_SCAN.lock().unwrap().push((auth.clone(), {
                let (scans, file) = (scans.clone(), file.clone());
                Arc::new(move || {
                    if scans.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                        chmod(&file, 0o000);
                    }
                })
            }));
            let staged = auth.join("a.json.tmp");
            std::fs::write(&staged, &replacement).unwrap();
            std::fs::rename(&staged, &file).unwrap();
            let scanned = || scans.load(std::sync::atomic::Ordering::SeqCst);
            wait_until(&format!("{case}: the reload ran"), || scanned() >= 1).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(
                store.get("a.json").map(|c| c.revision),
                Some(before),
                "{case}: A is kept while the file cannot be read"
            );
            if i == 0 {
                // A lasting denial: retries look, but do not reload, every PENDING_RETRY.
                tokio::time::sleep(PENDING_RETRY + Duration::from_millis(700)).await;
                assert_eq!(scanned(), 1, "{case}: no reload while the file stays unreadable");
                assert_eq!(store.get("a.json").map(|c| c.revision), Some(before), "{case}");
            }
            chmod(&file, 0o600);
            match want {
                Some(want) => wait_until(&format!("{case}: B applied"), || email().as_deref() == Some(want)).await,
                None => {
                    wait_until(&format!("{case}: the credential removed"), || {
                        store.get("a.json").is_none()
                    })
                    .await
                }
            }
            watcher.abort();
            let _ = watcher.await;
            BEFORE_SCAN.lock().unwrap().retain(|(d, _)| *d != auth);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    /// What a stage-fault case expects once the fault clears.
    #[derive(Clone, Copy)]
    enum Want {
        Email(&'static str),
        Disabled,
        Removed,
    }

    const OLD: &str = r#"{"type":"codex","access_token":"old-fake","email":"old@example.invalid"}"#;
    const RELOGIN: &str = r#"{"type":"codex","access_token":"new-fake","email":"new@example.invalid"}"#;
    const DISABLED: &str =
        r#"{"type":"codex","access_token":"old-fake","email":"old@example.invalid","disabled":true}"#;
    const REMOVED: &str = r#"{"type":"gemini","email":"g@example.invalid"}"#;

    /// A running watcher over a private auth folder holding `a.json` = [`OLD`], loaded.
    async fn watched() -> (Arc<Management>, PathBuf, PathBuf, PathBuf, tokio::task::JoinHandle<()>) {
        let (state, dir, auth) = state_with_auth_folder(false);
        let file = auth.join("a.json");
        std::fs::write(&file, OLD).unwrap();
        let watcher = start(&state);
        let store = state.rt.store();
        wait_until("A loaded", || store.get("a.json").is_some()).await;
        (state, dir, auth, file, watcher)
    }

    /// Writes `bytes` to `file` the way a login does: a new file renamed into place.
    fn replace(file: &Path, bytes: &str) {
        let staged = file.with_extension("json.tmp");
        std::fs::write(&staged, bytes).unwrap();
        std::fs::rename(&staged, file).unwrap();
    }

    fn holds(store: &crate::runtime::CredentialStore, want: Want) -> bool {
        let a = store.get("a.json");
        match want {
            Want::Email(email) => a.is_some_and(|c| c.str("email") == Some(email) && !c.disabled),
            Want::Disabled => a.is_some_and(|c| c.disabled),
            Want::Removed => a.is_none(),
        }
    }

    /// One stage-fault case: a write lands while a read of it fails at `stage`; the
    /// loaded credential stays while the error lasts; the error then clears with no
    /// file event at all, and the write is applied.
    async fn stage_case(stage: faults::Stage, case: &str, replacement: &str, want: Want) {
        let (state, dir, auth, file, watcher) = watched().await;
        let store = state.rt.store();
        let before = store.get("a.json").unwrap().revision;
        let target = if stage == faults::Stage::LookListing {
            &auth
        } else {
            &file
        };
        let hits = faults::arm(stage, target);
        replace(&file, replacement);
        let what = format!("{stage:?} {case}");
        wait_until(&format!("{what}: the fault was hit"), || {
            hits.load(std::sync::atomic::Ordering::SeqCst) > 0
        })
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            store.get("a.json").map(|c| (c.revision, c.disabled)),
            Some((before, false)),
            "{what}: A is kept while the error lasts"
        );
        faults::disarm(stage, target);
        wait_until(
            &format!("{what}: applied with no event after the error cleared"),
            || holds(store, want),
        )
        .await;
        watcher.abort();
        let _ = watcher.await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Sol re-review of eb9a6ec8: a transient read error at any stage of the path from
    /// the watcher's look to the publication leaves the write pending, and it is applied
    /// once the error clears, through the idle retry alone (no file event after it), for
    /// a re-login, a revocation by `disabled`, valid JSON without a credential (`{}`) and
    /// a removed provider's file. The look stage covers a failed file read and a folder
    /// listing that fails; the scan stage is the earlier fix, kept as a control. The
    /// freshness check only runs for a scanned credential that replaces the loaded one,
    /// so it has no `{}` or removed-provider case.
    #[tokio::test]
    async fn a_transient_error_at_any_stage_is_retried_without_an_event() {
        use faults::Stage::*;
        let mut cases = Vec::new();
        for stage in [Look, LookListing, Scan, Freshness] {
            for (case, replacement, want) in [
                ("re-login", RELOGIN, Want::Email("new@example.invalid")),
                ("disabled", DISABLED, Want::Disabled),
                ("{}", "{}", Want::Removed),
                ("removed provider", REMOVED, Want::Removed),
            ] {
                if stage == Freshness && matches!(want, Want::Removed) {
                    continue;
                }
                cases.push(stage_case(stage, case, replacement, want));
            }
        }
        futures_util::future::join_all(cases).await;
    }

    /// Counts the reloads of `auth`'s folder.
    fn count_reloads(auth: &Path) -> Arc<std::sync::atomic::AtomicUsize> {
        let reloads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        BEFORE_SCAN.lock().unwrap().push((auth.to_owned(), {
            let reloads = reloads.clone();
            Arc::new(move || {
                reloads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })
        }));
        reloads
    }

    /// A fault that lasts: at the freshness check, each idle retry reloads once (about
    /// every PENDING_RETRY, never every tick) and the loaded credential stays; at the
    /// look, the scan applies the write and later looks that still fail report the
    /// applied bytes, so nothing reloads every tick either. The write is applied once the
    /// fault clears, or at once when only the look fails.
    #[tokio::test]
    async fn a_lasting_fault_retries_slowly_and_never_drops_the_credential() {
        use faults::Stage::*;
        let run = |stage: faults::Stage| async move {
            let (state, dir, auth, file, watcher) = watched().await;
            let store = state.rt.store();
            let before = store.get("a.json").unwrap().revision;
            let target = if stage == LookListing {
                auth.clone()
            } else {
                file.clone()
            };
            let reloads = count_reloads(&auth);
            faults::arm(stage, &target);
            replace(&file, RELOGIN);
            let window = PENDING_RETRY * 2 + Duration::from_millis(1500);
            let started = Instant::now();
            while started.elapsed() < window {
                if stage == Freshness {
                    assert_eq!(
                        store.get("a.json").map(|c| c.revision),
                        Some(before),
                        "{stage:?}: A kept"
                    );
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let n = reloads.load(std::sync::atomic::Ordering::SeqCst);
            assert!(n <= 5, "{stage:?}: {n} reloads in {window:?}");
            if stage == Freshness {
                assert!(n >= 2, "{stage:?}: the idle retry reloads ({n})");
            } else {
                assert!(
                    holds(store, Want::Email("new@example.invalid")),
                    "{stage:?}: the scan applied it"
                );
            }
            faults::disarm(stage, &target);
            wait_until(&format!("{stage:?}: applied"), || {
                holds(store, Want::Email("new@example.invalid"))
            })
            .await;
            watcher.abort();
            let _ = watcher.await;
            BEFORE_SCAN.lock().unwrap().retain(|(d, _)| *d != auth);
            std::fs::remove_dir_all(dir).unwrap();
        };
        futures_util::future::join_all([run(Freshness), run(Look), run(LookListing)]).await;
    }

    /// Races with a write still pending after a failed freshness check: deleting the
    /// file removes the credential (the fault does not hold it); a second write replaces
    /// the pending one, and only it is applied once the fault clears; a token refresh
    /// that rewrites the file in the meantime is kept, and the pending re-login it
    /// overwrote never comes back.
    #[tokio::test]
    async fn a_pending_write_races_with_deletion_replacement_and_refresh() {
        use faults::Stage::Freshness;
        let pending = |state: Arc<Management>, file: PathBuf| async move {
            let hits = faults::arm(Freshness, &file);
            replace(&file, RELOGIN);
            wait_until("rejected at publication", || {
                hits.load(std::sync::atomic::Ordering::SeqCst) > 0
            })
            .await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert!(holds(state.rt.store(), Want::Email("old@example.invalid")));
        };
        // Deletion.
        let deletion = async {
            let (state, dir, _, file, watcher) = watched().await;
            pending(state.clone(), file.clone()).await;
            std::fs::remove_file(&file).unwrap();
            wait_until("deleted", || state.rt.store().get("a.json").is_none()).await;
            faults::disarm(Freshness, &file);
            tokio::time::sleep(PENDING_RETRY + Duration::from_millis(500)).await;
            assert!(
                state.rt.store().get("a.json").is_none(),
                "a deleted credential stays deleted"
            );
            watcher.abort();
            let _ = watcher.await;
            std::fs::remove_dir_all(dir).unwrap();
        };
        // Replacement.
        let replacement = async {
            let (state, dir, _, file, watcher) = watched().await;
            pending(state.clone(), file.clone()).await;
            replace(
                &file,
                r#"{"type":"codex","access_token":"third-fake","email":"third@example.invalid"}"#,
            );
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(
                holds(state.rt.store(), Want::Email("old@example.invalid")),
                "still rejected"
            );
            faults::disarm(Freshness, &file);
            let store = state.rt.store();
            wait_until("the latest write applied", || {
                holds(store, Want::Email("third@example.invalid"))
            })
            .await;
            watcher.abort();
            let _ = watcher.await;
            std::fs::remove_dir_all(dir).unwrap();
        };
        // Refresh.
        let refresh = async {
            let (state, dir, _, file, watcher) = watched().await;
            pending(state.clone(), file.clone()).await;
            let store = state.rt.store();
            let revision = store.get("a.json").unwrap().revision;
            let mut patch = cpa_core::credential::MetadataPatch::default();
            patch.set.insert("access_token".into(), "refreshed-fake".into());
            let refreshed = store.apply_patch("a.json", revision, &patch).unwrap().revision;
            faults::disarm(Freshness, &file);
            tokio::time::sleep(PENDING_RETRY + Duration::from_millis(700)).await;
            let now = store.get("a.json").unwrap();
            assert_eq!(
                (now.metadata["access_token"].as_str(), now.str("email"), now.revision),
                (Some("refreshed-fake"), Some("old@example.invalid"), refreshed),
                "the refresh is kept and the overwritten re-login never comes back"
            );
            watcher.abort();
            let _ = watcher.await;
            std::fs::remove_dir_all(dir).unwrap();
        };
        tokio::join!(deletion, replacement, refresh);
    }

    /// Finding 1 step by step, as the watcher loop drives it: the failed read leaves A
    /// loaded (revision, no removal), the file pending and the applied snapshot equal to
    /// the observation, so a lasting denial is no change; once readable, `ready` says so
    /// and the same observation applies B.
    #[tokio::test]
    async fn a_failed_read_is_pending_until_the_file_can_be_read() {
        let (state, dir, auth) = state_with_auth_folder(false);
        let file = auth.join("a.json");
        let account = |email: &str| format!(r#"{{"type":"codex","email":"{email}"}}"#);
        std::fs::write(&file, account("old@example.invalid")).unwrap();
        let mut cache = HashCache::default();
        let observed = look(&state, &mut cache);
        let (_, pending) = step_pending(&state, &mut cache, false, &observed);
        assert!(pending.is_empty());
        let store = state.rt.store();
        let before = store.get("a.json").unwrap().revision;
        std::fs::write(&file, account("new@example.invalid")).unwrap();
        let observed = look(&state, &mut cache); // Reads B.
        chmod(&file, 0o000); // Before the reload's read.
        if !denied(&file) {
            std::fs::remove_dir_all(dir).unwrap();
            return;
        }
        let (applied, pending) = step_pending(&state, &mut cache, false, &observed);
        assert_eq!(store.get("a.json").map(|c| c.revision), Some(before), "A kept");
        assert_eq!(pending.files.iter().collect::<Vec<_>>(), [&file]);
        assert!(look(&state, &mut cache) == applied, "a lasting denial is no change");
        assert!(!pending.ready(), "nothing to retry while unreadable");
        chmod(&file, 0o600);
        let observed = look(&state, &mut cache);
        assert!(observed == applied, "B was recorded as observed");
        assert!(pending.ready(), "readable again: retry");
        let (_, pending) = step_pending(&state, &mut cache, false, &observed);
        assert_eq!(store.get("a.json").unwrap().str("email"), Some("new@example.invalid"));
        assert!(pending.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Finding 1 with a refresh: a token refresh committed while the file is pending
    /// (it writes a readable file in place) is what the retry finds and keeps; it is not
    /// undone by an older copy, and nothing stays pending.
    #[tokio::test]
    async fn a_refresh_committed_while_a_read_is_pending_is_kept() {
        let (state, dir, auth) = state_with_auth_folder(false);
        let file = auth.join("a.json");
        std::fs::write(&file, r#"{"type":"codex","access_token":"old-fake"}"#).unwrap();
        let mut cache = HashCache::default();
        let observed = look(&state, &mut cache);
        step(&state, &mut cache, false, &observed);
        std::fs::write(&file, r#"{"type":"codex","access_token":"edited-fake"}"#).unwrap();
        let observed = look(&state, &mut cache);
        chmod(&file, 0o000);
        if !denied(&file) {
            std::fs::remove_dir_all(dir).unwrap();
            return;
        }
        let (_, pending) = step_pending(&state, &mut cache, false, &observed);
        assert!(!pending.is_empty());
        let store = state.rt.store();
        let revision = store.get("a.json").unwrap().revision;
        let mut patch = cpa_core::credential::MetadataPatch::default();
        patch.set.insert("access_token".into(), "refreshed-fake".into());
        let refreshed = store.apply_patch("a.json", revision, &patch).unwrap().revision;
        assert!(pending.ready(), "the refresh wrote a readable file");
        let observed = look(&state, &mut cache);
        let (_, pending) = step_pending(&state, &mut cache, false, &observed);
        let now = store.get("a.json").unwrap();
        assert_eq!(
            (now.metadata["access_token"].as_str(), now.revision),
            (Some("refreshed-fake"), refreshed)
        );
        assert!(pending.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Sol review of 6785ebb8, finding 2: a listing that fails after reaching some files
    /// settles those files by what was found; only the files it never reached are
    /// unknown. A file read and refused (a removed provider) drops its credential, as do
    /// a file found gone and a link to `/dev/null`, though the folder-wide rule would
    /// have kept them; a file not reached keeps its credential and revision. The watcher
    /// restores from its cache only the files the listing did not reach. A complete
    /// listing afterwards, with no byte changed, changes nothing more.
    #[tokio::test]
    async fn a_partial_listing_settles_the_files_it_reached() {
        let (state, dir, auth) = state_with_auth_folder(false);
        let account = |email: &str| format!(r#"{{"type":"codex","email":"{email}"}}"#);
        for name in ["a", "gone", "null", "kept"] {
            std::fs::write(
                auth.join(format!("{name}.json")),
                account(&format!("{name}@example.invalid")),
            )
            .unwrap();
        }
        let mut cache = HashCache::default();
        let observed = look(&state, &mut cache);
        step(&state, &mut cache, false, &observed);
        let store = state.rt.store();
        let kept = store.get("kept.json").unwrap().revision;
        std::fs::write(auth.join("a.json"), r#"{"type":"gemini","email":"a@example.invalid"}"#).unwrap();
        std::fs::remove_file(auth.join("gone.json")).unwrap();
        std::fs::remove_file(auth.join("null.json")).unwrap();
        std::os::unix::fs::symlink("/dev/null", auth.join("null.json")).unwrap();
        let partial = || credentials::Listing {
            dir: auth.clone(),
            paths: ["a", "gone", "null"].map(|n| auth.join(format!("{n}.json"))).to_vec(),
            failure: Some(std::io::Error::from_raw_os_error(libc::EIO)),
        };
        let observed = observe_listing(&state, &mut cache, partial()).snapshot;
        assert_eq!(
            observed
                .auth
                .keys()
                .map(|p| p.file_name().unwrap().to_str().unwrap())
                .collect::<Vec<_>>(),
            ["a.json", "kept.json"],
            "only the file not reached is restored from the cache"
        );
        let config = (*state.rt.config()).clone();
        let scan = credentials::scan_listing(&config, partial());
        assert_eq!(scan.unlisted.as_deref(), Some(auth.as_path()));
        state.publish(config, Some(scan));
        let ids = || {
            let mut ids: Vec<(String, u64)> = store
                .snapshot()
                .iter()
                .filter(|c| matches!(c.source, Source::File(_)))
                .map(|c| (c.id.clone(), c.revision))
                .collect();
            ids.sort();
            ids
        };
        assert_eq!(ids(), [("kept.json".to_owned(), kept)]);
        reload(&state).unwrap(); // Complete listing, nothing written.
        assert_eq!(ids(), [("kept.json".to_owned(), kept)]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The scan counts the entries the watcher observes: a credential whose file is
    /// replaced by a link to `/dev/null` is dropped by the reload that replacement
    /// triggers, so deleting the link (which the watcher cannot see, having never
    /// observed it) leaves nothing live.
    #[tokio::test]
    async fn a_file_replaced_by_a_special_file_link_drops_its_credential() {
        let (state, dir) = state();
        let file = dir.join("a.json");
        std::fs::write(&file, r#"{"type":"codex","email":"a@example.com"}"#).unwrap();
        reload(&state).unwrap();
        let loaded = |state: &Management| state.rt.store().snapshot().iter().any(|c| c.id == "a.json");
        assert!(loaded(&state));
        let capture = Capture::default();
        let _guard = capture.install();
        let watcher = start(&state);
        tokio::time::sleep(Duration::from_millis(500)).await;
        std::fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink("/dev/null", &file).unwrap();
        capture.wait_for("auth file changed (REMOVE): a.json").await;
        tokio::time::timeout(Duration::from_secs(3), async {
            while loaded(&state) {
                tokio::time::sleep(TICK).await;
            }
        })
        .await
        .expect("the reload the replacement triggered drops the credential");
        std::fs::remove_file(&file).unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!loaded(&state));
        watcher.abort();
        let _ = watcher.await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<std::sync::Mutex<Vec<u8>>>, Arc<tokio::sync::Notify>);
    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            self.1.notify_one();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Capture {
        fn install(&self) -> tracing::subscriber::DefaultGuard {
            let writer = self.clone();
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .without_time()
                .with_writer(move || writer.clone())
                .finish();
            tracing::subscriber::set_default(subscriber)
        }

        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }

        async fn wait_for(&self, needle: &str) {
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let notified = self.1.notified();
                    if self.text().contains(needle) {
                        break;
                    }
                    notified.await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{needle:?} was not logged:\n{}", self.text()));
        }
    }

    /// Go `updateInternalLocked` through the watcher's incremental reload: a credential
    /// an upstream 401 blocked is selectable as soon as its file holds a new access token
    /// (a re-login), with no restart, and a model's 429 cooldown on it stays. An edit
    /// that keeps the tokens keeps the block.
    #[tokio::test]
    async fn a_rewritten_token_clears_its_401_block_and_keeps_a_429() {
        use crate::runtime::{Outcome, Selection};
        use cpa_core::exec::{ExecError, FailureScope};
        let (state, dir) = state();
        let file = dir.join("claude-a.json");
        let doc = |token: &str, email: &str| {
            serde_json::json!({"type": "claude", "access_token": token, "refresh_token": "fake-rt", "email": email})
                .to_string()
        };
        std::fs::write(&file, doc("sk-ant-oat-old-fake", "a@example.invalid")).unwrap();
        let watcher = start(&state);
        let store = state.rt.store();
        let wait = |want: &'static str| {
            let store = store.clone();
            async move {
                tokio::time::timeout(Duration::from_secs(5), async {
                    while store.get("claude-a.json").is_none_or(|c| c.str("email") != Some(want)) {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .unwrap_or_else(|_| panic!("{want} was not reloaded"));
            }
        };
        wait("a@example.invalid").await;
        let select = |model: &str| store.select(Selection::new("claude", model));
        let mut quota = ExecError::local(429, FailureScope::Model, "rate limited");
        quota.retry_after = Some(Duration::from_secs(3600));
        select("q").unwrap().complete(Outcome::Failure(quota));
        let unauthorized = ExecError::local(
            401,
            FailureScope::Credential,
            "authentication_error: OAuth access token",
        );
        select("m").unwrap().complete(Outcome::Failure(unauthorized));
        assert!(select("m").is_none(), "the 401 blocks the credential");
        // Same tokens, another email: Go keeps the block.
        std::fs::write(&file, doc("sk-ant-oat-old-fake", "b@example.invalid")).unwrap();
        wait("b@example.invalid").await;
        assert!(select("m").is_none(), "an edit that keeps the tokens keeps the block");
        std::fs::write(&file, doc("sk-ant-oat-new-fake", "c@example.invalid")).unwrap();
        wait("c@example.invalid").await;
        let lease = select("m").expect("the new token is selectable at once");
        assert_eq!(lease.credential.str("access_token"), Some("sk-ant-oat-new-fake"));
        drop(lease);
        assert!(select("q").is_none(), "the 429 cooldown survives the reload");
        let left = store.cooldowns("claude-a.json");
        assert_eq!(
            left.iter().map(|c| (c.model.as_str(), c.status)).collect::<Vec<_>>(),
            [("q", 429)]
        );
        watcher.abort();
        let _ = watcher.await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn edit_before_first_stable_observation_is_logged_once() {
        let (state, dir) = state();
        let capture = Capture::default();
        let _guard = capture.install();
        let watcher = start(&state);
        std::fs::write(
            &state.path,
            format!("port: 9001\noauth: {{auth-dir: {}}}\n", dir.display()),
        )
        .unwrap();
        capture.wait_for("port: 0 -> 9001").await;
        watcher.abort();
        let _ = watcher.await;
        assert_eq!(capture.text().matches("port: 0 -> 9001").count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn edit_between_runtime_load_and_start_is_applied() {
        let (state, dir) = state();
        let capture = Capture::default();
        let _guard = capture.install();
        // The runtime holds port 0; the file changes before the watcher hashes it.
        std::fs::write(
            &state.path,
            format!("port: 9001\noauth: {{auth-dir: {}}}\n", dir.display()),
        )
        .unwrap();
        let watcher = start(&state);
        capture.wait_for("config successfully reloaded").await;
        assert_eq!(state.rt.config().port, 9001);
        watcher.abort();
        let _ = watcher.await;
        let text = capture.text();
        assert_eq!(text.matches("port: 0 -> 9001").count(), 1);
        assert_eq!(text.matches("config successfully reloaded").count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn unchanged_config_is_read_quietly_on_first_observation() {
        let (state, dir) = state();
        let capture = Capture::default();
        let _guard = capture.install();
        let initial = state.rt.config();
        let watcher = start(&state);
        // The first reload publishes a new snapshot. An auth file created after that
        // is logged by a later pass of the loop, so every line of the first pass
        // precedes its log line.
        tokio::time::timeout(Duration::from_secs(3), async {
            while Arc::ptr_eq(&state.rt.config(), &initial) {
                tokio::time::sleep(TICK).await;
            }
        })
        .await
        .unwrap();
        std::fs::write(dir.join("a.json"), "{}").unwrap();
        capture.wait_for("auth file changed (CREATE): a.json").await;
        watcher.abort();
        let _ = watcher.await;
        let text = capture.text();
        assert!(!text.contains("config successfully reloaded"), "{text}");
        assert!(!text.contains("config changes detected"), "{text}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A missing auth folder is watched through its parent; once it is created (empty,
    /// so nothing reloads), files written into it are still seen.
    #[tokio::test]
    async fn auth_folder_created_after_start_is_followed() {
        let (state, dir) = state();
        let auth = dir.join("later");
        std::fs::write(&state.path, format!("oauth: {{auth-dir: {}}}\n", auth.display())).unwrap();
        reload(&state).unwrap();
        let capture = Capture::default();
        let _guard = capture.install();
        let initial = state.rt.config();
        let watcher = start(&state);
        tokio::time::timeout(Duration::from_secs(3), async {
            while Arc::ptr_eq(&state.rt.config(), &initial) {
                tokio::time::sleep(TICK).await;
            }
        })
        .await
        .unwrap();
        std::fs::create_dir(&auth).unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        std::fs::write(auth.join("a.json"), "{}").unwrap();
        capture.wait_for("auth file changed (CREATE): a.json").await;
        watcher.abort();
        let _ = watcher.await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn hash_cache_rereads_only_changed_or_racy_files() {
        let dir = std::env::temp_dir().join(format!("cpa-watch-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.json");
        std::fs::write(&path, b"one").unwrap();
        let old = SystemTime::now() - Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let mut cache = HashCache::default();
        let now = SystemTime::now();
        let first = cache.hash(&path, &std::fs::metadata(&path).unwrap(), now).unwrap();
        // Same metadata, old mtime: the cached hash is reused even if the bytes were
        // swapped behind the cache's back (proves no re-read happens).
        let stat = cache.0[&path].0;
        cache.0.insert(path.clone(), (stat, [7; 32]));
        assert_eq!(
            cache.hash(&path, &std::fs::metadata(&path).unwrap(), now),
            Some([7; 32])
        );
        // A real change alters the metadata and is hashed again.
        std::fs::write(&path, b"two!").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let second = cache.hash(&path, &std::fs::metadata(&path).unwrap(), now).unwrap();
        assert_ne!(first, second);
        assert_eq!(second, <Hash>::from(Sha256::digest(b"two!")));
        // A recently modified file is re-read even with identical cached metadata.
        std::fs::write(&path, b"thr!").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        cache.0.insert(path.clone(), (Some(Stat::of(&meta)), [7; 32]));
        assert_eq!(
            cache.hash(&path, &meta, SystemTime::now()),
            Some(Sha256::digest(b"thr!").into())
        );
        let _ = std::fs::remove_file(&path);
    }
}
