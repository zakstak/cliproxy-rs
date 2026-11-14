//! Model catalog updaters (Go internal/registry/catalog_sources.go): one updater each
//! for models.json and the Codex client catalog (Go's Devin catalog updater went with
//! the Devin provider, issue #49). Each reads its
//! effective source ([`effective_sources`]) now and every three hours, validates what it
//! read, and publishes it only while that source is still the configured one. A failed
//! read or an invalid catalog keeps the last valid one. A config reload that changes a
//! source switches the updater at once; a read still running for the old source is
//! dropped.
//!
//! A source is an absolute local path, an http(s) URL, empty for the router-for-me
//! downloads, `embed` (the catalog built into the binary, published once) or `disabled`
//! (no updates; the published catalog stays). Remote reads use Go's `http.DefaultClient`
//! rules (environment proxy, Go's redirects) and are bounded to 30 seconds; Go sets no
//! timeout.
//!
//! The empty source of the Codex client catalog starts lazily outside Home mode: with the
//! first Codex credential or the first request from a Codex client that needs the
//! catalog. Go starts it at launch.
//!
//! [`refresh_now`] reads both current sources once on demand (the management refresh
//! routes; Go CLIProxyAPI `10417e5b`, after v8.0.35). Every read, periodic or on demand,
//! takes a sequence number when it starts, and a read publishes only while its source's
//! generation is current and no read that started later has published. Concurrent
//! on-demand requests for the same source, generation and lifetime share one read, which
//! runs in its own task: a caller that goes away stops neither the read nor the other
//! callers. Go starts one read per request.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::Runtime;

/// Go `modelsURLs`, tried in order.
pub const MODELS_URLS: [&str; 2] = [
    "https://raw.githubusercontent.com/router-for-me/models/refs/heads/main/models.json",
    "https://models.router-for.me/models.json",
];
/// Go `codexClientModelsURLs`.
const CODEX_URLS: [&str; 2] = [
    "https://raw.githubusercontent.com/router-for-me/models/refs/heads/main/codex_client_models.json",
    "https://models.router-for.me/codex_client_models.json",
];

/// Go `ModelsRefreshInterval`.
const REFRESH_INTERVAL: Duration = Duration::from_secs(3 * 3600);
/// Go `maxCodexClientModelsSize`, the cap on every catalog read.
const MAX_SIZE: usize = 8 << 20;
// ponytail: Go's DefaultClient has no timeout, so a stalled source holds its updater
// until the next reconfigure; 30 seconds per remote read is the bound here.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
/// The bound on one on-demand read: both default sources at [`FETCH_TIMEOUT`] each, and
/// a local file that never ends (a FIFO). Go bounds it by the request context only.
const REFRESH_NOW_TIMEOUT: Duration = Duration::from_secs(65);

/// Go `embeddedCatalogSource`.
const EMBED: &str = "embed";
const DISABLED: &str = "disabled";

/// A catalog one source served, and the source that served it (the empty source tries
/// several).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Read {
    data: Vec<u8>,
    source: String,
}

/// No source served a valid catalog; `source` is the first one tried (Go's reported
/// source).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Unavailable {
    source: String,
}

type Fetch = Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = Result<Read, Unavailable>> + Send>> + Send + Sync>;
/// Installs a catalog and returns the providers whose models changed.
type Publish = Box<dyn Fn(&[u8]) -> Result<Vec<String>, String> + Send + Sync>;
type Validate = fn(&[u8]) -> Result<(), String>;

/// Go `CatalogSourceStatus` texts.
const UNAVAILABLE: &str = "no valid model catalog source";
const SOURCE_CHANGED: &str = "model catalog source changed during refresh";
const STOPPED: &str = "model catalog updaters stopped";
const TIMED_OUT: &str = "model catalog refresh timed out";
const REJECTED: &str = "model catalog rejected; keeping last valid catalog";
const ABORTED: &str = "model catalog refresh ended without a result";

/// What became of one catalog in an on-demand refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// `disabled` or `embed`: nothing to read.
    Skipped,
    /// The source served a valid catalog. `changed` names the providers whose models it
    /// changed; it is empty when the catalog was unchanged or a read that started later
    /// already published.
    Refreshed { changed: Vec<String> },
    /// The last valid catalog stays.
    Failed(&'static str),
}

/// Go `CatalogSourceStatus`: one catalog's source and [`Outcome`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// The source read: for the empty source, the default that served the catalog (or
    /// the first default when none did).
    pub source: String,
    pub outcome: Outcome,
}

impl Status {
    fn failed(source: String, reason: &'static str) -> Self {
        Self {
            source,
            outcome: Outcome::Failed(reason),
        }
    }

    /// Go's JSON for `CatalogSourceStatus` (`omitempty` members left out).
    pub fn to_json(&self) -> Value {
        let mut out = serde_json::Map::new();
        if !self.source.is_empty() {
            out.insert("source".into(), self.source.clone().into());
        }
        out.insert(
            "refreshed".into(),
            matches!(self.outcome, Outcome::Refreshed { .. }).into(),
        );
        match &self.outcome {
            Outcome::Skipped => {
                out.insert("skipped".into(), true.into());
            }
            Outcome::Refreshed { changed } if !changed.is_empty() => {
                out.insert("changed_providers".into(), json!(changed));
            }
            Outcome::Refreshed { .. } => {}
            Outcome::Failed(reason) => {
                out.insert("error".into(), (*reason).into());
            }
        }
        Value::Object(out)
    }
}

/// Go `CatalogRefreshResult` without the removed Devin catalog (issue #49).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// models.json.
    pub catalog: Status,
    /// The Codex client catalog.
    pub codex_catalog: Status,
}

impl Report {
    /// Go `mergeProviderNames` over both catalogs, in order, each provider once.
    pub fn changed_providers(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for status in [&self.catalog, &self.codex_catalog] {
            if let Outcome::Refreshed { changed } = &status.outcome {
                for provider in changed {
                    if !out.contains(provider) {
                        out.push(provider.clone());
                    }
                }
            }
        }
        out
    }

    /// Go `errors.Join` of the catalogs' errors: `None` when every catalog succeeded.
    pub fn error(&self) -> Option<String> {
        let errors: Vec<&str> = [&self.catalog, &self.codex_catalog]
            .into_iter()
            .filter_map(|s| match s.outcome {
                Outcome::Failed(reason) => Some(reason),
                _ => None,
            })
            .collect();
        (!errors.is_empty()).then(|| errors.join("\n"))
    }
}

/// A service lifetime (the context Go hands `StartModelCatalogUpdaters`): cancelling it
/// stops every loop started under it, and a reload after that starts nothing.
#[derive(Clone)]
pub(crate) struct Lifetime {
    id: u64,
    token: CancellationToken,
}

impl Lifetime {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            token: CancellationToken::new(),
        }
    }
}

#[derive(Default)]
struct State {
    source: String,
    generation: u64,
    /// The lifetime the current source runs under, and the token that stops it.
    run: Option<(u64, CancellationToken)>,
    /// The sequence number of the last read started (Go `requestSeq`) and of the last
    /// read published (Go `publishedSeq`). Numbers never repeat, so a read of an older
    /// generation can never pass for a newer one.
    started: u64,
    published: u64,
    /// The on-demand read in flight, which on-demand requests for the same read join.
    manual: Option<Manual>,
}

/// What an on-demand read reads, under which generation and lifetime: requests share a
/// read only when all three match.
#[derive(Clone, PartialEq, Eq)]
struct ManualKey {
    generation: u64,
    lifetime: Option<u64>,
    source: String,
    revision: Option<crate::runtime::CatalogRevision>,
}

struct Manual {
    key: ManualKey,
    seq: u64,
    done: watch::Receiver<Option<Status>>,
}

/// Why a finished read was not published.
enum Fence {
    /// The source's generation changed, or its loop was stopped, while it ran.
    Stale,
    /// A read that started later already published.
    Superseded,
    /// The publisher refused the catalog.
    Rejected,
}

/// Go `catalogUpdater`: source changes are serialized with publication, not with reads.
pub(crate) struct Updater {
    state: Mutex<State>,
    /// The empty source is configured but waits for [`Updater::wake`].
    pending: AtomicBool,
    fetch: Fetch,
    publish: Publish,
    /// The source the empty source reports before a read names one (Go `defaultURLs[0]`).
    first_default: &'static str,
}

impl Updater {
    #[cfg(test)]
    fn new(fetch: Fetch, publish: Publish) -> Arc<Self> {
        Self::with_default(fetch, publish, "")
    }

    fn with_default(fetch: Fetch, publish: Publish, first_default: &'static str) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::default(),
            pending: AtomicBool::new(false),
            fetch,
            publish,
            first_default,
        })
    }

    fn display(&self, source: &str) -> String {
        if source.is_empty() { self.first_default } else { source }.to_owned()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Go `catalogUpdater.configure`: nothing when `source` already runs under
    /// `lifetime`; otherwise stops the old loop and starts one for `source` (none for
    /// `disabled`, and none yet when `lazy`).
    fn configure(self: &Arc<Self>, lifetime: &Lifetime, source: &str, lazy: bool) {
        let mut state = self.lock();
        if let Some((id, token)) = &state.run {
            if state.source == source && *id == lifetime.id && !token.is_cancelled() {
                return;
            }
            token.cancel();
        }
        state.generation += 1;
        state.source = source.to_owned();
        let token = lifetime.token.child_token();
        state.run = Some((lifetime.id, token.clone()));
        let pending = lazy && source != DISABLED;
        self.pending.store(pending, Ordering::Release);
        if source == DISABLED || pending {
            return;
        }
        self.spawn(token, source.to_owned(), state.generation);
    }

    /// Starts a source [`Updater::configure`] left waiting. Cost when none waits: one
    /// atomic load.
    fn wake(self: &Arc<Self>) {
        if !self.pending.load(Ordering::Acquire) {
            return;
        }
        let state = self.lock();
        if !self.pending.swap(false, Ordering::AcqRel) {
            return;
        }
        if let Some((_, token)) = &state.run {
            self.spawn(token.clone(), state.source.clone(), state.generation);
        }
    }

    fn spawn(self: &Arc<Self>, token: CancellationToken, source: String, generation: u64) {
        let this = self.clone();
        tokio::spawn(async move {
            let run = async {
                this.refresh(&token, &source, generation).await;
                if source == EMBED {
                    return;
                }
                let start = tokio::time::Instant::now() + REFRESH_INTERVAL;
                let mut ticker = tokio::time::interval_at(start, REFRESH_INTERVAL);
                loop {
                    ticker.tick().await;
                    this.refresh(&token, &source, generation).await;
                }
            };
            token.run_until_cancelled(run).await;
        });
    }

    /// The sequence number of a read starting now.
    fn begin(&self) -> u64 {
        let mut state = self.lock();
        state.started += 1;
        state.started
    }

    /// Publishes `data`, read as `seq` under `generation`, while that generation is
    /// current, `token` runs and no later read has published. Returns the changed
    /// providers.
    fn publish_if_current(
        &self,
        token: &CancellationToken,
        generation: u64,
        seq: u64,
        data: &[u8],
    ) -> Result<Vec<String>, Fence> {
        let mut state = self.lock();
        if state.generation != generation || token.is_cancelled() {
            return Err(Fence::Stale);
        }
        if seq <= state.published {
            return Err(Fence::Superseded);
        }
        let published = (self.publish)(data);
        if published.is_ok() {
            state.published = seq;
        }
        drop(state);
        published.map_err(|_| {
            tracing::warn!("{REJECTED}");
            Fence::Rejected
        })
    }

    /// Go `catalogUpdater.refresh`: publishes only while `generation` is current.
    async fn refresh(&self, token: &CancellationToken, source: &str, generation: u64) {
        let seq = self.begin();
        let read = match (self.fetch)(source.to_owned()).await {
            Ok(read) => read,
            Err(_) => {
                if !token.is_cancelled() {
                    tracing::warn!("model catalog refresh failed; keeping last valid catalog");
                }
                return;
            }
        };
        if let Ok(changed) = self.publish_if_current(token, generation, seq, &read.data)
            && !changed.is_empty()
        {
            tracing::info!("model catalog updated, changes detected for providers: {changed:?}");
        }
    }

    /// Go `catalogUpdater.refreshOnDemand`: one read of the current source, joined with
    /// an on-demand read of the same source already in flight. Under `lifetime`'s
    /// running loop the configured source is read and fenced by that loop; otherwise
    /// (no loop for this lifetime yet) `fallback`, the source the current config
    /// resolves to. `None` is a runtime whose updaters were never started.
    async fn refresh_now(
        self: &Arc<Self>,
        lifetime: Option<&Lifetime>,
        fallback: &str,
        revision: Option<crate::runtime::CatalogRevision>,
    ) -> Status {
        let (display, mut done) = {
            let mut state = self.lock();
            let running = match (&state.run, lifetime) {
                (Some((id, token)), Some(lifetime)) if *id == lifetime.id && !token.is_cancelled() => {
                    Some(token.clone())
                }
                _ => None,
            };
            let revision = if running.is_none() { revision } else { None };
            let (source, token) = match (running, lifetime) {
                (Some(token), _) => (state.source.clone(), token),
                (None, Some(lifetime)) if lifetime.token.is_cancelled() => {
                    return Status::failed(self.display(fallback), STOPPED);
                }
                (None, Some(lifetime)) => (fallback.to_owned(), lifetime.token.child_token()),
                (None, None) => (fallback.to_owned(), CancellationToken::new()),
            };
            if source == DISABLED || source == EMBED {
                return Status {
                    source,
                    outcome: Outcome::Skipped,
                };
            }
            let display = self.display(&source);
            let key = ManualKey {
                generation: state.generation,
                lifetime: lifetime.map(|l| l.id),
                source,
                revision,
            };
            let joined = state.manual.as_ref().filter(|m| m.key == key).map(|m| m.done.clone());
            let done = match joined {
                Some(done) => done,
                None => self.start_manual(&mut state, key, token),
            };
            (display, done)
        };
        match done.wait_for(Option::is_some).await {
            Ok(status) => status.clone().expect("waited for a status"),
            // The read's task ended without a status (a panic, or runtime shutdown).
            Err(_) => Status::failed(display, ABORTED),
        }
    }

    /// Starts the on-demand read `key` names in its own task, so that no caller's
    /// cancellation stops it, and records it for later requests to join.
    fn start_manual(
        self: &Arc<Self>,
        state: &mut State,
        key: ManualKey,
        token: CancellationToken,
    ) -> watch::Receiver<Option<Status>> {
        state.started += 1;
        let seq = state.started;
        let (tx, rx) = watch::channel(None);
        state.manual = Some(Manual {
            key: key.clone(),
            seq,
            done: rx.clone(),
        });
        /// Forgets the read when its task ends, however it ends, so that no request
        /// joins a read that can no longer answer.
        struct Forget(Arc<Updater>, u64);
        impl Drop for Forget {
            fn drop(&mut self) {
                let mut state = self.0.lock();
                if state.manual.as_ref().is_some_and(|m| m.seq == self.1) {
                    state.manual = None;
                }
            }
        }
        let forget = Forget(self.clone(), seq);
        tokio::spawn(async move {
            let status = forget.0.manual_read(&token, &key, seq).await;
            drop(forget);
            tx.send_replace(Some(status));
        });
        rx
    }

    async fn manual_read(&self, token: &CancellationToken, key: &ManualKey, seq: u64) -> Status {
        let read = token
            .run_until_cancelled(tokio::time::timeout(
                REFRESH_NOW_TIMEOUT,
                (self.fetch)(key.source.clone()),
            ))
            .await;
        let stale = || {
            let changed = self.lock().generation != key.generation;
            if changed { SOURCE_CHANGED } else { STOPPED }
        };
        let read = match read {
            None => return Status::failed(self.display(&key.source), stale()),
            Some(Err(_elapsed)) => {
                tracing::warn!("model catalog refresh timed out; keeping last valid catalog");
                return Status::failed(self.display(&key.source), TIMED_OUT);
            }
            Some(Ok(Err(unavailable))) => {
                tracing::warn!("model catalog refresh failed; keeping last valid catalog");
                return Status::failed(unavailable.source, UNAVAILABLE);
            }
            Some(Ok(Ok(read))) => read,
        };
        // Lock order is config commit, then updater, as in Runtime's reconfigure.
        // Keep the revision lock through publication, not just through its check.
        let _revision = match &key.revision {
            Some(revision) => match revision.lock_current() {
                Some(guard) => Some(guard),
                None => return Status::failed(read.source, SOURCE_CHANGED),
            },
            None => None,
        };
        let outcome = match self.publish_if_current(token, key.generation, seq, &read.data) {
            Ok(changed) => {
                if !changed.is_empty() {
                    tracing::info!("model catalog updated, changes detected for providers: {changed:?}");
                }
                Outcome::Refreshed { changed }
            }
            Err(Fence::Stale) => Outcome::Failed(stale()),
            // Go reports a read a later one overtook as refreshed: the catalog in place
            // is at least as new.
            Err(Fence::Superseded) => Outcome::Refreshed { changed: Vec::new() },
            Err(Fence::Rejected) => Outcome::Failed(REJECTED),
        };
        Status {
            source: read.source,
            outcome,
        }
    }
}

/// Why one catalog source failed. Only this category is logged: the source, an error
/// text or a redirect target can carry credentials (Go logs nothing more either).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadError {
    File,
    Request,
    /// A source Go reads that would not go out as Go sends it
    /// ([`cpa_exec::proxy::go_target`]).
    Rewritten,
    Status(u16),
    Body,
    TooLarge,
    Invalid,
}

/// The first request Go sends for `source`, when it would go out as Go sends it
/// ([`cpa_exec::proxy::go_target`]); http.NewRequest drops the first hop's empty port.
fn go_target_of(source: &str) -> Option<axum::http::Uri> {
    let mut url = cpa_core::config::go_url::Escaped::parse(source)?;
    url.remove_empty_port();
    cpa_exec::proxy::go_target(&url)
}

/// Go `readCatalogSource`: an absolute path is read from disk, anything else is fetched
/// with Go's request target (`URL.RequestURI`) and must answer 200. Either is capped at
/// 8 MiB.
async fn read_source(source: &str) -> Result<Vec<u8>, ReadError> {
    let data = if cpa_core::config::go_is_abs(source) {
        use tokio::io::AsyncReadExt;
        let file = tokio::fs::File::open(source).await.map_err(|_| ReadError::File)?;
        let mut data = Vec::new();
        file.take(MAX_SIZE as u64 + 1)
            .read_to_end(&mut data)
            .await
            .map_err(|_| ReadError::File)?;
        data
    } else {
        if go_target_of(source).is_none() {
            return Err(ReadError::Rewritten);
        }
        // A client per read: reads are hours apart and the client is tied to no runtime.
        let client = cpa_exec::proxy::default_client();
        let mut headers = cpa_exec::proxy::GoHeaders::new();
        headers.exact_target();
        let response = cpa_exec::proxy::request(&client, wreq::Method::GET, source, headers, None, Some(FETCH_TIMEOUT))
            .await
            .map_err(|_| ReadError::Request)?;
        if response.status != 200 {
            return Err(ReadError::Status(response.status));
        }
        cpa_exec::proxy::read_all(response.body, MAX_SIZE + 1, false)
            .await
            .map_err(|_| ReadError::Body)?
            .to_vec()
    };
    if data.len() > MAX_SIZE {
        return Err(ReadError::TooLarge);
    }
    Ok(data)
}

/// Go `catalogFetcher`: `embed` is the built-in catalog, the empty source tries
/// `defaults` in order, and any other source is the only one tried. A candidate must
/// pass `validate`. Go `catalogFetcherWithSource` (`10417e5b`) also names the candidate
/// that served the catalog, or the first one tried.
fn fetcher(embedded: &'static [u8], defaults: Vec<String>, validate: Validate) -> Fetch {
    let defaults: Arc<[String]> = defaults.into();
    Arc::new(move |source: String| {
        let defaults = defaults.clone();
        Box::pin(async move {
            if source == EMBED {
                return Ok(Read {
                    data: embedded.to_vec(),
                    source,
                });
            }
            let candidates = if source.is_empty() {
                defaults.to_vec()
            } else {
                vec![source]
            };
            for candidate in &candidates {
                let read = read_source(candidate).await;
                match read.and_then(|data| validate(&data).map(|()| data).map_err(|_| ReadError::Invalid)) {
                    Ok(data) => {
                        return Ok(Read {
                            data,
                            source: candidate.clone(),
                        });
                    }
                    Err(reason) => tracing::debug!(?reason, "model catalog source failed"),
                }
            }
            Err(Unavailable {
                source: candidates.into_iter().next().unwrap_or_default(),
            })
        })
    })
}

fn owned(urls: &[&str]) -> Vec<String> {
    urls.iter().map(|u| (*u).to_owned()).collect()
}

fn general_fetcher(defaults: Vec<String>) -> Fetch {
    fetcher(cpa_core::registry::MODELS_JSON.as_bytes(), defaults, |data| {
        cpa_core::registry::validate_catalog_json(&String::from_utf8_lossy(data))
    })
}

/// Go `publishCatalogBytes`: a changed section rebuilds the registries that use it
/// ([`Runtime::registry`] keys on the catalog generation and rebuilds from the current
/// credentials). Returns the changed providers.
fn publish_general(data: &[u8]) -> Result<Vec<String>, String> {
    cpa_core::registry::refresh_catalog(&String::from_utf8_lossy(data))
}

/// The process-wide updaters: models.json and the Codex client catalog.
static CATALOGS: LazyLock<[Arc<Updater>; 2]> = LazyLock::new(|| {
    [
        Updater::with_default(
            general_fetcher(owned(&MODELS_URLS)),
            Box::new(publish_general),
            MODELS_URLS[0],
        ),
        Updater::with_default(
            fetcher(
                cpa_common::codex_client::CLIENT_MODELS_JSON.as_bytes(),
                owned(&CODEX_URLS),
                cpa_common::codex_catalog::validate,
            ),
            // Go reports no changed provider for the Codex client catalog.
            Box::new(|data| cpa_common::codex_catalog::load(data).map(|_| Vec::new())),
            CODEX_URLS[0],
        ),
    ]
});

/// Go `effectiveCatalogSources` over `[catalog, codex-catalog]`: an empty source is
/// `embed` under `--local-model`, and Home mode disables models.json whatever its source.
pub(crate) fn effective_sources(sources: &[String; 2], local: bool, home: bool) -> [String; 2] {
    let mut effective = sources
        .clone()
        .map(|s| if s.is_empty() && local { EMBED.to_owned() } else { s });
    if home {
        effective[0] = DISABLED.to_owned();
    }
    effective
}

#[derive(Clone, Copy)]
struct Policy {
    local: bool,
    home: bool,
    /// A test runtime: the empty source never downloads (see [`test_runtime`]).
    deny_default: bool,
}

/// Go `configureCatalogs` for sources the config load already validated. At startup the
/// empty Codex source waits outside Home mode.
fn configure(updaters: &[Arc<Updater>; 2], lifetime: &Lifetime, sources: &[String; 2], policy: Policy, startup: bool) {
    // Go `UpdateModelCatalogSources`: nothing after shutdown.
    if lifetime.token.is_cancelled() {
        return;
    }
    for (index, (updater, source)) in updaters.iter().zip(resolve(sources, policy)).enumerate() {
        let lazy = startup && !policy.home && index > 0 && source.is_empty();
        updater.configure(lifetime, &source, lazy);
    }
}

/// The source each updater runs for `sources` under `policy`: [`effective_sources`],
/// and `disabled` for the empty source of a test runtime.
fn resolve(sources: &[String; 2], policy: Policy) -> [String; 2] {
    effective_sources(sources, policy.local, policy.home).map(|source| {
        if policy.deny_default && source.is_empty() {
            DISABLED.to_owned()
        } else {
            source
        }
    })
}

/// Go `RefreshModelCatalogsWithPolicy` for both updaters at once: reads each current
/// source once and publishes it as the periodic read would ([`Updater::refresh_now`]).
/// Under a started runtime the running loops' sources are read; a runtime whose updaters
/// were never started reads the sources its current config resolves to under its
/// `--local-model` and test policy, as Go falls back to the handler's config (Home mode
/// is only known once started).
pub async fn refresh_now(rt: &Runtime) -> Report {
    let (started, sources, revision) = rt.with_catalogs(|session, config, revision| {
        (
            session.map(|s| (s.lifetime.clone(), s.policy)),
            config.catalog_sources(),
            revision,
        )
    });
    let (lifetime, policy) = match started {
        Some((lifetime, policy)) => (Some(lifetime), policy),
        None => (
            None,
            Policy {
                local: rt.local_model(),
                home: false,
                deny_default: test_runtime(rt),
            },
        ),
    };
    let [catalog, codex] = resolve(&sources, policy);
    let (catalog, codex_catalog) = tokio::join!(
        CATALOGS[0].refresh_now(lifetime.as_ref(), &catalog, Some(revision.clone())),
        CATALOGS[1].refresh_now(lifetime.as_ref(), &codex, Some(revision)),
    );
    Report { catalog, codex_catalog }
}

/// The catalog policy of one runtime's lifetime (Go's `catalogRuntime` and service
/// context). Dropped with the runtime, which stops its readers.
pub(crate) struct Session {
    lifetime: Lifetime,
    policy: Policy,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.lifetime.token.cancel();
    }
}

impl Session {
    /// Go `StartModelCatalogUpdaters` (`startup`) and `UpdateModelCatalogSources`: called
    /// by the runtime under its commit mutex, so commits reconfigure in their order.
    pub(crate) fn configure(&self, config: &cpa_core::config::Config, startup: bool) {
        configure(
            &CATALOGS,
            &self.lifetime,
            &config.catalog_sources(),
            self.policy,
            startup,
        );
    }
}

/// Go `startModelCatalogUpdaters`, for the lifetime of `rt`: configures the catalogs
/// from the current config, and every later config commit configures them again
/// ([`Runtime::publish_config_and_policy`]). Call once per runtime. Cost while no Codex
/// credential exists: one
/// parked task that looks at the credentials' providers after each change to the set.
/// In Home mode the local store stays empty, so every updater starts at once, as in Go.
pub fn start(rt: &Arc<Runtime>, home_enabled: bool) {
    let deny_default = test_runtime(rt);
    let policy = Policy {
        local: rt.local_model(),
        home: home_enabled,
        deny_default,
    };
    rt.start_catalogs(Session {
        lifetime: Lifetime::new(),
        policy,
    });
    if home_enabled {
        tracing::info!("Home mode: models.json updates disabled; Codex client model list follows Home model IDs");
    }
    if deny_default {
        return;
    }
    // Updaters that serve one provider, each started by that provider's first credential.
    let mut wanted: Vec<(&'static str, fn())> = vec![("codex", || CATALOGS[1].wake())];
    if home_enabled {
        for (_, start) in wanted {
            start();
        }
        return;
    }
    let weak = Arc::downgrade(rt);
    let mut changes = rt.store().subscribe();
    tokio::spawn(async move {
        loop {
            {
                let Some(rt) = weak.upgrade() else { return };
                changes.borrow_and_update();
                for c in rt.store().snapshot().iter() {
                    wanted.retain(|(provider, start)| {
                        let found = c.provider == *provider;
                        if found {
                            start();
                        }
                        !found
                    });
                }
            }
            if wanted.is_empty() || changes.changed().await.is_err() {
                return;
            }
        }
    });
}

/// A Codex client asked for its model catalog: start the default source if it waits.
pub(crate) fn codex_client_catalog_wanted() {
    CATALOGS[1].wake();
}

/// A runtime built by [`crate::testing::runtime`] never downloads the router-for-me
/// catalogs: tests stay off the network and keep the embedded data. Explicit sources
/// (files and loopback mocks) still run.
fn test_runtime(rt: &Runtime) -> bool {
    rt.deny_external.load(Ordering::Relaxed)
}

/// One models.json refresh from the first of `urls` that serves a valid catalog (the
/// empty source's reader with other defaults). For tests against local mocks.
pub async fn refresh(urls: &[String]) {
    match general_fetcher(urls.to_vec())(String::new()).await {
        Ok(read) => {
            if publish_general(&read.data).is_err() {
                tracing::warn!("{REJECTED}");
            }
        }
        Err(_) => tracing::warn!("model catalog refresh failed; keeping last valid catalog"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    /// A read of `source` whose bytes are its name.
    fn read(source: String) -> Read {
        served(source.clone(), source)
    }

    /// A read of `source` that served `data`.
    fn served(source: String, data: String) -> Read {
        Read {
            data: data.into_bytes(),
            source,
        }
    }

    /// An updater whose read returns the source's name and whose publications arrive on
    /// the returned channel.
    fn echo() -> (Arc<Updater>, UnboundedReceiver<String>) {
        let (tx, rx) = unbounded_channel();
        let fetch: Fetch = Arc::new(|source: String| Box::pin(async move { Ok(read(source)) }));
        let publish: Publish = Box::new(move |data| {
            tx.send(String::from_utf8_lossy(data).into_owned()).unwrap();
            Ok(Vec::new())
        });
        (Updater::new(fetch, publish), rx)
    }

    async fn next(rx: &mut UnboundedReceiver<String>) -> String {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("catalog source was not updated")
            .unwrap()
    }

    fn source(u: &Updater) -> String {
        u.lock().source.clone()
    }

    fn generation(u: &Updater) -> u64 {
        u.lock().generation
    }

    /// Go TestEffectiveCatalogSources.
    #[test]
    fn effective_sources_match_go() {
        let url = "https://example.com/catalog.json";
        for local in [false, true] {
            for home in [false, true] {
                for mask in 0..4 {
                    let sources: [String; 2] = std::array::from_fn(|i| {
                        if mask & (1 << i) != 0 {
                            url.to_owned()
                        } else {
                            String::new()
                        }
                    });
                    let got = effective_sources(&sources, local, home);
                    for (index, given) in sources.iter().enumerate() {
                        let mut want = if given.is_empty() && local {
                            EMBED
                        } else {
                            given.as_str()
                        };
                        if home && index == 0 {
                            want = DISABLED;
                        }
                        assert_eq!(
                            got[index], want,
                            "local={local} home={home} mask={mask} catalog={index}"
                        );
                    }
                }
            }
        }
    }

    /// Go TestCatalogSourceSwitchRejectsStalePublication: a read that finishes after the
    /// source changed is not published, and the new source does not wait for it.
    #[tokio::test]
    async fn source_switch_rejects_stale_publication() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let started_tx = Mutex::new(Some(started_tx));
        let release = Arc::new(tokio::sync::Notify::new());
        let held = release.clone();
        let fetch: Fetch = Arc::new(move |source: String| {
            let started = (source == "old").then(|| started_tx.lock().unwrap().take()).flatten();
            let release = held.clone();
            Box::pin(async move {
                if let Some(started) = started {
                    started.send(()).unwrap();
                    release.notified().await;
                }
                Ok(read(source))
            })
        });
        let (tx, mut published) = unbounded_channel();
        let u = Updater::new(
            fetch,
            Box::new(move |data| {
                tx.send(String::from_utf8_lossy(data).into_owned()).unwrap();
                Ok(Vec::new())
            }),
        );
        u.lock().generation = 1;
        let lifetime = Lifetime::new();
        let old = {
            let (u, token) = (u.clone(), lifetime.token.clone());
            tokio::spawn(async move { u.refresh(&token, "old", 1).await })
        };
        started_rx.await.unwrap();
        u.configure(&lifetime, "new", false);
        assert_eq!(next(&mut published).await, "new");
        release.notify_one();
        old.await.unwrap();
        assert!(published.try_recv().is_err(), "stale publication");
        u.configure(&lifetime, "new", false);
        assert_eq!(generation(&u), 2, "unchanged source restarted");
    }

    /// Go TestCatalogRefreshKeepsLastValidData and TestDisabledCatalogDoesNotFetch.
    #[tokio::test]
    async fn failures_keep_the_last_valid_catalog_and_disabled_reads_nothing() {
        let last = Arc::new(Mutex::new("last valid".to_owned()));
        let keep = |fetch: Fetch, ok: bool| {
            let last = last.clone();
            Updater::new(
                fetch,
                Box::new(move |data| {
                    if !ok {
                        return Err("invalid".into());
                    }
                    *last.lock().unwrap() = String::from_utf8_lossy(data).into_owned();
                    Ok(Vec::new())
                }),
            )
        };
        let token = CancellationToken::new();
        let unavailable: Fetch = Arc::new(|_| Box::pin(async { Err(Unavailable { source: String::new() }) }));
        keep(unavailable, true).refresh(&token, "", 0).await;
        let invalid: Fetch = Arc::new(|_| Box::pin(async { Ok(read("invalid".into())) }));
        keep(invalid, false).refresh(&token, "", 0).await;
        assert_eq!(*last.lock().unwrap(), "last valid");

        let reads = Arc::new(AtomicU64::new(0));
        let counted = reads.clone();
        let fetch: Fetch = Arc::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(read(String::new())) })
        });
        let u = keep(fetch, true);
        u.configure(&Lifetime::new(), DISABLED, false);
        u.wake();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!((source(&u), reads.load(Ordering::SeqCst)), (DISABLED.to_owned(), 0));
    }

    /// K1 (#68): when catalog discovery is unavailable (every default source refuses
    /// the connection), the models.json updater publishes nothing, so the embedded
    /// catalog stays in effect; that catalog, as `embed` serves it, lists the K1
    /// additions next to the entries they join, GPT-5.5 included.
    #[tokio::test]
    async fn unavailable_discovery_keeps_the_embedded_k1_catalog() {
        // Port 1 on loopback refuses connections; nothing leaves the host.
        let fetch = general_fetcher(vec![
            "http://127.0.0.1:1/models.json".to_owned(),
            "http://127.0.0.1:1/fallback/models.json".to_owned(),
        ]);
        let published = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let sink = published.clone();
        let u = Updater::new(
            fetch.clone(),
            Box::new(move |data| {
                sink.lock().unwrap().push(data.to_vec());
                Ok(Vec::new())
            }),
        );
        assert!(fetch(String::new()).await.is_err(), "a refused default source was read");
        u.refresh(&CancellationToken::new(), "", 0).await;
        assert!(published.lock().unwrap().is_empty(), "an unavailable source published");

        let embedded = fetch(EMBED.to_owned()).await.expect("embed always reads").data;
        let catalog = cpa_core::registry::Catalog::parse(std::str::from_utf8(&embedded).unwrap()).unwrap();
        let ids = |channel: &str| {
            catalog
                .channel(channel)
                .iter()
                .map(|m| m.id.clone())
                .collect::<Vec<_>>()
        };
        let has = |channel: &str, id: &str| ids(channel).iter().any(|m| m == id);
        assert!(has("claude", "claude-fable-5-2") && has("claude", "claude-fable-5-1"));
        for tier in ["codex-free", "codex-team", "codex-plus", "codex-pro"] {
            assert!(
                has(tier, "gpt-6.1-terra") && has(tier, "gpt-5.5"),
                "{tier}: {:?}",
                ids(tier)
            );
            assert!(!has(tier, "gpt-6-chat-thinking"), "{tier}");
        }
        assert_eq!(ids("meta").first().map(String::as_str), Some("muse-ember-1.0"));
        // X4 (#87): Grok Imagine Video 1.5 Lite is an xAI built-in, not an embedded
        // catalog entry; the xAI definitions served while discovery is unavailable list it.
        let xai = cpa_core::registry::dynamic::static_definitions("xai").expect("the xai channel");
        assert!(
            xai.iter().any(|m| m.id == "grok-imagine-video-1.5-lite"),
            "X4 (#87): {:?}",
            ids("xai")
        );
    }

    /// Go TestCatalogFetcherSources over local mocks and files: `embed`, a URL and a
    /// path each serve their catalog; an invalid or missing explicit source fails without
    /// falling back to the defaults; the empty source reads the defaults; a file is
    /// read again each time.
    #[tokio::test]
    async fn fetcher_reads_each_kind_of_source() {
        use axum::routing::get;
        let catalogs: [(&str, &'static [u8], Validate); 2] = [
            ("general", cpa_core::registry::MODELS_JSON.as_bytes(), |d| {
                cpa_core::registry::validate_catalog_json(&String::from_utf8_lossy(d))
            }),
            (
                "codex",
                cpa_common::codex_client::CLIENT_MODELS_JSON.as_bytes(),
                cpa_common::codex_catalog::validate,
            ),
        ];
        let dir = std::env::temp_dir().join(format!("cpa-catalog-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, data, validate) in catalogs {
            let official_hits = Arc::new(AtomicU64::new(0));
            let hits = official_hits.clone();
            let app = axum::Router::new()
                .route(
                    "/official",
                    get(move || {
                        hits.fetch_add(1, Ordering::SeqCst);
                        async move { data }
                    }),
                )
                .route("/custom", get(move || async move { data }))
                .route("/broken", get(|| async { "invalid" }))
                .route("/missing", get(|| async { (axum::http::StatusCode::NOT_FOUND, "") }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let path = dir.join(format!("{name}.json"));
            std::fs::write(&path, data).unwrap();
            let path = path.to_str().unwrap().to_owned();
            let fetch = fetcher(data, vec![format!("{base}/official")], validate);
            for source in [EMBED.to_owned(), format!("{base}/custom"), path.clone()] {
                assert_eq!(
                    fetch(source.clone()).await.map(|r| r.data).as_deref(),
                    Ok(data),
                    "{name}: {source}"
                );
            }
            for source in [
                format!("{base}/broken"),
                format!("{base}/missing"),
                format!("{path}.missing"),
            ] {
                assert!(fetch(source.clone()).await.is_err(), "{name}: accepted {source}");
            }
            assert_eq!(
                official_hits.load(Ordering::SeqCst),
                0,
                "{name}: fell back to the default"
            );
            assert_eq!(
                fetch(String::new()).await.map(|r| r.data).as_deref(),
                Ok(data),
                "{name}"
            );
            assert_eq!(official_hits.load(Ordering::SeqCst), 1, "{name}: default not read");
            std::fs::write(&path, "invalid").unwrap();
            assert!(fetch(path.clone()).await.is_err(), "{name}: file was not read again");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    async fn serve(app: axum::Router) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    }

    /// A remote source goes out with Go's request target (`URL.RequestURI`, recorded
    /// with Go 1.26.4): dot segments as written, the path escaped as Go escapes it, and
    /// each redirect resolved and escaped as Go's client does (more cases in cpa-exec's
    /// Go-recorded redirect replay). A source that would not go out so is refused
    /// without a request.
    #[tokio::test]
    async fn sources_are_requested_as_go_requests_them() {
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let record = seen.clone();
        let app = axum::Router::new().fallback(move |request: axum::extract::Request| {
            let uri = request.uri().to_string();
            let auth = request
                .headers()
                .get("authorization")
                .map(|v| v.to_str().unwrap().to_owned());
            record.lock().unwrap().push(match auth {
                Some(auth) => format!("{uri} {auth}"),
                None => uri.clone(),
            });
            async move {
                use axum::response::IntoResponse;
                let location = match uri.as_str() {
                    "/dots" => "/x/%2E%2e/catalog.json",
                    "/backslash" => "/a\\..\\catalog.json",
                    "/tab" => "/a/\t../catalog.json",
                    _ => return cpa_common::codex_client::CLIENT_MODELS_JSON.into_response(),
                };
                (
                    axum::http::StatusCode::FOUND,
                    [(axum::http::header::LOCATION, location)],
                )
                    .into_response()
            }
        });
        let addr = serve(app).await;
        let fetch = fetcher(
            cpa_common::codex_client::CLIENT_MODELS_JSON.as_bytes(),
            Vec::new(),
            cpa_common::codex_catalog::validate,
        );
        for path in [
            "/a/../catalog.json",
            "/my catalogs/../catalog.json",
            "/mod\u{e8}les/catalog.json",
            "/a%2Fb/x|y.json",
        ] {
            assert!(fetch(format!("http://{addr}{path}")).await.is_ok(), "{path}");
        }
        assert!(
            fetch(format!("http://u%3A:p@{addr}/a/../catalog.json?v=1"))
                .await
                .is_ok()
        );
        // Go requests these; the URL would not go out as Go sends it.
        for source in [
            "http://b\u{fc}cher.example/a/../catalog.json",
            "http://[fe80::1%25en0]:8080/catalog.json",
            "http://127.0.0.1:1/catalog.json?q=a b",
            // Go sends Host: 127.0.0.1:80 (and :080, :443); wreq would drop the port.
            "http://127.0.0.1:80/catalog.json",
            "http://127.0.0.1:080/catalog.json",
            "https://127.0.0.1:443/catalog.json",
        ] {
            assert_eq!(read_source(source).await, Err(ReadError::Rewritten), "{source}");
        }
        // http.NewRequest drops the first hop's empty port, so Go's Host header has none
        // either: the source is accepted. Not sent: port 80 belongs to no test.
        assert!(go_target_of("http://127.0.0.1:/catalog.json").is_some());
        for path in ["/dots", "/backslash"] {
            assert!(fetch(format!("http://{addr}{path}")).await.is_ok(), "{path}");
        }
        // Go's url.Parse fails on the tab; WHATWG would strip it and request /catalog.json.
        assert!(fetch(format!("http://{addr}/tab")).await.is_err());
        assert_eq!(
            *seen.lock().unwrap(),
            [
                "/a/../catalog.json",
                "/my%20catalogs/../catalog.json",
                "/mod%C3%A8les/catalog.json",
                "/a/b/x%7Cy.json",
                // Basic "u::p", as Go sends the unescaped user info.
                "/a/../catalog.json?v=1 Basic dTo6cA==",
                "/dots",
                "/x/%2E%2e/catalog.json",
                "/backslash",
                "/a%5C..%5Ccatalog.json",
                "/tab",
            ]
        );
    }

    /// Go TestCatalogPolicyHotReload, then the lazy default: at startup outside Home the
    /// empty Codex source waits for a wake, and a reload that changes it starts it at once.
    #[tokio::test]
    async fn policy_follows_reloads_and_lifetimes() {
        let (general, mut general_rx) = echo();
        let (codex, mut codex_rx) = echo();
        let updaters = [general.clone(), codex.clone()];
        let sources = |g: &str, c: &str| [g.to_owned(), c.to_owned()];
        let policy = |local, home| Policy {
            local,
            home,
            deny_default: false,
        };
        let lifetime = Lifetime::new();
        let (url, codex_url) = ("https://example.com/general", "https://example.com/codex");
        configure(&updaters, &lifetime, &sources(url, ""), policy(true, false), true);
        assert_eq!(next(&mut general_rx).await, url);
        assert_eq!(next(&mut codex_rx).await, EMBED);
        configure(
            &updaters,
            &lifetime,
            &sources("", codex_url),
            policy(true, false),
            false,
        );
        assert_eq!(next(&mut general_rx).await, EMBED);
        assert_eq!(next(&mut codex_rx).await, codex_url);
        let ignored = "https://example.com/ignored";
        configure(&updaters, &lifetime, &sources(ignored, ""), policy(true, true), false);
        assert_eq!(next(&mut codex_rx).await, EMBED);
        assert_eq!(source(&general), DISABLED);
        configure(&updaters, &lifetime, &sources("", ""), policy(true, false), false);
        assert_eq!(next(&mut general_rx).await, EMBED);

        // Shutdown fences reloads; a new lifetime restarts the same sources.
        lifetime.token.cancel();
        let stopped = generation(&codex);
        configure(
            &updaters,
            &lifetime,
            &sources("", "https://example.com/stopped"),
            policy(true, false),
            false,
        );
        assert_eq!(generation(&codex), stopped, "reload after shutdown restarted a reader");
        let lifetime = Lifetime::new();
        configure(&updaters, &lifetime, &sources(ignored, ""), policy(true, true), true);
        assert_eq!(next(&mut codex_rx).await, EMBED);
        assert_eq!(source(&general), DISABLED);

        // The default source: models.json starts, Codex waits.
        let lifetime = Lifetime::new();
        configure(&updaters, &lifetime, &sources("", ""), policy(false, false), true);
        assert_eq!(next(&mut general_rx).await, "");
        configure(&updaters, &lifetime, &sources("", ""), policy(false, false), false);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(codex_rx.try_recv().is_err());
        codex.wake();
        assert_eq!(next(&mut codex_rx).await, "");
        // In Home mode the Codex default starts at launch.
        let lifetime = Lifetime::new();
        configure(&updaters, &lifetime, &sources("", ""), policy(false, true), true);
        assert_eq!(next(&mut codex_rx).await, "");
    }

    /// A test runtime downloads no default catalog, through either entry point: the
    /// startup plan (Home mode starts every updater at once) or a Codex client asking
    /// for its catalog.
    #[tokio::test]
    async fn test_runtimes_start_no_default_download() {
        let executors = cpa_exec::Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            meta: Default::default(),
            xai: Default::default(),
        };
        let dir = std::env::temp_dir().join(format!("cpa-updater-{}", uuid::Uuid::new_v4()));
        let cfg = cpa_core::config::Config::parse(&format!("auth-dir: {}\n", dir.display())).unwrap();
        let rt = Arc::new(crate::testing::runtime(cfg, Vec::new(), executors));
        start(&rt, false);
        start(&rt, true);
        let _ = crate::codex_models::response(&rt, "0.130.0");
        for updater in CATALOGS.iter() {
            assert_eq!(source(updater), DISABLED);
            assert!(!updater.pending.load(Ordering::SeqCst));
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A fetch that counts its reads and passes each read's number and source to
    /// `result`. A read for which `hold` is true waits for a permit on the returned
    /// semaphore first.
    fn gated(
        hold: impl Fn(u64, &str) -> bool + Send + Sync + 'static,
        result: impl Fn(u64, String) -> Result<Read, Unavailable> + Send + Sync + 'static,
    ) -> (Fetch, Arc<AtomicU64>, Arc<tokio::sync::Semaphore>) {
        let reads = Arc::new(AtomicU64::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let (counted, gate) = (reads.clone(), release.clone());
        let (hold, result) = (Arc::new(hold), Arc::new(result));
        let fetch: Fetch = Arc::new(move |source: String| {
            let n = counted.fetch_add(1, Ordering::SeqCst) + 1;
            let (gate, hold, result) = (gate.clone(), hold.clone(), result.clone());
            Box::pin(async move {
                if hold(n, &source) {
                    gate.acquire().await.unwrap().forget();
                }
                result(n, source)
            })
        });
        (fetch, reads, release)
    }

    /// An updater over `fetch` whose publications are recorded and report `claude`.
    fn recording(fetch: Fetch) -> (Arc<Updater>, Arc<Mutex<Vec<String>>>) {
        let published = Arc::new(Mutex::new(Vec::new()));
        let sink = published.clone();
        let publish: Publish = Box::new(move |data| {
            sink.lock().unwrap().push(String::from_utf8_lossy(data).into_owned());
            Ok(vec!["claude".to_owned()])
        });
        (Updater::new(fetch, publish), published)
    }

    async fn until(what: &str, done: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    fn refreshed(source: &str, changed: &[&str]) -> Status {
        Status {
            source: source.to_owned(),
            outcome: Outcome::Refreshed {
                changed: changed.iter().map(|p| (*p).to_owned()).collect(),
            },
        }
    }

    /// #98: concurrent on-demand requests share one read of the running source (not the
    /// fallback), the read outlives a caller that goes away, and a request after it ends
    /// starts a new read.
    #[tokio::test]
    async fn on_demand_requests_share_one_read_that_outlives_its_callers() {
        use futures_util::poll;
        let (fetch, reads, release) = gated(
            |_, _| true,
            |n, source| {
                let data = format!("{source}-{n}");
                Ok(served(source, data))
            },
        );
        let (u, published) = recording(fetch);
        let lifetime = Lifetime::new();
        // Configured and waiting (the lazy default's state): no periodic read runs.
        u.configure(&lifetime, "src", true);
        let mut first = Box::pin(u.refresh_now(Some(&lifetime), "fallback", None));
        assert!(poll!(first.as_mut()).is_pending());
        until("the first read", || reads.load(Ordering::SeqCst) == 1).await;
        let mut second = Box::pin(u.refresh_now(Some(&lifetime), "fallback", None));
        let mut third = Box::pin(u.refresh_now(Some(&lifetime), "fallback", None));
        assert!(poll!(second.as_mut()).is_pending() && poll!(third.as_mut()).is_pending());
        drop(first);
        release.add_permits(1);
        let (second, third) = tokio::join!(second, third);
        assert_eq!(second, refreshed("src", &["claude"]));
        assert_eq!(third, second);
        assert_eq!(reads.load(Ordering::SeqCst), 1, "joined requests read again");
        assert_eq!(*published.lock().unwrap(), ["src-1"]);

        release.add_permits(1);
        assert_eq!(
            u.refresh_now(Some(&lifetime), "fallback", None).await,
            refreshed("src", &["claude"])
        );
        assert_eq!(reads.load(Ordering::SeqCst), 2, "a finished read was joined");
        assert_eq!(*published.lock().unwrap(), ["src-1", "src-2"]);
        assert!(u.lock().manual.is_none());
    }

    /// #98: a failed, timed-out or aborted on-demand read answers every waiting request,
    /// publishes nothing, and leaves no read behind for later requests to join.
    #[tokio::test(start_paused = true)]
    async fn failed_timed_out_and_aborted_reads_answer_every_waiter() {
        use futures_util::poll;
        let lifetime = Lifetime::new();
        let (fetch, reads, release) = gated(|_, _| true, |_, source| Err(Unavailable { source }));
        let (u, published) = recording(fetch);
        u.configure(&lifetime, "src", true);
        let mut a = Box::pin(u.refresh_now(Some(&lifetime), "", None));
        let mut b = Box::pin(u.refresh_now(Some(&lifetime), "", None));
        assert!(poll!(a.as_mut()).is_pending() && poll!(b.as_mut()).is_pending());
        release.add_permits(1);
        let failed = Status::failed("src".into(), UNAVAILABLE);
        assert_eq!(tokio::join!(a, b), (failed.clone(), failed));
        assert_eq!(reads.load(Ordering::SeqCst), 1);

        // A read that never ends is bounded (virtual time).
        let never: Fetch = Arc::new(|_| Box::pin(std::future::pending()));
        let (u, never_published) = recording(never);
        u.configure(&lifetime, "src", true);
        let mut a = Box::pin(u.refresh_now(Some(&lifetime), "", None));
        let mut b = Box::pin(u.refresh_now(Some(&lifetime), "", None));
        assert!(poll!(a.as_mut()).is_pending() && poll!(b.as_mut()).is_pending());
        let started = tokio::time::Instant::now();
        let timed_out = Status::failed("src".into(), TIMED_OUT);
        assert_eq!(tokio::join!(a, b), (timed_out.clone(), timed_out));
        let waited = started.elapsed();
        assert!(
            waited >= REFRESH_NOW_TIMEOUT && waited < REFRESH_NOW_TIMEOUT + Duration::from_secs(1),
            "{waited:?}"
        );
        assert!(u.lock().manual.is_none());

        // A read whose task panics answers its waiters and is forgotten.
        let panics = Arc::new(AtomicU64::new(0));
        let counted = panics.clone();
        let panicking: Fetch = Arc::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { panic!("synthetic catalog read panic") })
        });
        let (u, panic_published) = recording(panicking);
        u.configure(&lifetime, "src", true);
        let aborted = Status::failed("src".into(), ABORTED);
        assert_eq!(u.refresh_now(Some(&lifetime), "", None).await, aborted);
        assert_eq!(u.refresh_now(Some(&lifetime), "", None).await, aborted);
        assert_eq!(panics.load(Ordering::SeqCst), 2, "a dead read was joined");
        for published in [published, never_published, panic_published] {
            assert!(published.lock().unwrap().is_empty());
        }
    }

    /// #98: an on-demand read still running when its source changes is not published,
    /// and the new source does not wait for it: a read under the running loop stops with
    /// the loop, and a read of a fallback source (no loop of its lifetime) is refused at
    /// publication. A lifetime that ends answers its waiters at once, and later requests
    /// under it read nothing.
    #[tokio::test]
    async fn a_source_change_or_shutdown_fences_an_on_demand_read() {
        use futures_util::poll;
        let (fetch, reads, release) = gated(|_, source| source == "old", |_, source| Ok(read(source)));
        let (u, published) = recording(fetch);
        let lifetime = Lifetime::new();
        u.configure(&lifetime, "old", true);
        let mut manual = Box::pin(u.refresh_now(Some(&lifetime), "old", None));
        assert!(poll!(manual.as_mut()).is_pending());
        until("the old read", || reads.load(Ordering::SeqCst) == 1).await;
        u.configure(&lifetime, "new", false);
        assert_eq!(manual.await, Status::failed("old".into(), SOURCE_CHANGED));
        until("the new source", || published.lock().unwrap().len() == 1).await;
        assert_eq!(*published.lock().unwrap(), ["new"]);

        // A fallback read is not stopped by the loop; its publication is refused.
        let other = Lifetime::new();
        let mut fallback = Box::pin(u.refresh_now(Some(&other), "old", None));
        assert!(poll!(fallback.as_mut()).is_pending());
        until("the fallback read", || reads.load(Ordering::SeqCst) == 3).await;
        u.configure(&lifetime, "newer", false);
        until("the newer source", || published.lock().unwrap().len() == 2).await;
        release.add_permits(1);
        assert_eq!(fallback.await, Status::failed("old".into(), SOURCE_CHANGED));
        assert_eq!(*published.lock().unwrap(), ["new", "newer"], "stale publication");

        let lifetime = Lifetime::new();
        u.configure(&lifetime, "old", true);
        let mut manual = Box::pin(u.refresh_now(Some(&lifetime), "old", None));
        assert!(poll!(manual.as_mut()).is_pending());
        until("the second old read", || reads.load(Ordering::SeqCst) == 5).await;
        lifetime.token.cancel();
        assert_eq!(manual.await, Status::failed("old".into(), STOPPED));
        assert_eq!(
            u.refresh_now(Some(&lifetime), "old", None).await,
            Status::failed("old".into(), STOPPED)
        );
        assert_eq!(reads.load(Ordering::SeqCst), 5, "read after shutdown");
        assert_eq!(*published.lock().unwrap(), ["new", "newer"]);
        assert_eq!(release.available_permits(), 0);
    }

    /// #98 (Go `10417e5b` same-source ordering): reads of one source publish in the
    /// order they started, whichever finishes first, periodic or on demand.
    #[tokio::test]
    async fn reads_publish_in_start_order_whatever_order_they_finish() {
        use futures_util::poll;
        // Odd reads wait for a permit.
        let (fetch, reads, release) = gated(|n, _| n % 2 == 1, |n, source| Ok(served(source, format!("v{n}"))));
        let (u, published) = recording(fetch);
        let lifetime = Lifetime::new();
        u.configure(&lifetime, "same", true);
        let (generation, token) = {
            let state = u.lock();
            (state.generation, state.run.as_ref().unwrap().1.clone())
        };

        // A slow periodic read, then an on-demand one that finishes first.
        let periodic = {
            let (u, token) = (u.clone(), token.clone());
            tokio::spawn(async move { u.refresh(&token, "same", generation).await })
        };
        until("the periodic read", || reads.load(Ordering::SeqCst) == 1).await;
        assert_eq!(
            u.refresh_now(Some(&lifetime), "", None).await,
            refreshed("same", &["claude"])
        );
        release.add_permits(1);
        periodic.await.unwrap();
        assert_eq!(
            *published.lock().unwrap(),
            ["v2"],
            "an older periodic read overwrote a newer one"
        );

        // A slow on-demand read, then a periodic one that finishes first.
        let mut manual = Box::pin(u.refresh_now(Some(&lifetime), "", None));
        assert!(poll!(manual.as_mut()).is_pending());
        until("the on-demand read", || reads.load(Ordering::SeqCst) == 3).await;
        u.refresh(&token, "same", generation).await;
        release.add_permits(1);
        assert_eq!(
            manual.await,
            refreshed("same", &[]),
            "overtaken: refreshed, nothing changed"
        );
        assert_eq!(
            *published.lock().unwrap(),
            ["v2", "v4"],
            "an older on-demand read overwrote a newer one"
        );
    }

    /// #98: `disabled` and `embed` read nothing; the running loop's source wins over the
    /// fallback; without a loop for the lifetime the fallback is read; and the fallback
    /// follows `--local-model`, Home mode and the test runtime's no-download rule.
    #[tokio::test]
    async fn on_demand_reads_follow_the_configured_policy() {
        let (fetch, reads, _release) = gated(|_, _| false, |_, source| Ok(read(source)));
        let (u, published) = recording(fetch);
        for source in [DISABLED, EMBED] {
            let skipped = Status {
                source: source.to_owned(),
                outcome: Outcome::Skipped,
            };
            assert_eq!(u.refresh_now(None, source, None).await, skipped);
        }
        let lifetime = Lifetime::new();
        u.configure(&lifetime, DISABLED, false);
        let status = u
            .refresh_now(Some(&lifetime), "https://example.com/catalog.json", None)
            .await;
        assert_eq!(status.outcome, Outcome::Skipped);
        assert_eq!(reads.load(Ordering::SeqCst), 0);

        // Another lifetime's loop runs: this lifetime's fallback is read.
        let other = Lifetime::new();
        assert_eq!(
            u.refresh_now(Some(&other), "/catalog.json", None).await,
            refreshed("/catalog.json", &["claude"])
        );
        assert_eq!(*published.lock().unwrap(), ["/catalog.json"]);

        let policy = |local, home, deny_default| Policy {
            local,
            home,
            deny_default,
        };
        let sources = |a: &str, b: &str| [a.to_owned(), b.to_owned()];
        assert_eq!(
            resolve(&sources("", ""), policy(false, false, true)),
            [DISABLED, DISABLED]
        );
        assert_eq!(resolve(&sources("", ""), policy(true, false, true)), [EMBED, EMBED]);
        assert_eq!(resolve(&sources("", ""), policy(false, false, false)), ["", ""]);
        assert_eq!(
            resolve(&sources("https://example.com/a", "/b.json"), policy(false, true, true)),
            [DISABLED, "/b.json"]
        );
    }

    /// Go `CatalogSourceStatus` and `CatalogRefreshResult`, without the Devin catalog.
    #[test]
    fn reports_render_go_catalog_source_statuses() {
        let render = |s: &Status| serde_json::to_string(&s.to_json()).unwrap();
        let ok = refreshed("https://example.com/models.json", &["claude", "xai"]);
        assert_eq!(
            render(&ok),
            r#"{"source":"https://example.com/models.json","refreshed":true,"changed_providers":["claude","xai"]}"#
        );
        let skipped = Status {
            source: DISABLED.into(),
            outcome: Outcome::Skipped,
        };
        assert_eq!(
            render(&skipped),
            r#"{"source":"disabled","refreshed":false,"skipped":true}"#
        );
        assert_eq!(
            render(&refreshed("embed", &[])),
            r#"{"source":"embed","refreshed":true}"#
        );
        let failed = Status::failed("/catalog.json".into(), UNAVAILABLE);
        assert_eq!(
            render(&failed),
            r#"{"source":"/catalog.json","refreshed":false,"error":"no valid model catalog source"}"#
        );
        let report = Report {
            catalog: ok.clone(),
            codex_catalog: skipped,
        };
        assert_eq!(
            (report.changed_providers(), report.error()),
            (vec!["claude".into(), "xai".into()], None)
        );
        let report = Report {
            catalog: refreshed("a", &["codex", "claude"]),
            codex_catalog: refreshed("b", &["codex"]),
        };
        assert_eq!(report.changed_providers(), ["codex", "claude"]);
        let report = Report {
            catalog: failed.clone(),
            codex_catalog: Status::failed("b".into(), TIMED_OUT),
        };
        assert_eq!(
            report.error().as_deref(),
            Some("no valid model catalog source\nmodel catalog refresh timed out")
        );
        assert!(report.changed_providers().is_empty());
    }
}
