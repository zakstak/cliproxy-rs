//! Credentials dispatched by a control plane instead of the local scheduler (Go Home
//! mode: `pickHomeDispatchSelection` / `executeHomeOnce`). While a dispatcher is
//! installed every request routes to provider `home`, each pick asks the dispatcher for
//! one credential, and the credential's lease ends with a release the dispatcher
//! reports back. The dispatcher owns cooldowns and retry limits; nothing is recorded in
//! the local scheduler.

use cpa_core::gostr::GoStr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::response::IntoResponse;
use cpa_core::credential::Credential;
use cpa_core::exec::ExecError;
use futures_util::future::BoxFuture;

/// Attribute carrying the upstream model the dispatcher chose (Go
/// `homeUpstreamModelAttributeKey`).
pub const UPSTREAM_MODEL: &str = "home_upstream_model";
/// Set to `true` when responses report the route model instead (Go
/// `homeForceMappingAttributeKey`).
pub const FORCE_MAPPING: &str = "home_force_mapping";
/// The alias Home mapped from (Go `homeOriginalAliasAttributeKey`).
pub const ORIGINAL_ALIAS: &str = "home_original_alias";
/// The dispatched auth's own `provider` field (Go `Auth.Provider`; the selection
/// provider is its lower-case form). The credential's provider is the executor key.
pub const PROVIDER: &str = cpa_core::config::credentials::HOME_PROVIDER;
/// Home's definition of the dispatched model, as sent (Go `homeDispatchModelInfo`):
/// [`crate::capabilities::bind_home`] makes it the attempt's resolved model.
pub const MODEL_INFO: &str = "home_model_info";
/// The session the dispatcher sent for this pick (Go
/// `HomeDispatchSelection.CanonicalSessionID`); usage records report it.
pub const SESSION: &str = "home_session_id";
/// Its parent (Go `HomeDispatchSelection.ParentSessionID`).
pub const PARENT_SESSION: &str = "home_parent_session_id";
/// What a Home pick carries that Go keeps outside `Auth.Attributes`: the auth's
/// runtime state, and the attribute names the dispatcher set itself (plugins never see
/// those; attributes Home sent stay).
#[derive(Debug, Default, Clone)]
pub struct Private {
    pub availability: Arc<Availability>,
    pub attributes: Vec<String>,
}

/// The runtime state Go keeps on a dispatched `coreauth.Auth` (`status`, `unavailable`,
/// `next_retry_after`, `model_states`) and the Responses WebSocket pin check reads,
/// decoded as Go's typed JSON decoding does.
#[derive(Debug, Default, PartialEq)]
pub struct Availability {
    state: State,
    /// A `None` entry is Go's nil state (`null`): present, but never consulted.
    model_states: std::collections::BTreeMap<String, Option<State>>,
}

#[derive(Debug, Default, PartialEq)]
struct State {
    status: String,
    unavailable: bool,
    /// Unix seconds and nanoseconds; `None` is Go's zero time.
    retry: Option<(i64, u32)>,
}

/// An object's members as written. ponytail: a repeated key keeps its last value, where
/// Go applies each in turn and skips a `null`; Go also matches names case-insensitively.
type RawObject<'a> = std::collections::HashMap<String, &'a serde_json::value::RawValue>;

impl State {
    fn decode(object: &RawObject<'_>) -> Result<Self, ()> {
        use serde_json::Value;
        let field = |key: &str| object.get(key).map(|raw| raw.get()).filter(|raw| *raw != "null");
        let value = |raw: &str| serde_json::from_str::<Value>(raw).map_err(|_| ());
        Ok(Self {
            status: match field("status").map(value).transpose()? {
                None => String::new(),
                Some(Value::String(s)) => s,
                Some(_) => return Err(()),
            },
            unavailable: match field("unavailable").map(value).transpose()? {
                None => false,
                Some(v) => v.as_bool().ok_or(())?,
            },
            // Go's `Time.UnmarshalJSON` parses the quoted bytes as written, escapes and
            // all: a backslash never parses.
            retry: match field("next_retry_after") {
                None => None,
                Some(raw) => {
                    let quoted = raw.strip_prefix('"').and_then(|r| r.strip_suffix('"')).ok_or(())?;
                    go_time(quoted).ok_or(())?
                }
            },
        })
    }

    fn disabled(&self) -> bool {
        self.status == "disabled"
    }

    /// `Unavailable && !NextRetryAfter.IsZero() && NextRetryAfter.After(now)`.
    fn cooling(&self, now: (i64, u32)) -> bool {
        self.unavailable && self.retry.is_some_and(|at| at > now)
    }
}

impl Availability {
    /// Go's `json.Unmarshal` of an auth object (as written) into `coreauth.Auth`, for
    /// these fields: `null` or absent keeps the default, any other mistyped value or
    /// unparsable time fails the whole reply (Home's `invalid_auth`, with this message).
    pub fn decode(auth: &str) -> Result<Self, &'static str> {
        Self::decode_fields(auth).map_err(|()| "home returned invalid auth payload")
    }

    fn decode_fields(auth: &str) -> Result<Self, ()> {
        let object: RawObject<'_> = serde_json::from_str(auth).map_err(|_| ())?;
        let state = State::decode(&object)?;
        let model_states = match object.get("model_states").map(|raw| raw.get()) {
            None | Some("null") => Default::default(),
            Some(raw) => serde_json::from_str::<RawObject<'_>>(raw)
                .map_err(|_| ())?
                .into_iter()
                .map(|(model, state)| match state.get() {
                    "null" => Ok((model, None)),
                    raw => {
                        let state: RawObject<'_> = serde_json::from_str(raw).map_err(|_| ())?;
                        Ok((model, Some(State::decode(&state)?)))
                    }
                })
                .collect::<Result<_, ()>>()?,
        };
        Ok(Self { state, model_states })
    }
}

/// Go's `time.Time` JSON decoding (`parseStrictRFC3339`, whose strict checks are off,
/// so `time.Parse(time.RFC3339, ...)`): a one- or two-digit hour, a fraction after `.`
/// or `,` (nanoseconds, the rest ignored), `Z` or a `±hh:mm` offset up to 24:60.
/// `Some(None)` is Go's zero time (`IsZero`), `None` a value Go rejects.
fn go_time(raw: &str) -> Option<Option<(i64, u32)>> {
    let b = raw.as_bytes();
    let digits = |from: usize, to: usize| -> Option<u32> {
        let part = b.get(from..to)?;
        part.iter()
            .all(u8::is_ascii_digit)
            .then(|| part.iter().fold(0, |n, d| n * 10 + u32::from(d - b'0')))
    };
    let at = |i: usize, c: u8| b.get(i) == Some(&c);
    // Go's `stdLongYear` wants a digit first, so no sign.
    let year = digits(0, 4)? as i32;
    let (month, day) = (digits(5, 7)?, digits(8, 10)?);
    if !(at(4, b'-') && at(7, b'-') && at(10, b'T')) {
        return None;
    }
    let mut i = if b.get(12).is_some_and(u8::is_ascii_digit) {
        13
    } else {
        12
    };
    let hour = digits(11, i)?;
    if !(at(i, b':') && at(i + 3, b':')) {
        return None;
    }
    let (minute, second) = (digits(i + 1, i + 3)?, digits(i + 4, i + 6)?);
    i += 6;
    let mut nanos = 0;
    if matches!(b.get(i), Some(b'.' | b',')) && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
        let start = i + 1;
        i = start;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        let fraction = &b[start..i];
        for k in 0..9 {
            nanos = nanos * 10 + fraction.get(k).map_or(0, |d| u32::from(d - b'0'));
        }
    }
    let offset = match b.get(i)? {
        b'Z' => {
            i += 1;
            0
        }
        sign @ (b'+' | b'-') => {
            let (hh, mm) = (digits(i + 1, i + 3)?, digits(i + 4, i + 6)?);
            if !at(i + 3, b':') || hh > 24 || mm > 60 {
                return None;
            }
            i += 6;
            let offset = i64::from((hh * 60 + mm) * 60);
            if *sign == b'-' { -offset } else { offset }
        }
        _ => return None,
    };
    if i != b.len() || !(1..=12).contains(&month) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let local = chrono::NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(hour, minute, second)?;
    let seconds = local.and_utc().timestamp() - offset;
    // Go's zero time: January 1, year 1, 00:00:00 UTC.
    const ZERO: i64 = -62_135_596_800;
    Some((seconds != ZERO || nanos != 0).then_some((seconds, nanos)))
}

/// Go `responsesWebsocketAuthAvailableForModel` on a dispatched auth's snapshot:
/// enabled (`disabled` and `status`), then the model's state when the snapshot has one
/// (its status and cooldown decide), else the auth-wide cooldown. A cooldown counts only
/// while its retry time is set and in the future.
pub fn available_for_model(
    credential: &Credential,
    snapshot: &Availability,
    model: &str,
    now: std::time::SystemTime,
) -> bool {
    if credential.disabled {
        return false;
    }
    let now = match now.duration_since(std::time::UNIX_EPOCH) {
        Ok(since) => (since.as_secs() as i64, since.subsec_nanos()),
        Err(_) => (0, 0),
    };
    if snapshot.state.disabled() {
        return false;
    }
    if !model.is_empty() && !snapshot.model_states.is_empty() {
        let base = cpa_common::thinking::parse_suffix(model).model_name;
        let base = base.trim();
        let state = match snapshot.model_states.get(model) {
            Some(Some(state)) => Some(state),
            _ if !base.is_empty() && base != model => snapshot.model_states.get(base).and_then(Option::as_ref),
            _ => None,
        };
        if let Some(state) = state {
            return !state.disabled() && !state.cooling(now);
        }
    }
    !snapshot.state.cooling(now)
}

/// Go `HomeDispatchSelection.Provider`: the dispatched auth's provider, lower-cased.
pub fn selection_provider(credential: &Credential) -> String {
    credential
        .attributes
        .get(PROVIDER)
        .map_or(credential.provider.as_str(), String::as_str)
        .trim()
        .go_lower()
}

/// One credential request (Go's RPOP request fields).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteRequest {
    /// The route model as the client sent it (Home mode resolves nothing locally).
    pub model: String,
    pub session_id: String,
    pub parent_session_id: String,
    /// Downstream request headers; the dispatcher authenticates the client with them.
    pub headers: Vec<(String, String)>,
    /// 1 for the first pick of a retry round, then 2, 3, ... (Go `homeAuthCount`).
    pub count: i64,
    pub retry_round: i64,
    /// Credentials already tried this round.
    pub excluded: Vec<String>,
    pub pinned: String,
    pub request_id: String,
    /// `http`, `stream` or `websocket`.
    pub kind: &'static str,
    /// Go `credentialPolicyFromContext`: narrows Home's choice (`codex_alpha_search_v1`),
    /// empty for inference.
    pub credential_policy: String,
}

/// Completes once the dispatcher acknowledged a release (bounded by the dispatcher).
pub type ReleaseWait = BoxFuture<'static, Result<(), String>>;

/// Ends a dispatched credential's lease exactly once; returns the acknowledgement to
/// wait for before the next pick of the same request, when there is one.
pub type EndLease = Box<dyn FnOnce() -> Option<ReleaseWait> + Send>;

/// Set to `true` when the dispatcher cancels the execution (Go: a draining registry
/// closes the selection's bound resources, cancelling its attempt contexts).
pub type CancelSignal = tokio::sync::watch::Receiver<bool>;

/// Resolves once `signal` is set; never when its sender is gone without setting it.
pub(crate) fn cancelled(mut signal: CancelSignal) -> BoxFuture<'static, ()> {
    Box::pin(async move {
        if signal.wait_for(|cancelled| *cancelled).await.is_err() {
            std::future::pending::<()>().await;
        }
    })
}

/// The error an execution cancelled by the dispatcher ends with (Go `context.Canceled`).
pub(crate) fn cancelled_error() -> ExecError {
    ExecError::local(502, cpa_core::exec::FailureScope::Transport, "context canceled")
}

/// Go `clienterror.StatusClientClosedRequest`: what `HTTPStatusFromErrorOr` makes of a
/// cancelled context, so of a request Home's drain stopped.
pub(crate) const CLIENT_CLOSED: u16 = 499;

/// The error Go's `http.Client` returns for a request its cancelled context stopped
/// (`*url.Error`): `Post "<url>": context canceled`.
pub(crate) fn cancelled_request(method: &str, url: &str) -> String {
    let mut op = method.to_ascii_lowercase();
    if let Some(first) = op.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    format!("{op} {}: context canceled", cpa_common::gostr::quote(url))
}

/// A dispatched credential and the end of its lease.
pub struct RemoteGrant {
    pub credential: Credential,
    pub end: EndLease,
    /// Cancels the execution running on this credential.
    pub cancel: Option<CancelSignal>,
    /// The request-retry limit Home set for this request (Go `selection.requestRetry`).
    pub request_retry: Option<i64>,
    /// The client key Home authenticated (Go `dispatch.UserAPIKey`), trimmed; empty
    /// when Home sent none. Private request context, like `Caller::principal`.
    pub user_api_key: String,
    pub private: Private,
}

/// How a failed pick takes part in retries (Go's typed Home errors).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteErrorKind {
    Plain,
    /// Go `homeDispatchRetryAfterError` (`model_cooldown`).
    Cooldown {
        retry_after: Option<Duration>,
        request_retry: Option<i64>,
    },
    /// Go `HomeConcurrencyBusyError`: never retried; `header` is its safe `Retry-After`.
    Busy {
        header: Option<u64>,
    },
}

/// A failed pick: the client-facing error (status and Go's `code: message`), its code
/// and its retry behaviour.
#[derive(Debug, Clone)]
pub struct RemoteError {
    /// Boxed, like `code`, to keep the error small (it travels in `Result`s).
    pub error: Box<ExecError>,
    pub code: Box<str>,
    pub kind: RemoteErrorKind,
}

impl RemoteError {
    pub fn plain(error: ExecError, code: &str) -> Self {
        Self {
            error: Box::new(error),
            code: code.into(),
            kind: RemoteErrorKind::Plain,
        }
    }
}

/// Why the control plane's model catalog is not available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelsError {
    /// No live control-plane client (Go `home.Current() == nil`).
    Unavailable,
    /// The request failed; the text is Go's error.
    Failed(String),
}

pub trait RemoteDispatch: Send + Sync + 'static {
    /// Go `HeartbeatOK`: requests are refused while the control plane is unreachable.
    fn available(&self) -> bool;
    /// One pick.
    fn dispatch(&self, request: RemoteRequest) -> BoxFuture<'_, Result<RemoteGrant, RemoteError>>;
    /// Go `GetModels`: the catalog for the client these request headers and query
    /// parameters authenticate (raw Home payload).
    fn models(
        &self,
        headers: Vec<(String, String)>,
        query: Vec<(String, String)>,
    ) -> BoxFuture<'_, Result<Vec<u8>, ModelsError>>;

    /// Go RPushRequestLog. Unavailable Home lifetimes discard the log, never
    /// fall back to writing it locally. Test/internal dispatchers may ignore it.
    fn request_log(&self, payload: Vec<u8>) -> BoxFuture<'_, Result<(), String>> {
        let _ = payload;
        Box::pin(async { Ok(()) })
    }
}

/// Releases ended during one request, awaited before its next pick (Go
/// `endHomeSelectionBeforeRedispatch`).
#[derive(Clone, Default)]
pub struct PendingReleases(Arc<Mutex<Vec<ReleaseWait>>>);

impl PendingReleases {
    pub(crate) fn push(&self, wait: ReleaseWait) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).push(wait);
    }

    /// Waits for every release ended so far; the first failure is returned.
    pub(crate) async fn settle(&self) -> Result<(), String> {
        let waits = std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner));
        for wait in waits {
            wait.await?;
        }
        Ok(())
    }
}

/// A Home selection a handler could not make, as Go's handlers write it:
/// `HTTPStatusFromErrorOr(err, 503)`, `err.Error()` and the safe `Retry-After` seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub status: u16,
    pub text: String,
    pub retry_after: Option<u64>,
}

impl Refusal {
    fn not_found(message: &str) -> Self {
        Self {
            status: 503,
            text: format!("auth_not_found: {message}"),
            retry_after: None,
        }
    }
}

/// Go `SelectHomeAuthByKind` and `SelectHomeAuthWithCredentialPolicy`: one Home pick for
/// a handler outside the dispatch loop. A pick `accept` refuses is released, and the
/// release acknowledged, before the next pick, which excludes it; refusing the same
/// credential twice ends the selection. Also returns the client key Home authenticated
/// (Go `setHomeUserAPIKeyOnGinContext`: the last non-empty one of these picks).
pub(crate) async fn select(
    rt: &crate::Runtime,
    remote: &dyn RemoteDispatch,
    selection: crate::runtime::Selection,
    request: RemoteRequest,
    accept: impl Fn(&Credential) -> bool,
) -> Result<(crate::runtime::Lease, String), Refusal> {
    let releases = PendingReleases::default();
    let mut tried: Vec<String> = Vec::new();
    let mut home_key = String::new();
    let mut count = request.count.max(1);
    loop {
        let pick = RemoteRequest {
            count,
            excluded: tried.clone(),
            ..request.clone()
        };
        let (lease, _, user_key) = rt
            .acquire_remote(remote, selection.clone(), pick, &releases)
            .await
            .map_err(|failed| Refusal {
                status: failed.error.status,
                text: String::from_utf8_lossy(&failed.error.body).into_owned(),
                retry_after: match failed.kind {
                    RemoteErrorKind::Cooldown { retry_after, .. } => {
                        retry_after.map(|d| d.as_secs() + u64::from(d.subsec_nanos() > 0))
                    }
                    RemoteErrorKind::Busy { header } => header,
                    RemoteErrorKind::Plain => None,
                },
            })?;
        if !user_key.trim().is_empty() {
            home_key = user_key.trim().to_owned();
        }
        let credential = lease.credential.clone();
        if accept(&credential) {
            return Ok((lease, home_key));
        }
        // Go `endHomeSelectionBeforeRedispatch`.
        drop(lease);
        if let Err(message) = releases.settle().await {
            return Err(Refusal {
                status: 503,
                text: format!("home_unavailable: Home did not acknowledge credential release: {message}"),
                retry_after: None,
            });
        }
        let id = credential.id.trim().to_owned();
        if id.is_empty() {
            return Err(Refusal::not_found("selected auth has no ID"));
        }
        if tried.contains(&id) {
            return Err(Refusal::not_found("selector repeatedly returned an ineligible auth"));
        }
        tried.push(id);
        count += 1;
    }
}

/// The lease side of a dispatched credential: ends it on the first report.
pub(crate) struct RemoteEnd {
    pub end: Option<EndLease>,
    pub releases: PendingReleases,
    pub cancel: Option<CancelSignal>,
    pub private: Private,
}

impl RemoteEnd {
    pub(crate) fn finish(&mut self) {
        if let Some(end) = self.end.take()
            && let Some(wait) = end()
        {
            self.releases.push(wait);
        }
    }
}

tokio::task_local! {
    /// The `?key=` (else `?auth_token=`) value of the request being served (Go
    /// `homeQueryCredentialFromContext` reads gin's query).
    static QUERY_CREDENTIAL: String;
}

/// `fut` with this request's query credential, for dispatch work that outlives the
/// gate (a keep-alive moves the unfinished call into the response body).
pub(crate) fn keep_query_credential<F: std::future::Future>(fut: F) -> impl std::future::Future<Output = F::Output> {
    let credential = QUERY_CREDENTIAL.try_with(Clone::clone).unwrap_or_default();
    QUERY_CREDENTIAL.scope(credential, fut)
}

/// Go `homeDispatchHeaders`: the request headers Home authenticates the client with;
/// a query credential is added as X-Goog-Api-Key when no credential header was sent:
/// the request's `?key=`/`?auth_token=`, else the query key `caller` authenticated with
/// (Go's `accessMetadata` branch; it also serves WebSocket sessions, which run after
/// their upgrade request).
pub(crate) fn home_headers(
    headers: &axum::http::HeaderMap,
    caller: Option<&cpa_core::exec::Caller>,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    let mut credential = QUERY_CREDENTIAL.try_with(Clone::clone).unwrap_or_default();
    if credential.is_empty()
        && let Some(caller) = caller.filter(|c| matches!(c.source, "query-key" | "query-auth-token"))
    {
        credential = caller.principal.trim().to_owned();
    }
    let has_header = ["authorization", "x-goog-api-key", "x-api-key"]
        .iter()
        .any(|name| headers.get(*name).is_some_and(|v| !v.is_empty()));
    if !credential.is_empty() && !has_header {
        out.push(("x-goog-api-key".to_owned(), credential));
    }
    out
}

/// The query credential Go reads for Home: `key`, else `auth_token`, trimmed.
fn query_credential(query: &str) -> String {
    ["key", "auth_token"]
        .iter()
        .map(|name| {
            crate::access::query_get(query, name)
                .map(|v| String::from_utf8_lossy(&v).trim().to_owned())
                .unwrap_or_default()
        })
        .find(|v| !v.is_empty())
        .unwrap_or_default()
}

/// Go `homeHeartbeatMiddleware`: while a dispatcher is installed and its control plane
/// is unreachable, every route but management answers 503 with an empty body.
/// Inside, the request's query credential is available to `home_headers`.
pub async fn gate(
    axum::extract::State(rt): axum::extract::State<Arc<crate::Runtime>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let path = request.uri().path();
    let exempt = ["/v0/management", "/v8/management"]
        .iter()
        .any(|base| path == *base || path.strip_prefix(base).is_some_and(|rest| rest.starts_with('/')))
        || path == "/management.html";
    if !exempt
        && let Some(remote) = rt.remote_dispatch()
        && !remote.available()
    {
        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let credential = query_credential(request.uri().query().unwrap_or_default());
    QUERY_CREDENTIAL.scope(credential, next.run(request)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Go `time.Time` JSON decoding, expected values from Go 1.27: a comma fraction and
    /// a 24:60 offset are accepted, extra digits are cut at nanoseconds, a one-digit
    /// hour is accepted, and Go's zero time (whatever the offset) is `IsZero`.
    #[test]
    fn home_times_decode_as_go_does() {
        const Y2999: i64 = 32_472_144_000;
        for (raw, want) in [
            ("2999-01-01T00:00:00Z", Some(Some((Y2999, 0)))),
            ("2999-01-01T00:00:00,1Z", Some(Some((Y2999, 100_000_000)))),
            ("2999-01-01T00:00:00.1234567891234Z", Some(Some((Y2999, 123_456_789)))),
            ("2999-01-01T00:00:00+24:60", Some(Some((Y2999 - 25 * 3600, 0)))),
            ("2999-01-01T00:00:00-00:00", Some(Some((Y2999, 0)))),
            ("2000-01-01T0:00:00Z", Some(Some((946_684_800, 0)))),
            ("0001-01-01T00:00:00Z", Some(None)),
            ("0001-01-01T01:00:00+01:00", Some(None)),
            ("2999-01-01T00:00:00+25:00", None),
            ("2999-01-01T00:00:00+00:61", None),
            ("2999-01-01T00:00:00z", None),
            ("2999-01-01t00:00:00Z", None),
            ("2999-01-01T00:00:60Z", None),
            ("2999-01-01T24:00:00Z", None),
            ("2999-02-30T00:00:00Z", None),
            ("2999-01-01T00:00:00,Z", None),
            ("2999-01-01T00:00:00+0100", None),
            ("2999-01-01T00:00:00Z ", None),
            ("+001-01-01T00:00:00Z", None),
            ("-001-01-01T00:00:00Z", None),
            ("2999-01-01T00:00:00", None),
            ("", None),
        ] {
            assert_eq!(go_time(raw), want, "{raw}");
        }
    }

    /// Go decodes the auth's runtime state into typed fields: `null` keeps a default,
    /// any other mistyped value fails the reply.
    #[test]
    fn home_availability_rejects_what_go_cannot_decode() {
        let decode = Availability::decode;
        for valid in [
            r#"{}"#,
            r#"{"status":null,"unavailable":null,"next_retry_after":null,"model_states":null}"#,
            r#"{"model_states":{"m":null,"n":{"status":"disabled"}}}"#,
        ] {
            assert!(decode(valid).is_ok(), "{valid}");
        }
        for invalid in [
            r#"{"status":1}"#,
            r#"{"unavailable":"true"}"#,
            r#"{"next_retry_after":""}"#,
            r#"{"next_retry_after":0}"#,
            r#"{"model_states":[]}"#,
            r#"{"model_states":{"m":1}}"#,
            r#"{"model_states":{"m":{"next_retry_after":""}}}"#,
            r#"{"model_states":{"m":{"unavailable":1}}}"#,
            // Go parses the quoted bytes as written: an escaped `Z` is not `Z`.
            r#"{"next_retry_after":"2999-01-01T00:00:00\u005a"}"#,
            r#"{"model_states":{"m":{"next_retry_after":"2999-01-01T00:00:00\u005a"}}}"#,
        ] {
            assert!(decode(invalid).is_err(), "{invalid}");
        }
    }

    /// `responsesWebsocketAuthAvailableForModel` on decoded snapshots, at 2026-01-01.
    #[test]
    fn home_availability_follows_go() {
        let now = std::time::UNIX_EPOCH + Duration::from_secs(1_767_225_600);
        let available = |json: &str, model: &str| {
            let snapshot = Availability::decode(json).unwrap();
            available_for_model(&Credential::relay_session("home-a"), &snapshot, model, now)
        };
        let future = "2999-01-01T00:00:00,1Z";
        for (json, model, want) in [
            (r#"{}"#.to_owned(), "m", true),
            (r#"{"status":"disabled"}"#.into(), "m", false),
            (
                format!(r#"{{"unavailable":true,"next_retry_after":"{future}"}}"#),
                "m",
                false,
            ),
            (
                r#"{"unavailable":true,"next_retry_after":"2001-01-01T00:00:00Z"}"#.into(),
                "m",
                true,
            ),
            (
                r#"{"unavailable":true,"next_retry_after":"0001-01-01T00:00:00Z"}"#.into(),
                "m",
                true,
            ),
            (r#"{"unavailable":true}"#.into(), "m", true),
            (
                format!(r#"{{"model_states":{{"m":{{"unavailable":true,"next_retry_after":"{future}"}}}}}}"#),
                "m",
                false,
            ),
            // The suffix-free base model's state, and the model's state over the auth's.
            (
                r#"{"model_states":{"m":{"status":"disabled"}}}"#.into(),
                "m(high)",
                false,
            ),
            (
                format!(r#"{{"unavailable":true,"next_retry_after":"{future}","model_states":{{"m":{{}}}}}}"#),
                "m",
                true,
            ),
            // A nil state is never consulted: the auth-wide cooldown decides.
            (
                format!(r#"{{"unavailable":true,"next_retry_after":"{future}","model_states":{{"m":null}}}}"#),
                "m",
                false,
            ),
            (r#"{"model_states":{"other":{"status":"disabled"}}}"#.into(), "m", true),
        ] {
            assert_eq!(available(&json, model), want, "{json} {model}");
        }
    }

    /// Go `homeDispatchHeaders` and `homeQueryCredentialFromContext`: `?key=` (else
    /// `?auth_token=`) reaches Home as X-Goog-Api-Key unless a credential header was
    /// sent; outside a request nothing is added. Covers Go
    /// `TestHomeDispatchHeadersAddsQueryKeyCredential`,
    /// `TestHomeDispatchHeadersAddsQueryCredentialFromAccessMetadata` (the query-key
    /// caller), `TestHomeDispatchHeadersKeepsExistingCredentialHeader` and
    /// `TestHomeDispatchHeadersIgnoresHeaderCredentialSource` (the header caller).
    #[tokio::test]
    async fn query_credentials_reach_home_as_x_goog_api_key() {
        let headers = |pairs: &[(&str, &str)]| {
            let mut map = axum::http::HeaderMap::new();
            for (name, value) in pairs {
                map.insert(
                    axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    value.parse().unwrap(),
                );
            }
            map
        };
        let goog = |query: &str, pairs: &[(&str, &str)]| {
            let map = headers(pairs);
            let credential = query_credential(query);
            async move {
                QUERY_CREDENTIAL
                    .scope(credential, async {
                        home_headers(&map, None)
                            .into_iter()
                            .filter(|(name, _)| name == "x-goog-api-key")
                            .map(|(_, value)| value)
                            .collect::<Vec<_>>()
                    })
                    .await
            }
        };
        assert_eq!(goog("key=k1&alt=sse", &[]).await, ["k1"]);
        assert_eq!(goog("auth_token=t1", &[("user-agent", "x")]).await, ["t1"]);
        assert_eq!(goog("auth_token=t1&key=%20k2%20", &[]).await, ["k2"]);
        assert!(goog("key=k1", &[("authorization", "Bearer other")]).await.is_empty());
        assert_eq!(
            goog("key=k1", &[("x-goog-api-key", "header-key")]).await,
            ["header-key"]
        );
        assert!(goog("key=k1", &[("x-api-key", "k")]).await.is_empty());
        assert!(goog("alt=sse", &[]).await.is_empty());
        assert!(home_headers(&headers(&[]), None).is_empty());
        // Outside a request, a query-key caller still identifies itself.
        let caller = cpa_core::exec::Caller {
            principal: "local-key".into(),
            source: "query-key",
            provider: String::new(),
        };
        let outside: Vec<_> = home_headers(&headers(&[]), Some(&caller));
        assert_eq!(outside, [("x-goog-api-key".to_owned(), "local-key".to_owned())]);
        let header_caller = cpa_core::exec::Caller {
            principal: "local-key".into(),
            source: "authorization",
            provider: String::new(),
        };
        assert!(home_headers(&headers(&[]), Some(&header_caller)).is_empty());
    }
}
