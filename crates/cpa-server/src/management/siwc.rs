//! v8 Sign in with ChatGPT binding (Go v8.0.35 `d50a1995`, `ae3ee3be`:
//! internal/api/handlers/management/oauth_siwc.go, oauth_siwc_mutations.go and
//! sdk/cliproxy/auth/codex_siwc_bind.go). A binding adds the `siwc` token partition and
//! `use_siwc` to an existing file-backed Codex OAuth credential. The session owns the
//! target, PKCE verifier, device ID and redirect; a callback only supplies the code and
//! the issued client ID, through the v8 callback route alone.

use cpa_core::gostr::GoStr;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use cpa_core::credential::Credential;
use cpa_exec::codex_siwc::{self, REDIRECT_URI, Tokens};
use serde_json::{Map, Value};

use super::Management;
use super::auth_files::{CodexLock, Query, auth_kind, fail, file_name, lookup, path_of, reply, runtime_only};
use super::oauth::CallbackRequest;
use crate::runtime::PatchError;

/// Go `siwcBindTTL`.
const TTL: Duration = Duration::from_secs(5 * 60);

/// Go `siwcBindSession`, minus what the session store already tracks.
pub(crate) struct Flow {
    pub(super) target_id: String,
    /// The credential file, symlinks resolved, as checked at start.
    pub(super) path: PathBuf,
    device_id: String,
    /// The account of the credential the binding started on, `""` for none (Go v8.0.67
    /// `siwcBindSession.accountID`, 6bbd2cb7). Its file held the same account then; the
    /// commit requires both the loaded credential and the file to still hold it.
    account: String,
    /// The issued client the credential already held, `""` for none: a rebinding
    /// reauthorizes with it (OpenAI "Accounts and sessions"; Go 3891ebaa for the CLI).
    client: String,
    verifier: String,
    deadline: Instant,
    exchange: Mutex<Option<tokio::task::AbortHandle>>,
}

#[cfg(test)]
impl Flow {
    /// A binding of `target_id` that never exchanges, for session-store tests.
    pub(super) fn idle(target_id: &str) -> Arc<Self> {
        Arc::new(Self {
            target_id: target_id.into(),
            path: PathBuf::new(),
            device_id: String::new(),
            account: String::new(),
            client: String::new(),
            verifier: String::new(),
            deadline: Instant::now() + TTL,
            exchange: Mutex::default(),
        })
    }
}

impl Flow {
    /// Stops an in-flight token exchange. A commit already running finishes; it
    /// re-checks the session before writing.
    pub(super) fn cancel(&self) {
        if let Some(task) = self.exchange.lock().unwrap_or_else(PoisonError::into_inner).take() {
            task.abort();
        }
    }
}

fn non_empty(meta: &Map<String, Value>, key: &str) -> bool {
    meta.get(key)
        .and_then(Value::as_str)
        .is_some_and(|v| !v.trim().is_empty())
}

fn has_outer(meta: &Map<String, Value>) -> bool {
    non_empty(meta, "access_token") && non_empty(meta, "refresh_token")
}

/// A credential's account: its `account_id` string, `""` for none.
fn account_of(c: &Credential) -> &str {
    c.str("account_id").unwrap_or_default()
}

/// Whether a file's `account_id` member is `expected` (Go v8.0.67 6bbd2cb7): the same
/// string, or for `""` none, `null` or `""`. Any other value is another account.
fn holds_account(meta: &Map<String, Value>, expected: &str) -> bool {
    match meta.get("account_id") {
        None | Some(Value::Null) => expected.is_empty(),
        Some(Value::String(account)) => account == expected,
        Some(_) => false,
    }
}

/// The issued client a SIWC partition holds: `Ok("")` for none, `Err` for a member
/// that is not a usable issued client (not a string, or the registration client).
fn saved_client(meta: &Map<String, Value>) -> Result<String, ()> {
    let Some(Value::Object(siwc)) = meta.get("siwc") else {
        return Ok(String::new());
    };
    match siwc.get("client_id") {
        None => Ok(String::new()),
        Some(Value::String(id)) if id.trim() == codex_siwc::DYNAMIC_CLIENT_ID => Err(()),
        Some(Value::String(id)) => Ok(id.trim().to_owned()),
        Some(_) => Err(()),
    }
}

/// Go `siwcBindEligible`.
pub(super) fn eligible(c: &Credential) -> bool {
    c.provider.eq_ignore_ascii_case("codex")
        && path_of(c).is_some()
        && !runtime_only(c)
        && auth_kind(c) == Some("oauth")
        && has_outer(&c.metadata)
}

/// Go `sameSIWCMutationPath`'s canonical form: symlinks resolved, else those of the
/// parent (for a file that is gone), else the absolute path.
fn canonical(path: &Path) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(path) {
        return real;
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
        && let Ok(parent) = std::fs::canonicalize(if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        })
    {
        return parent.join(name);
    }
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The spelling two canonical paths are compared in, never used for I/O: Go's
/// `filepath.Clean` (`.` dropped, `..` applied lexically, never above the root).
fn clean(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    let mut depth = 0usize;
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => out.push(component),
            Component::CurDir => {}
            Component::ParentDir if depth > 0 => {
                out.pop();
                depth -= 1;
            }
            Component::ParentDir if !out.has_root() => out.push(".."),
            Component::ParentDir => {}
            Component::Normal(name) => {
                out.push(name);
                depth += 1;
            }
        }
    }
    out
}

/// Go `sameSIWCMutationPath`: one file (Go `os.SameFile`, which also catches hard links
/// and another spelling on a case-insensitive file system), else one canonical path.
pub(super) fn same_path(a: &Path, b: &Path) -> bool {
    if let (Some(x), Some(y)) = (file_id(a), file_id(b))
        && x == y
    {
        return true;
    }
    clean(&canonical(a)) == clean(&canonical(b))
}

/// The file `path` names after symlinks, as Go's `os.Stat` identifies it.
fn file_id(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

/// Go `resolveSIWCBindFile`: the credential's real path inside `auth-dir` and its
/// current content. The file itself must not be a symlink; the folder may be.
fn resolve(auth_dir: &Path, path: &Path) -> Option<(PathBuf, Map<String, Value>)> {
    let dir = std::fs::canonicalize(auth_dir).ok()?;
    if !std::fs::symlink_metadata(path).ok()?.file_type().is_file() {
        return None;
    }
    let real = std::fs::canonicalize(path).ok()?;
    if real.strip_prefix(&dir).ok()?.as_os_str().is_empty() || !std::fs::metadata(&real).ok()?.is_file() {
        return None;
    }
    let meta: Map<String, Value> = serde_json::from_slice(&std::fs::read(&real).ok()?).ok()?;
    let codex = meta
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|t| t.trim().eq_ignore_ascii_case("codex"));
    (codex && has_outer(&meta)).then_some((real, meta))
}

/// Go `StartSIWCBind`: `POST /v8/management/oauth/siwc/bind {"name": ...}`.
pub(crate) async fn bind(
    State(state): State<Arc<Management>>,
    crate::body_limit::FirstJson(body): crate::body_limit::FirstJson,
) -> Response {
    // gin `ShouldBindJSON` into `struct{ Name string }`: the first JSON value; members
    // apply in order, the name matched under case folding; null leaves it empty.
    let Some(members) = super::api_call::bind_struct(&body, &["name"]) else {
        return fail(StatusCode::BAD_REQUEST, "invalid request body");
    };
    let mut name = String::new();
    for (_, value) in members {
        match value {
            Value::String(s) => name = s,
            Value::Null => {}
            _ => return fail(StatusCode::BAD_REQUEST, "invalid request body"),
        }
    }
    let name = name.trim().to_owned();
    if name.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "name is required");
    }
    // Go holds its commit gate from the lookup through registration: a v8 upload or
    // delete lands before (and is the target) or after (and fails the binding).
    tokio::task::spawn_blocking(move || {
        let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
        start(&state, &name)
    })
    .await
    .unwrap_or_else(|_| fail(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))
}

/// [`bind`] after the request is read; callers hold `disk`.
fn start(state: &Management, name: &str) -> Response {
    let all = state.rt.store().snapshot();
    let Some(target) = all
        .iter()
        .find(|c| c.id == name)
        .or_else(|| all.iter().find(|c| file_name(c).as_deref() == Some(name)))
    else {
        return fail(StatusCode::NOT_FOUND, "auth file not found");
    };
    if !eligible(target) {
        return fail(
            StatusCode::BAD_REQUEST,
            "SIWC binding requires a file-backed Codex OAuth credential",
        );
    }
    let cfg = state.rt.config();
    let resolved = path_of(target).and_then(|p| resolve(&cfg.auth_dir, p));
    let Some((path, meta)) = resolved else {
        return fail(StatusCode::BAD_REQUEST, "SIWC binding target is unavailable or invalid");
    };
    // The file must still hold the loaded credential's account (a replacement the
    // watcher has not applied yet is not this target), and a saved issued client must
    // be usable for reauthorization.
    let account = account_of(target).to_owned();
    let client = saved_client(&meta);
    let (true, Ok(client)) = (holds_account(&meta, &account), client) else {
        return fail(StatusCode::BAD_REQUEST, "SIWC binding target is unavailable or invalid");
    };
    // Rebinding keeps a valid device ID.
    let device_id = meta
        .get("siwc")
        .and_then(|s| s.get("device_id"))
        .and_then(Value::as_str)
        .filter(|id| codex_siwc::is_valid_uuid(id))
        .map_or_else(|| uuid::Uuid::new_v4().to_string(), |id| id.trim().go_lower());
    let (Ok((verifier, challenge)), Ok(sid), Ok(nonce)) = (
        cpa_exec::oauth::pkce(),
        codex_siwc::random_base64url(),
        codex_siwc::random_base64url(),
    ) else {
        return fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to initialize SIWC authorization",
        );
    };
    let identifier = codex_siwc::identifier(&cfg);
    let url = if client.is_empty() {
        codex_siwc::authorize_url(&sid, &nonce, &device_id, REDIRECT_URI, &challenge, &identifier)
    } else {
        codex_siwc::reauth_url(&client, &sid, &nonce, &device_id, REDIRECT_URI, &challenge)
    };
    let Ok(url) = url else {
        return fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to generate SIWC authorization URL",
        );
    };
    let flow = Arc::new(Flow {
        target_id: target.id.clone(),
        path,
        device_id,
        account,
        client,
        verifier,
        deadline: Instant::now() + TTL,
        exchange: Mutex::default(),
    });
    if !state.oauth.register_siwc(&sid, flow, TTL) {
        return fail(
            StatusCode::CONFLICT,
            "a SIWC binding is already pending for this credential",
        );
    }
    reply(
        StatusCode::OK,
        [
            ("status", "ok".into()),
            ("url", url.into()),
            ("state", sid.into()),
            ("flow", "siwc_bind".into()),
            ("expires_in", TTL.as_secs().into()),
        ],
    )
}

/// Whether an upstream refused `c`'s SIWC token: SIWC is in use, so the request token
/// was the SIWC one, and the store holds a 401 cooldown for the credential or its
/// tokens were refused for good. Both clear when the tokens change or a binding lands.
// ponytail: Go records the rejected SIWC token itself (`RejectedSIWCAccessToken`), so
// its `invalid` outlives the 30-minute 401 cooldown, an outer token refresh and a
// `use_siwc` toggle; port that with A4's renewal state if the status needs it.
fn rejected(store: &crate::runtime::CredentialStore, c: &Credential) -> bool {
    codex_siwc::enabled(c)
        && (store.tokens_rejected(c) || store.cooldowns(&c.id).iter().any(|cooldown| cooldown.status == 401))
}

/// Go v8.0.67 `CodexSIWCStatus` for a stored credential, now.
fn local_status(store: &crate::runtime::CredentialStore, c: &Credential) -> &'static str {
    codex_siwc::status(c, rejected(store, c), chrono::Utc::now())
}

/// Go v8.0.67 `GetSIWCStatus` (06fd5b8b): `GET /v8/management/oauth/siwc/status` and
/// `/v8/management/credentials/siwc/status`, `?name=` (or `id`) and/or `?auth_index=`
/// (or `authIndex`). Reads the loaded credential and the store's rejection state only:
/// no lock, no file read, no refresh, no upstream request, and never a token.
pub(crate) async fn status(
    State(state): State<Arc<Management>>,
    axum::extract::RawQuery(raw): axum::extract::RawQuery,
) -> Response {
    let q = Query::parse(raw);
    let pick = |keys: [&str; 2]| {
        keys.iter()
            .map(|k| q.first(k).trim().to_owned())
            .find(|v| !v.is_empty())
            .unwrap_or_default()
    };
    let (name, index) = (pick(["name", "id"]), pick(["auth_index", "authIndex"]));
    if name.is_empty() && index.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "name or auth_index is required");
    }
    let store = state.rt.store();
    let target = if name.is_empty() {
        store
            .snapshot()
            .into_iter()
            .find(|c| cpa_core::config::credentials::auth_index(c) == index)
    } else {
        lookup(&state, &name, &index)
    };
    let Some(target) = target else {
        return fail(StatusCode::NOT_FOUND, "auth file not found");
    };
    // Go `IsCodexAuthRecord`.
    let codex =
        target.provider.trim().go_eq_fold("codex") || target.str("type").is_some_and(|t| t.trim().go_eq_fold("codex"));
    if !codex {
        return fail(StatusCode::BAD_REQUEST, "SIWC status requires a Codex credential");
    }
    reply(StatusCode::OK, [("status", local_status(store, &target).into())])
}

fn status_error(status: StatusCode, message: &str) -> Response {
    reply(status, [("status", "error".into()), ("error", message.into())])
}

/// The scheme, host and path of an already-parsed callback URL, as Go compares them,
/// or `None` when it carries user info.
fn callback_target(raw: &str) -> Option<(String, String, String)> {
    // Go `URL.Path`: escapes decoded (Go's parser has rejected bad ones), `+`, `&` and
    // `;` literal, nothing normalized.
    let parsed = cpa_core::config::go_url::parse(raw)?;
    let before_query = raw.split('#').next()?.split('?').next()?;
    let (_, rest) = before_query.split_once("://")?;
    let (authority, path) = rest.find('/').map_or((rest, ""), |i| rest.split_at(i));
    if authority.contains('@') {
        return None;
    }
    let path = super::percent_decode(path);
    let host = parsed.host().to_owned();
    Some((parsed.scheme, host, path))
}

/// Go `handleOAuthCallbackV8` for a binding's state; `None` hands any other state to
/// the ordinary callback. `query` is the GET query, whose keys may not repeat.
pub(super) fn callback(state: &Arc<Management>, req: &CallbackRequest, query: Option<&Query>) -> Option<Response> {
    let mut sid = req.state.trim().to_owned();
    let redirect_raw = req.redirect_url.trim();
    let mut redirect = None;
    if !redirect_raw.is_empty() {
        if cpa_core::config::go_url::parse(redirect_raw).is_none() {
            return Some(status_error(StatusCode::BAD_REQUEST, "invalid redirect_url"));
        }
        let q = Query::parse(Some(
            redirect_raw
                .split('#')
                .next()
                .and_then(|u| u.split_once('?'))
                .map(|(_, q)| q.to_owned())
                .unwrap_or_default(),
        ));
        if sid.is_empty() {
            sid = q.first("state").trim().to_owned();
        }
        redirect = Some(q);
    }
    let flow = state.oauth.siwc(&sid)?;
    let conflicting = || Some(status_error(StatusCode::BAD_REQUEST, "conflicting callback parameters"));
    if let Some(q) = query
        && ["provider", "state", "code", "client_id", "error"]
            .iter()
            .any(|k| q.all(k).len() > 1)
    {
        return conflicting();
    }
    let provider = req.provider.trim();
    if !provider.is_empty() && super::oauth::normalize_callback_provider(provider).as_deref() != Some("codex") {
        return Some(status_error(StatusCode::BAD_REQUEST, "provider does not match state"));
    }
    let (mut code, mut client_id, mut error) = (
        req.code.trim().to_owned(),
        req.client_id.trim().to_owned(),
        req.error.trim().to_owned(),
    );
    if let Some(q) = &redirect {
        let expected = Some((
            "http".to_owned(),
            "127.0.0.1:1455".to_owned(),
            "/auth/callback".to_owned(),
        ));
        let fragment = redirect_raw.split_once('#').is_some_and(|(_, f)| !f.is_empty());
        if fragment || callback_target(redirect_raw) != expected {
            return Some(status_error(StatusCode::BAD_REQUEST, "unexpected SIWC callback URL"));
        }
        let mut given = req.state.trim().to_owned();
        for (key, target) in [
            ("state", &mut given),
            ("code", &mut code),
            ("client_id", &mut client_id),
            ("error", &mut error),
        ] {
            let value = q.first(key).trim();
            if q.all(key).len() > 1 || (!value.is_empty() && !target.is_empty() && value != target) {
                return conflicting();
            }
            if target.is_empty() {
                *target = value.to_owned();
            }
        }
    }
    // A rebinding reauthorizes with the saved issued client: a callback may omit it,
    // and one naming another client is refused without consuming the session.
    if !flow.client.is_empty() && error.is_empty() {
        if !client_id.is_empty() && client_id != flow.client {
            return Some(status_error(
                StatusCode::BAD_REQUEST,
                "callback client ID does not match the saved registration",
            ));
        }
        client_id = flow.client.clone();
    }
    if error.is_empty() && (code.is_empty() || client_id.is_empty()) {
        return Some(status_error(
            StatusCode::BAD_REQUEST,
            "SIWC callback requires code and client_id",
        ));
    }
    if !state.oauth.begin_siwc(&sid, &flow) {
        return Some(status_error(
            StatusCode::CONFLICT,
            "SIWC session is no longer accepting callbacks",
        ));
    }
    let ok = Some(reply(StatusCode::OK, [("status", "ok".into())]));
    if !error.is_empty() {
        // Never the provider's text: it is readable by anyone holding the state.
        fail_session(state, &sid, &flow, "SIWC authorization was denied or failed");
        return ok;
    }
    let (worker, task_flow) = (state.clone(), flow.clone());
    let (guard, guard_sid, guard_flow) = (state.clone(), sid.clone(), flow.clone());
    let work = async move {
        let oauth = match &worker.login_base {
            Some(base) => cpa_exec::codex_oauth::CodexOAuth::new(super::oauth::login_client(&worker))
                .with_siwc_token_url(&format!("{base}/api/accounts/oauth/token")),
            None => cpa_exec::codex_oauth::CodexOAuth::new(super::oauth::login_client(&worker)),
        };
        let exchange = oauth.exchange_siwc(
            &code,
            &client_id,
            REDIRECT_URI,
            &task_flow.device_id,
            &task_flow.verifier,
        );
        // Past the deadline the session has expired: nothing to report.
        let Ok(tokens) = tokio::time::timeout_at(task_flow.deadline.into(), exchange).await else {
            return;
        };
        // The credential's preparation lock, which every outer token refresh holds from
        // its read to its commit: the binding lands before or after a refresh, so it
        // never makes a rotated outer token stale (Go keeps both through its mutation
        // gate). Owned and moved into the commit, so a cancel that aborts this task while
        // the commit runs does not release it.
        let lock = worker.rt.store().prepare_lock(&task_flow.target_id);
        let Ok(prepared) = tokio::time::timeout_at(task_flow.deadline.into(), lock.lock_owned()).await else {
            return;
        };
        let tokens = tokens.ok();
        let _ = super::auth_files::with_codex_lock(&worker, move |state, lock| {
            let _prepared = &prepared;
            // The commit runs on a blocking thread, outside the worker's panic catch.
            let committed = catch_unwind(AssertUnwindSafe(|| {
                commit(state, &sid, &task_flow, tokens.as_ref(), lock)
            }));
            committed.unwrap_or_else(|_| {
                tracing::error!("codex SIWC commit panicked; sign-in {sid} failed");
                fail_session(state, &sid, &task_flow, super::oauth::PANIC_MESSAGE);
                Ok(())
            })
        })
        .await;
    };
    let task = tokio::spawn(async move {
        if super::oauth::panicked(&guard_sid, "codex SIWC", work).await {
            fail_session(&guard, &guard_sid, &guard_flow, super::oauth::PANIC_MESSAGE);
        }
    });
    *flow.exchange.lock().unwrap_or_else(PoisonError::into_inner) = Some(task.abort_handle());
    ok
}

/// Go `siwcBindSession.fail`: the fixed message, never an upstream body.
fn fail_session(state: &Management, sid: &str, flow: &Flow, message: &str) {
    state.oauth.set_error(sid, message);
    flow.cancel();
    settled(state, sid);
}

fn settled(state: &Management, sid: &str) {
    if let Some(settled) = &state.login_settled {
        let _ = settled.send(sid.to_owned());
    }
}

/// Go `siwcBindSession.exchange` after the token response. Holds `disk` from the
/// pending check through publication, so v8 uploads and deletes of the file land
/// before (failing the binding) or after it, and the file's credential lock from
/// before the commit is claimed (Go `BindCodexSIWC`'s `WithFileLock`); `Err` names
/// the file when another holder has that lock ([`super::auth_files::with_codex_lock`]).
/// Blocking.
fn commit(
    state: &Management,
    sid: &str,
    flow: &Arc<Flow>,
    tokens: Option<&Tokens>,
    lock: &mut CodexLock,
) -> Result<(), PathBuf> {
    let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
    if !state.oauth.siwc_is_pending(sid, flow) {
        settled(state, sid);
        return Ok(());
    }
    let Some(tokens) = tokens else {
        fail_session(
            state,
            sid,
            flow,
            "Failed to exchange SIWC authorization code; start a new binding",
        );
        return Ok(());
    };
    let cfg = state.rt.config();
    if resolve(&cfg.auth_dir, &flow.path).is_none_or(|(path, _)| path != flow.path) {
        fail_session(state, sid, flow, "SIWC binding target is no longer available");
        return Ok(());
    }
    let target = state.rt.store().get(&flow.target_id);
    match target
        .as_deref()
        .and_then(path_of)
        .map_or(Ok(()), |path| lock.hold(path))
    {
        Ok(()) => {}
        Err(PatchError::LockBusy(path)) => return Err(path),
        Err(e) => {
            tracing::warn!("SIWC binding was not saved: {e:?}");
            fail_session(
                state,
                sid,
                flow,
                "Failed to save SIWC binding; the target may have changed",
            );
            return Ok(());
        }
    }
    if !state.oauth.claim_siwc_commit(sid, flow) {
        settled(state, sid);
        return Ok(());
    }
    match write_binding(state, flow, tokens, lock) {
        Ok(()) => {
            state.rt.store().recover_unauthorized(&flow.target_id);
            state.oauth.complete(sid);
            settled(state, sid);
        }
        Err(e) => {
            tracing::warn!("SIWC binding was not saved: {e}");
            fail_session(
                state,
                sid,
                flow,
                "Failed to save SIWC binding; the target may have changed",
            );
        }
    }
    Ok(())
}

/// Go `Manager.BindCodexSIWC`: re-reads the current credential and its file and
/// replaces only `siwc` and `use_siwc`, while the credential is still the record it
/// was read from (a refresh in between retries on the refreshed file). A failed write
/// publishes nothing.
///
/// ponytail: Go also rejects a target re-registered since the start (its registration
/// epoch, not reproduced here, M5-0247); this checks the same file and provider, the
/// account the binding started with in both the loaded credential and the file, and
/// the file's bytes up to the write. v8 uploads and deletes fail the binding explicitly.
fn write_binding(state: &Management, flow: &Flow, tokens: &Tokens, lock: &mut CodexLock) -> Result<(), String> {
    for _ in 0..8 {
        let current = state
            .rt
            .store()
            .snapshot()
            .into_iter()
            .find(|c| c.id == flow.target_id)
            .ok_or("target is missing")?;
        if !eligible(&current) {
            return Err("target is no longer a file-backed Codex OAuth credential".into());
        }
        let path = path_of(&current).ok_or("target is not file-backed")?;
        // Read, merged and written under the file's credential lock, so a refresh in
        // another process lands before the read or after the write.
        lock.hold(path).map_err(|e| format!("{e:?}"))?;
        if !std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
            || std::fs::canonicalize(path).ok().as_deref() != Some(flow.path.as_path())
        {
            return Err("backing file changed".into());
        }
        let disk = std::fs::read(path).map_err(|e| format!("read backing file: {e}"))?;
        let meta: Map<String, Value> = serde_json::from_slice(&disk).map_err(|_| "invalid Codex OAuth file")?;
        let codex = meta
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| t.trim().eq_ignore_ascii_case("codex"));
        if !codex || !has_outer(&meta) {
            return Err("invalid Codex OAuth file".into());
        }
        // The account the binding started on, in the loaded credential and the file.
        if account_of(&current) != flow.account || !holds_account(&meta, &flow.account) {
            return Err("backing account changed".into());
        }
        let bytes = codex_siwc::bind_file(&disk, tokens).ok_or("invalid Codex OAuth file")?;
        #[cfg(test)]
        {
            let mut hooks = tests::BEFORE_SAVE.lock().unwrap();
            let hook = hooks
                .iter()
                .position(|(p, _)| p == &flow.path)
                .map(|i| hooks.remove(i).1);
            drop(hooks);
            if let Some(hook) = hook {
                hook();
            }
        }
        // Saved only while the file still holds the bytes read above (Go 88041f0c): a
        // writer that does not take the credential lock is never overwritten.
        let read: [u8; 32] = <sha2::Sha256 as sha2::Digest>::digest(&disk).into();
        match super::auth_files::save_bytes_if(state, &current, bytes, read) {
            Ok(()) => return Ok(()),
            // The store's record moved on (a refresh committed): bind onto it.
            Err(crate::runtime::PatchError::Stale { current: now }) if now != current.revision => continue,
            Err(crate::runtime::PatchError::Stale { .. }) => {
                return Err("backing file changed during binding".into());
            }
            Err(e) => return Err(format!("{e:?}")),
        }
    }
    Err("the credential kept changing".into())
}

/// Go's v8 credential-list summary for Codex: support, the toggle, whether a SIWC
/// token set is stored (not whether it works) and its expiry. Never a token.
pub(super) fn summary(
    c: &Credential,
    store: &crate::runtime::CredentialStore,
    entry: &mut std::collections::BTreeMap<&'static str, Value>,
) {
    if !c.provider.trim().eq_ignore_ascii_case("codex") {
        return;
    }
    // Go v8.0.67 `siwc_status` (06fd5b8b).
    entry.insert("siwc_status", local_status(store, c).into());
    entry.insert("siwc_bind_supported", eligible(c).into());
    // Go `IsCodexUseSIWC`: a true attribute wins, else the file's flag.
    let flag = |v: &Value| match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) => super::auth_files::go_parse_bool(s.trim()),
        Value::Number(n) => n.as_f64().map(|f| f != 0.0),
        _ => None,
    };
    let use_siwc = c
        .attributes
        .get("use_siwc")
        .and_then(|a| super::auth_files::go_parse_bool(a.trim()))
        .filter(|b| *b)
        .or_else(|| c.metadata.get("use_siwc").and_then(flag))
        .unwrap_or(false);
    entry.insert("use_siwc", use_siwc.into());
    let section = c.metadata.get("siwc").and_then(Value::as_object);
    let bound = section
        .is_some_and(|s| non_empty(s, "access_token") && non_empty(s, "refresh_token") && non_empty(s, "client_id"));
    entry.insert("siwc_bound", bound.into());
    if let Some(expiry) = section
        .filter(|s| !s.is_empty())
        .and_then(super::auth_files::siwc_expiry)
    {
        let utc: chrono::DateTime<chrono::Utc> = expiry.into();
        entry.insert(
            "siwc_expires_at",
            utc.to_rfc3339_opts(chrono::SecondsFormat::Secs, true).into(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body_limit::FirstJson;

    /// Runs once in the next commit of a binding of the file it names (its real path),
    /// after the commit read the file and before it saves.
    #[allow(clippy::type_complexity)]
    pub(super) static BEFORE_SAVE: Mutex<Vec<(PathBuf, Box<dyn FnOnce() + Send>)>> = Mutex::new(Vec::new());

    const GRANT: &str = r#"{"access_token":"siwc-at","refresh_token":"siwc-rt","id_token":"siwc-id","scope":"openid chatgpt.tokens.use.direct","expires_in":3600}"#;

    /// A loopback SIWC token endpoint answering [`GRANT`]; returns its base URL and the
    /// client IDs it was asked with.
    async fn token_endpoint() -> (String, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let app = axum::Router::new().route(
            "/api/accounts/oauth/token",
            axum::routing::post({
                let seen = seen.clone();
                move |body: axum::body::Bytes| async move {
                    let form: std::collections::HashMap<String, String> =
                        url::form_urlencoded::parse(&body).into_owned().collect();
                    seen.lock()
                        .unwrap()
                        .push(form.get("client_id").cloned().unwrap_or_default());
                    GRANT
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, seen)
    }

    async fn start_binding(state: &Arc<Management>, name: &str) -> Response {
        bind(
            State(state.clone()),
            FirstJson(axum::body::Bytes::from(format!(r#"{{"name":"{name}"}}"#))),
        )
        .await
    }

    /// Sends the binding's callback with `client_id` and waits for its outcome: the
    /// session's status text (empty once completed) and whether it completed.
    async fn finish(state: &Arc<Management>, sid: &str, client_id: &str) -> (String, bool) {
        let req = CallbackRequest {
            state: sid.to_owned(),
            code: "c".into(),
            client_id: client_id.into(),
            ..Default::default()
        };
        let response = callback(state, &req, None).unwrap();
        if response.status() != StatusCode::OK {
            let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
            return (String::from_utf8_lossy(&body).into_owned(), false);
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let (_, status, completed) = state.oauth.details(sid).unwrap();
                if completed || !status.is_empty() {
                    return (status, completed);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the binding settles")
    }

    const SAVE_FAILED: &str = "Failed to save SIWC binding; the target may have changed";

    /// #91 (Go v8.0.67 6bbd2cb7): a binding started on a credential without an account
    /// never lands on one that has an account, whether the replacement reached the store
    /// (a watcher reload) or only the file. Before, only an account present at start was
    /// compared, so an account-less start bound onto any account.
    #[tokio::test]
    async fn a_binding_started_without_an_account_never_lands_on_one() {
        let (base, _) = token_endpoint().await;
        for published in [true, false] {
            let (dir, file, state) = fixture(Some(base.clone()));
            let accountless = r#"{"type":"codex","access_token":"a","refresh_token":"r"}"#;
            std::fs::write(&file, accountless).unwrap();
            state.publish((*state.rt.config()).clone(), None);
            let sid = started_state(start_binding(&state, "codex.json").await).await;
            assert_eq!(state.oauth.siwc(&sid).unwrap().account, "");
            let replaced = codex_file("acct-b");
            std::fs::write(&file, &replaced).unwrap();
            if published {
                state.publish((*state.rt.config()).clone(), None);
            }
            let outcome = finish(&state, &sid, "oaiapp_issued").await;
            assert_eq!(outcome, (SAVE_FAILED.to_owned(), false), "published: {published}");
            assert_eq!(
                std::fs::read_to_string(&file).unwrap(),
                replaced,
                "published: {published}"
            );
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    /// #91 (Go 6bbd2cb7 `StartSIWCBind`): a start refuses a target whose file holds
    /// another account than the credential the store has loaded (a replacement the
    /// watcher has not applied yet), instead of binding the file's account.
    #[tokio::test]
    async fn a_start_refuses_a_file_that_holds_another_account() {
        for (loaded, disk) in [
            (codex_file("old"), codex_file("new")),
            (
                codex_file("old"),
                r#"{"type":"codex","access_token":"a","refresh_token":"r"}"#.to_owned(),
            ),
            (
                r#"{"type":"codex","access_token":"a","refresh_token":"r"}"#.to_owned(),
                codex_file("new"),
            ),
        ] {
            let (dir, file, state) = fixture(None);
            std::fs::write(&file, &loaded).unwrap();
            state.publish((*state.rt.config()).clone(), None);
            std::fs::write(&file, &disk).unwrap();
            let response = start_binding(&state, "codex.json").await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{loaded} -> {disk}");
            let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert_eq!(
                &body[..],
                br#"{"error":"SIWC binding target is unavailable or invalid"}"#
            );
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    /// #91 (Go 88041f0c): a writer that does not take the credential lock rewrites the
    /// file in place after the commit read it: the binding fails and the file keeps that
    /// writer's bytes. Before, the commit overwrote them.
    #[tokio::test]
    async fn an_in_place_rewrite_during_the_commit_fails_the_binding() {
        let (base, _) = token_endpoint().await;
        let (dir, file, state) = fixture(Some(base));
        let sid = started_state(start_binding(&state, "codex.json").await).await;
        let external = codex_file("old").replace(r#""refresh_token":"r""#, r#""refresh_token":"r-external""#);
        BEFORE_SAVE.lock().unwrap().push((
            std::fs::canonicalize(&file).unwrap(),
            Box::new({
                let (file, external) = (file.clone(), external.clone());
                move || std::fs::write(&file, external).unwrap()
            }),
        ));
        let outcome = finish(&state, &sid, "oaiapp_issued").await;
        assert_eq!(outcome, (SAVE_FAILED.to_owned(), false));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), external);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// #91 (Go 88041f0c): a legacy (v0) upload of the target between the start and the
    /// commit does not fail the session by itself, unlike a v8 one. One that brings
    /// another account fails the binding; one that keeps the account (a rotated outer
    /// token) is what the binding lands on, its tokens kept.
    #[tokio::test]
    async fn a_legacy_upload_during_a_binding() {
        let (base, _) = token_endpoint().await;
        for (account, lands) in [("acct-b", false), ("old", true)] {
            let (dir, file, state) = fixture(Some(base.clone()));
            let sid = started_state(start_binding(&state, "codex.json").await).await;
            let upload = codex_file(account).replace(r#""refresh_token":"r""#, r#""refresh_token":"r-uploaded""#);
            let stored = super::super::auth_files::with_codex_lock(&state, {
                let upload = upload.clone();
                move |state, lock| {
                    super::super::auth_files::store_file(state, "codex.json", upload.as_bytes(), false, lock)
                }
            })
            .await
            .unwrap();
            assert!(stored.is_ok(), "{account}: {stored:?}");
            let outcome = finish(&state, &sid, "oaiapp_issued").await;
            let disk: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
            assert_eq!(
                disk["refresh_token"], "r-uploaded",
                "{account}: the upload's tokens stay"
            );
            if lands {
                assert_eq!(outcome, (String::new(), true), "{account}");
                assert_eq!(disk["siwc"]["client_id"], "oaiapp_issued", "{account}");
            } else {
                assert_eq!(outcome, (SAVE_FAILED.to_owned(), false), "{account}");
                assert!(disk.get("siwc").is_none(), "{account}");
            }
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    /// A token refresh committed through the store after the commit read the file
    /// changes the record and the file: the commit binds again onto the refreshed file,
    /// so the rotated outer token is kept (the byte fence does not fail it).
    #[tokio::test]
    async fn a_refresh_committed_during_the_commit_is_kept() {
        let (base, _) = token_endpoint().await;
        let (dir, file, state) = fixture(Some(base));
        let sid = started_state(start_binding(&state, "codex.json").await).await;
        BEFORE_SAVE.lock().unwrap().push((
            std::fs::canonicalize(&file).unwrap(),
            Box::new({
                let state = state.clone();
                move || {
                    let store = state.rt.store();
                    let revision = store.get("codex.json").unwrap().revision;
                    let mut patch = cpa_core::credential::MetadataPatch::default();
                    patch.set.insert("refresh_token".into(), "r-refreshed".into());
                    store.apply_patch("codex.json", revision, &patch).unwrap();
                }
            }),
        ));
        assert_eq!(finish(&state, &sid, "oaiapp_issued").await, (String::new(), true));
        let disk: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(
            (&disk["refresh_token"], &disk["siwc"]["client_id"]),
            (&Value::from("r-refreshed"), &Value::from("oaiapp_issued"))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// #91 (Go bb2d8f82): with a relative `auth-dir`, the start and the commit resolve
    /// the credential against the working directory, once, and the binding lands.
    #[tokio::test]
    async fn a_relative_auth_dir_binds() {
        let (base, _) = token_endpoint().await;
        let (dir, _, state) = fixture(Some(base));
        let local = PathBuf::from(format!("cpa-siwc-relative-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&local).unwrap();
        std::fs::write(local.join("codex.json"), codex_file("old")).unwrap();
        std::fs::write(&state.path, format!("oauth: {{auth-dir: {}}}\n", local.display())).unwrap();
        crate::watching::reload(&state).unwrap();
        assert!(state.rt.config().auth_dir.is_relative());
        let sid = started_state(start_binding(&state, "codex.json").await).await;
        assert_eq!(finish(&state, &sid, "oaiapp_issued").await, (String::new(), true));
        let bound: Value = serde_json::from_slice(&std::fs::read(local.join("codex.json")).unwrap()).unwrap();
        assert_eq!(bound["siwc"]["client_id"], "oaiapp_issued");
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(local).unwrap();
    }

    /// #91 (OpenAI "Accounts and sessions"; Go 3891ebaa for the CLI): rebinding a
    /// credential that holds an issued client reauthorizes with it, without
    /// `agent_name_hint`. A callback naming another client is refused and leaves the
    /// session open; one without a client ID exchanges with the saved client.
    #[tokio::test]
    async fn a_rebinding_reuses_the_saved_issued_client() {
        let (base, seen) = token_endpoint().await;
        let (dir, file, state) = fixture(Some(base));
        let saved = r#"{"type":"codex","account_id":"old","access_token":"a","refresh_token":"r","siwc":{"access_token":"s","refresh_token":"sr","client_id":"oaiapp_saved","device_id":"e61bbe28-07ef-466d-8e5d-a344f94ab305"},"use_siwc":true}"#;
        std::fs::write(&file, saved).unwrap();
        state.publish((*state.rt.config()).clone(), None);
        let response = start_binding(&state, "codex.json").await;
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let started: Value = serde_json::from_slice(&body).unwrap();
        let url = url::Url::parse(started["url"].as_str().unwrap()).unwrap();
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query["client_id"], "oaiapp_saved");
        assert!(!query.contains_key("agent_name_hint"), "{query:?}");
        assert_eq!(
            query["ext_agent_host_id"],
            "urn:uuid:e61bbe28-07ef-466d-8e5d-a344f94ab305"
        );
        let sid = started["state"].as_str().unwrap().to_owned();
        let (refused, _) = finish(&state, &sid, "oaiapp_other").await;
        assert!(
            refused.contains("callback client ID does not match the saved registration"),
            "{refused}"
        );
        assert!(seen.lock().unwrap().is_empty(), "nothing exchanged");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), saved);
        assert_eq!(finish(&state, &sid, "").await, (String::new(), true));
        assert_eq!(*seen.lock().unwrap(), ["oaiapp_saved"]);
        let bound: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(bound["siwc"]["client_id"], "oaiapp_saved");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// #91 (Go v8.0.67 06fd5b8b): `GET /v8/management/oauth/siwc/status` and its alias
    /// `/v8/management/credentials/siwc/status` report a Codex credential's local SIWC
    /// status, by name or auth index, behind the management key: `off` until SIWC is in
    /// use, `valid` with an unexpired token, `invalid` once an upstream 401 refused it or
    /// its real expiry passed. The answer carries no token, and asking changes nothing:
    /// the file, the record and the token endpoint stay untouched.
    #[tokio::test]
    async fn the_siwc_status_route_is_read_only_and_reveals_no_token() {
        use tower::ServiceExt as _;
        let (base, seen) = token_endpoint().await;
        let (dir, file, state) = fixture_with(crate::management::Options {
            login_base: Some(base),
            management_password: Some("fake-management-only".into()),
            ..Default::default()
        });
        std::fs::write(
            dir.join("claude.json"),
            r#"{"type":"claude","access_token":"a","refresh_token":"b"}"#,
        )
        .unwrap();
        state.publish((*state.rt.config()).clone(), None);
        let get = |path: String, key: Option<&'static str>| {
            let state = state.clone();
            async move {
                let mut request = axum::http::Request::get(path).extension(axum::extract::ConnectInfo(
                    std::net::SocketAddr::from(([127, 0, 0, 1], 1234)),
                ));
                if let Some(key) = key {
                    request = request.header("authorization", format!("Bearer {key}"));
                }
                let response = crate::management::router(state)
                    .oneshot(request.body(axum::body::Body::empty()).unwrap())
                    .await
                    .unwrap();
                let status = response.status().as_u16();
                let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
                (status, String::from_utf8(body.to_vec()).unwrap())
            }
        };
        let key = Some("fake-management-only");
        let both = |query: &str| {
            [
                format!("/v8/management/oauth/siwc/status?{query}"),
                format!("/v8/management/credentials/siwc/status?{query}"),
            ]
        };
        for path in both("name=codex.json") {
            assert_eq!(get(path, key).await, (200, r#"{"status":"off"}"#.to_owned()));
        }
        let sid = started_state(start_binding(&state, "codex.json").await).await;
        assert_eq!(finish(&state, &sid, "oaiapp_issued").await, (String::new(), true));
        let bound = state.rt.store().get("codex.json").unwrap();
        // Asking changes nothing: the file, the record and the token endpoint.
        let untouched = |why: &str| {
            let snapshot = (
                std::fs::read(&file).unwrap(),
                state.rt.store().get("codex.json").unwrap().revision,
                seen.lock().unwrap().len(),
            );
            let why = why.to_owned();
            let (file, state, seen) = (file.clone(), state.clone(), seen.clone());
            move || {
                let now = (
                    std::fs::read(&file).unwrap(),
                    state.rt.store().get("codex.json").unwrap().revision,
                    seen.lock().unwrap().len(),
                );
                assert!(now == snapshot, "{why}: the status changed something");
            }
        };
        let check = untouched("valid");
        let index = cpa_core::config::credentials::auth_index(&bound);
        for path in both("name=codex.json")
            .into_iter()
            .chain(both("id=codex.json"))
            .chain(both(&format!("auth_index={index}")))
            .chain(both(&format!("authIndex={index}")))
        {
            let (status, body) = get(path.clone(), key).await;
            assert_eq!((status, body.as_str()), (200, r#"{"status":"valid"}"#), "{path}");
        }
        check();
        // An upstream 401 on the SIWC token (its completion saves the credential's status).
        let lease = state
            .rt
            .store()
            .select(crate::runtime::Selection::new("codex", "gpt-5"))
            .expect("selectable");
        lease.complete(crate::runtime::Outcome::Failure(cpa_core::exec::ExecError::local(
            401,
            cpa_core::exec::FailureScope::Credential,
            "401 unauthorized",
        )));
        let check = untouched("invalid");
        for path in both("name=codex.json") {
            assert_eq!(get(path, key).await, (200, r#"{"status":"invalid"}"#.to_owned()));
        }
        for (path, want) in [
            ("/v8/management/oauth/siwc/status".to_owned(), 400),
            ("/v8/management/oauth/siwc/status?name=missing.json".to_owned(), 404),
            ("/v8/management/oauth/siwc/status?name=claude.json".to_owned(), 400),
            ("/v0/management/oauth/siwc/status?name=codex.json".to_owned(), 404),
        ] {
            assert_eq!(get(path.clone(), key).await.0, want, "{path}");
        }
        for path in both("name=codex.json") {
            assert_eq!(
                get(path.clone(), None).await.0,
                401,
                "{path}: the management key is required"
            );
            assert_eq!(get(path.clone(), Some("wrong")).await.0, 401, "{path}");
        }
        check();
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// #91 (Go 06fd5b8b `SIWCRealExpirationTime`): the status reads the token's real
    /// expiry (`expires_at`, then `expires_in` after the last refresh, then the JWT
    /// `exp`), never the margin-deducted `expired`.
    #[test]
    fn the_siwc_status_reads_the_real_expiry() {
        let now = chrono::DateTime::parse_from_rfc3339("2030-01-01T00:00:00Z")
            .unwrap()
            .into();
        let cred = |siwc: Value, use_siwc: bool| Credential {
            id: "c.json".into(),
            provider: "codex".into(),
            source: cpa_core::credential::Source::File("c.json".into()),
            disabled: false,
            label: String::new(),
            attributes: Default::default(),
            metadata: serde_json::json!({"type": "codex", "use_siwc": use_siwc, "siwc": siwc})
                .as_object()
                .unwrap()
                .clone(),
            revision: 0,
        };
        let jwt = |exp: i64| {
            use base64::Engine as _;
            let b = |v: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
            format!("{}.{}.sig", b(r#"{"alg":"none"}"#), b(&format!(r#"{{"exp":{exp}}}"#)))
        };
        let soon = "2030-01-01T00:02:00Z";
        for (siwc, use_siwc, want) in [
            (
                serde_json::json!({"access_token": "t", "expires_at": soon}),
                false,
                "off",
            ),
            (serde_json::json!({"expires_at": soon}), true, "invalid"),
            (
                serde_json::json!({"access_token": "t", "expires_at": soon, "expired": "2029-12-31T23:59:00Z"}),
                true,
                "valid",
            ),
            (
                serde_json::json!({"access_token": "t", "expires_at": "2029-12-31T23:59:59Z"}),
                true,
                "invalid",
            ),
            (
                serde_json::json!({"access_token": "t", "expired": soon}),
                true,
                "invalid",
            ),
            (
                serde_json::json!({"access_token": "t", "expires_in": 3600, "last_refresh": "2029-12-31T23:30:00Z"}),
                true,
                "valid",
            ),
            (
                serde_json::json!({"access_token": "t", "expires_in": 600, "last_refresh": "2029-12-31T23:30:00Z"}),
                true,
                "invalid",
            ),
            (serde_json::json!({"access_token": jwt(1_893_456_120)}), true, "valid"),
            (serde_json::json!({"access_token": jwt(1_893_455_000)}), true, "invalid"),
        ] {
            let c = cred(siwc.clone(), use_siwc);
            assert_eq!(codex_siwc::status(&c, false, now), want, "{siwc}");
        }
        let c = cred(serde_json::json!({"access_token": "t", "expires_at": soon}), true);
        assert_eq!(codex_siwc::status(&c, true, now), "invalid", "rejected");
    }

    /// Go `StartSIWCBind` holds its commit gate from the lookup through registration: a
    /// bind waiting on `disk` has read nothing yet, so a replacement made under the lock
    /// (what a v8 upload does) is the target it registers, never the file it replaced.
    #[tokio::test]
    async fn a_bind_reads_its_target_only_under_the_disk_lock() {
        let (dir, file, state) = fixture(None);
        // A writer holds `disk` until told to replace the target and publish it.
        let (locked_tx, locked) = tokio::sync::oneshot::channel();
        let (go, go_rx) = std::sync::mpsc::channel::<()>();
        let writer = tokio::task::spawn_blocking({
            let (state, file, new) = (state.clone(), file.clone(), codex_file("new"));
            move || {
                let _held = state.disk.lock().unwrap();
                locked_tx.send(()).unwrap();
                go_rx.recv().unwrap();
                std::fs::write(&file, new).unwrap();
                state.publish((*state.rt.config()).clone(), None);
            }
        });
        locked.await.unwrap();
        let started = tokio::spawn(bind(
            State(state.clone()),
            FirstJson(axum::body::Bytes::from(r#"{"name":"codex.json"}"#)),
        ));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !started.is_finished(),
            "the bind waits for disk before reading its target"
        );
        go.send(()).unwrap();
        writer.await.unwrap();
        let response = started.await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(state.oauth.siwc(&started_state(response).await).unwrap().account, "new");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A cancel that aborts the exchange task while its commit runs leaves the commit
    /// holding the preparation lock: an outer refresh cannot read the credential until
    /// the commit returns, so the binding never makes its rotated tokens stale.
    ///
    /// Single-threaded: the exchange task acquires the lock and spawns its commit in one
    /// poll, so once the test sees the lock taken the commit is running (on the blocking
    /// pool, paused on `disk`).
    #[tokio::test]
    async fn a_cancelled_binding_holds_the_preparation_lock_through_its_commit() {
        let app = axum::Router::new().route(
            "/api/accounts/oauth/token",
            axum::routing::post(|| async {
                r#"{"access_token":"siwc-at","refresh_token":"siwc-rt","id_token":"siwc-id","scope":"openid chatgpt.tokens.use.direct","expires_in":3600}"#
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (dir, _file, state) = fixture(Some(base));
        let response = bind(
            State(state.clone()),
            FirstJson(axum::body::Bytes::from(r#"{"name":"codex.json"}"#)),
        )
        .await;
        let sid = started_state(response).await;
        // Hold `disk` so the commit pauses at its start, holding the preparation lock.
        let (held_tx, held) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn({
            let state = state.clone();
            move || {
                let _disk = state.disk.lock().unwrap();
                held_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }
        });
        held.recv().unwrap();
        let req = CallbackRequest {
            state: sid.clone(),
            code: "c".into(),
            client_id: "oaiapp_issued".into(),
            ..Default::default()
        };
        assert_eq!(callback(&state, &req, None).unwrap().status(), StatusCode::OK);
        let lock = state.rt.store().prepare_lock("codex.json");
        tokio::time::timeout(Duration::from_secs(5), async {
            while lock.try_lock().is_ok() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the commit takes the preparation lock");
        let flow = state.oauth.siwc(&sid).unwrap();
        let task = flow.exchange.lock().unwrap().clone().unwrap();
        flow.cancel();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !task.is_finished() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the cancel aborts the exchange task");
        assert!(
            lock.try_lock().is_err(),
            "the paused commit still holds the preparation lock"
        );
        release.send(()).unwrap();
        holder.join().unwrap();
        tokio::time::timeout(Duration::from_secs(5), lock.lock_owned())
            .await
            .expect("released once the commit returns");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Go `TestSIWCFileMutationRejectsLateExchange`: the exchange has returned and its
    /// commit waits on `disk` while a v8 mutation of the file runs there. The commit
    /// then finds the binding failed and writes nothing: a reupload keeps its bytes,
    /// and a deleted file stays gone. The deletes run the route's own code under the
    /// lock; the upload does what `store_file` does there.
    ///
    /// Single-threaded, as in the test above: once the preparation lock is taken, the
    /// commit is running on the blocking pool, paused on `disk`.
    #[tokio::test]
    async fn a_late_exchange_lands_nothing_after_a_v8_mutation() {
        let app = axum::Router::new().route(
            "/api/accounts/oauth/token",
            axum::routing::post(|| async {
                r#"{"access_token":"siwc-at","refresh_token":"siwc-rt","id_token":"siwc-id","scope":"openid chatgpt.tokens.use.direct","expires_in":3600}"#
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        for (operation, query, body) in [
            ("upload", "", ""),
            ("delete", "name=codex.json", ""),
            ("delete-all", "all=true", ""),
            ("delete-batch", "", r#"["codex.json","other.json"]"#),
        ] {
            let (dir, file, state) = fixture(Some(base.clone()));
            std::fs::write(dir.join("other.json"), codex_file("other")).unwrap();
            let sid = started_state(
                bind(
                    State(state.clone()),
                    FirstJson(axum::body::Bytes::from(r#"{"name":"codex.json"}"#)),
                )
                .await,
            )
            .await;
            let reupload = codex_file("reuploaded");
            let (held_tx, held) = std::sync::mpsc::channel();
            let (go, go_rx) = std::sync::mpsc::channel::<()>();
            let mutation = std::thread::spawn({
                let (state, file, reupload) = (state.clone(), file.clone(), reupload.clone());
                move || {
                    let _disk = state.disk.lock().unwrap();
                    held_tx.send(()).unwrap();
                    go_rx.recv().unwrap();
                    if operation == "upload" {
                        state.oauth.invalidate_siwc(&file);
                        std::fs::write(&file, reupload).unwrap();
                    } else {
                        let q = Query::parse(Some(query.to_owned()));
                        let response = super::super::auth_files::delete_sync(&state, &q, body.as_bytes(), true);
                        assert_eq!(response.status(), StatusCode::OK, "{operation}");
                    }
                }
            });
            held.recv().unwrap();
            let req = CallbackRequest {
                state: sid.clone(),
                code: "c".into(),
                client_id: "oaiapp_issued".into(),
                ..Default::default()
            };
            assert_eq!(callback(&state, &req, None).unwrap().status(), StatusCode::OK);
            let lock = state.rt.store().prepare_lock("codex.json");
            tokio::time::timeout(Duration::from_secs(5), async {
                while lock.try_lock().is_ok() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the commit waits on disk");
            go.send(()).unwrap();
            mutation.join().unwrap();
            tokio::time::timeout(Duration::from_secs(5), lock.lock_owned())
                .await
                .expect("the commit returns");
            let (_, status, completed) = state.oauth.details(&sid).unwrap();
            assert_eq!(
                (status.as_str(), completed),
                ("SIWC binding target changed; start a new binding", false),
                "{operation}"
            );
            if operation == "upload" {
                assert_eq!(std::fs::read_to_string(&file).unwrap(), reupload, "{operation}");
            } else {
                assert!(!file.exists(), "{operation}: the deleted file came back");
            }
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    /// Go `TestSIWCMutationPathAliases`, plus what Go's `os.SameFile` adds: a hard link
    /// is the same file, and a file that is gone matches through its folder's real path.
    #[test]
    fn mutation_paths_match_every_alias_of_the_file() {
        let dir = std::env::temp_dir().join(format!("cpa-siwc-alias-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("real")).unwrap();
        let path = dir.join("real").join("auth.json");
        std::fs::write(&path, "{}").unwrap();
        std::os::unix::fs::symlink(&path, dir.join("real").join("alias.json")).unwrap();
        std::fs::hard_link(&path, dir.join("real").join("hard.json")).unwrap();
        std::fs::write(dir.join("real").join("other.json"), "{}").unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("linked")).unwrap();
        let at = |rel: &str| dir.join(rel);
        assert!(same_path(&at("real/alias.json"), &path));
        assert!(same_path(&at("real/hard.json"), &path));
        assert!(same_path(&at("linked/auth.json"), &path));
        assert!(same_path(&at("linked/gone.json"), &at("real/gone.json")));
        assert!(!same_path(&path, &at("real/other.json")));
        assert!(!same_path(&path, &at("real/gone.json")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// With the file and its folder both gone, the binding's canonical path (saved at
    /// start) and a mutation path compare as Go's `Abs` and `Clean` spell them.
    #[test]
    fn a_gone_folder_compares_cleaned_paths() {
        let root = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("cpa-siwc-gone-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("auth")).unwrap();
        std::fs::write(root.join("auth/codex.json"), "{}").unwrap();
        let bound = std::fs::canonicalize(root.join("auth/codex.json")).unwrap();
        std::fs::remove_dir_all(root.join("auth")).unwrap();
        assert!(same_path(&root.join("auth/../auth/codex.json"), &bound));
        assert!(same_path(&root.join("auth/./codex.json"), &bound));
        assert!(!same_path(&root.join("auth/../codex.json"), &bound));
        assert!(!same_path(&root.join("other/../auth/other.json"), &bound));
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Go `BindCodexSIWC` reads, merges and writes the file under its credential lock:
    /// a commit waits while another process's refresh holds it, then binds onto the
    /// file that refresh wrote, so the rotated outer refresh token is kept.
    #[tokio::test]
    async fn a_binding_waits_for_the_credential_lock_and_keeps_a_rotated_token() {
        let app = axum::Router::new().route(
            "/api/accounts/oauth/token",
            axum::routing::post(|| async {
                r#"{"access_token":"siwc-at","refresh_token":"siwc-rt","id_token":"siwc-id","scope":"openid chatgpt.tokens.use.direct","expires_in":3600}"#
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (dir, file, state) = fixture(Some(base));
        let response = bind(
            State(state.clone()),
            FirstJson(axum::body::Bytes::from(r#"{"name":"codex.json"}"#)),
        )
        .await;
        let sid = started_state(response).await;
        let held = cpa_exec::codex_siwc_bind::lock_file(&file).unwrap();
        let req = CallbackRequest {
            state: sid,
            code: "c".into(),
            client_id: "oaiapp_issued".into(),
            ..Default::default()
        };
        assert_eq!(callback(&state, &req, None).unwrap().status(), StatusCode::OK);
        crate::runtime::tests::lock_waited(&file).await;
        // The other process's refresh spends "r" and writes "r2" under the lock.
        let rotated = codex_file("old").replace(r#""refresh_token":"r""#, r#""refresh_token":"r2""#);
        std::fs::write(&file, rotated).unwrap();
        drop(held);
        let bound = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let disk: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
                if disk.get("siwc").is_some() {
                    return disk;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the binding lands once the lock is free");
        assert_eq!(
            (&bound["refresh_token"], &bound["siwc"]["refresh_token"]),
            (&Value::from("r2"), &Value::from("siwc-rt"))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn codex_file(account: &str) -> String {
        format!(r#"{{"type":"codex","account_id":"{account}","access_token":"a","refresh_token":"r"}}"#)
    }

    async fn started_state(response: Response) -> String {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice::<Value>(&body).unwrap()["state"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// A private auth dir holding `codex.json` for account "old".
    fn fixture(login_base: Option<String>) -> (PathBuf, PathBuf, Arc<Management>) {
        fixture_with(crate::management::Options {
            login_base,
            ..Default::default()
        })
    }

    /// [`fixture`] with management `options`.
    fn fixture_with(options: crate::management::Options) -> (PathBuf, PathBuf, Arc<Management>) {
        let dir = std::env::temp_dir().join(format!("cpa-siwc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("codex.json");
        std::fs::write(&file, codex_file("old")).unwrap();
        let path = dir.join("config.yaml");
        std::fs::write(&path, format!("oauth: {{auth-dir: {}}}\n", dir.display())).unwrap();
        let cfg = cpa_core::config::Config::load(&path).unwrap();
        let rt = Arc::new(crate::testing::runtime(
            cfg.clone(),
            cpa_core::config::credentials::from_auth_dir(&cfg),
            cpa_exec::Executors {
                claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:9").unwrap(),
                codex: Default::default(),
                meta: Default::default(),
                xai: Default::default(),
            },
        ));
        (dir, file, Management::with_options(rt, path, options))
    }
}
