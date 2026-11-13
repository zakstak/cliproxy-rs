//! Codex token refresh: which token partitions refresh (Go codex_executor_auth.go
//! `Refresh` / `refreshLocked`), and the partition-scoped result that lands on the
//! credential file as read under its lock (internal/auth/codex/refresh_patch.go).

use std::collections::BTreeMap;

use chrono::Utc;
use cpa_common::json::GoValue;
use cpa_core::credential::{Credential, MetadataPatch};
use cpa_core::exec::ExecError;
use serde_json::{Map, Value};

use crate::codex_oauth::{self, CodexOAuth, GrantError};

/// The refresh tokens of one credential that a token endpoint refused for good (a
/// terminal [`GrantError`]), by partition, as SHA-256 hashes. A refused token is never
/// sent again: the partition counts as having no refresh credential until its refresh
/// token changes (a new sign-in, an upload, another process's rotation). The outer and
/// Sign in with ChatGPT partitions are separate token sets (OpenAI "Accounts and
/// sessions": each registration's credentials stay separate), so each is refused alone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Refused {
    pub outer: Option<[u8; 32]>,
    pub siwc: Option<[u8; 32]>,
}

fn token_hash(token: &str) -> Option<[u8; 32]> {
    use sha2::Digest as _;
    let token = token.trim();
    (!token.is_empty()).then(|| sha2::Sha256::digest(token.as_bytes()).into())
}

impl Refused {
    pub fn is_empty(&self) -> bool {
        self.outer.is_none() && self.siwc.is_none()
    }

    /// Whether `credential`'s outer refresh token is the refused one.
    pub fn outer_refused(&self, credential: &Credential) -> bool {
        self.outer.is_some() && self.outer == token_hash(codex_oauth::refresh_token(credential))
    }

    /// Whether `credential`'s Sign in with ChatGPT refresh token is the refused one.
    pub fn siwc_refused(&self, credential: &Credential) -> bool {
        self.siwc.is_some() && self.siwc == token_hash(crate::codex_siwc::refresh_inputs(credential).0)
    }

    /// What still applies to `credential`: a partition whose refresh token changed is no
    /// longer refused.
    pub fn current(&self, credential: &Credential) -> Self {
        Self {
            outer: self.outer.filter(|_| self.outer_refused(credential)),
            siwc: self.siwc.filter(|_| self.siwc_refused(credential)),
        }
    }

    /// Both records, `later`'s partitions over these.
    pub fn merged(&self, later: &Self) -> Self {
        Self {
            outer: later.outer.or(self.outer),
            siwc: later.siwc.or(self.siwc),
        }
    }

    /// Whether the partition requests use (Sign in with ChatGPT when `use_siwc` is on)
    /// has its refresh token refused.
    pub fn active_refused(&self, credential: &Credential) -> bool {
        if crate::codex_siwc::enabled(credential) {
            self.siwc_refused(credential)
        } else {
            self.outer_refused(credential)
        }
    }

    /// `credential` as a refresh sees it: each refused refresh token removed, so the
    /// partition has no renewal credential (never due, never refreshed). Requests keep
    /// using its access token until it expires or an upstream refuses it.
    pub fn mask(&self, credential: &Credential) -> Credential {
        let mut view = credential.clone();
        if self.outer_refused(credential) {
            view.metadata.remove("refresh_token");
            view.metadata.remove("refreshToken");
        }
        if self.siwc_refused(credential)
            && let Some(Value::Object(siwc)) = view.metadata.get_mut("siwc")
        {
            siwc.remove("refresh_token");
        }
        view
    }
}

/// What one refresh grant per partition produced: the merged patch, and each failure.
struct Grants {
    patch: MetadataPatch,
    refreshed: bool,
    failures: Vec<(&'static str, GrantError)>,
    refused: Refused,
}

impl Grants {
    /// The patch when any partition refreshed, else the first failure (outer first).
    fn result(self, credential: &Credential) -> Result<MetadataPatch, ExecError> {
        if !self.refreshed {
            return match self.failures.into_iter().next() {
                Some((_, error)) => Err(error.error),
                None => Ok(self.patch),
            };
        }
        for (partition, error) in self.failures {
            tracing::warn!(
                credential = %credential.id,
                partition,
                status = error.error.status,
                terminal = error.terminal,
                "codex token refresh failed for one partition; the other refreshed"
            );
        }
        Ok(self.patch)
    }
}

/// What a Codex refresh was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshMode {
    /// A due refresh ([`Refresher::prepare`]).
    Due,
    /// After an upstream rejected the token requests use ([`Refresher::refresh_now`]).
    Rejected,
    /// A management refresh ([`Refresher::refresh_all`]).
    Management,
}

/// The refresh grants of one credential on the client its proxy selects. It owns
/// everything it needs, so a refresh can outlive the request that started it.
#[derive(Clone)]
pub struct Refresher {
    oauth: CodexOAuth,
}

impl Refresher {
    pub(crate) fn new(oauth: CodexOAuth) -> Self {
        Self { oauth }
    }

    /// Refreshes the due partitions. Go refreshes Codex only in the background and never
    /// blocks a request on it, so a failed refresh while the token requests use is still
    /// valid keeps the credential usable; the failure is cached for five minutes by the
    /// OAuth service, for each token set.
    pub async fn prepare(&self, credential: &Credential) -> Result<MetadataPatch, ExecError> {
        let now = Utc::now();
        let (outer, siwc) = due_partitions(credential, now);
        self.refresh_due(credential, outer, siwc, now).await.0
    }

    /// [`Self::prepare`] of the partitions `outer` and `siwc`.
    async fn refresh_due(
        &self,
        credential: &Credential,
        outer: bool,
        siwc: bool,
        now: chrono::DateTime<Utc>,
    ) -> (Result<MetadataPatch, ExecError>, Refused) {
        if !outer && !siwc {
            return (Ok(MetadataPatch::default()), Refused::default());
        }
        let grants = self.grants(credential, outer, siwc, true).await;
        let refused = grants.refused;
        (Self::tolerate(credential, now, grants.result(credential)), refused)
    }

    /// A due refresh that failed while the access token requests use is still valid
    /// keeps the credential usable (Go refreshes Codex only in the background).
    fn tolerate(
        credential: &Credential,
        now: chrono::DateTime<Utc>,
        result: Result<MetadataPatch, ExecError>,
    ) -> Result<MetadataPatch, ExecError> {
        let usable = if crate::codex_siwc::enabled(credential) {
            crate::codex_siwc::access_usable(credential, now)
        } else {
            codex_oauth::access_usable(credential, now)
        };
        match result {
            Err(error) if usable => {
                tracing::warn!(
                    credential = %credential.id,
                    status = error.status,
                    "codex token refresh failed; current access token is still valid"
                );
                Ok(MetadataPatch::default())
            }
            other => other,
        }
    }

    /// Refreshes after an upstream rejected the token requests use (Go marks it
    /// `RejectedAccessToken`): that partition (the Sign in with ChatGPT one when
    /// `use_siwc` is on) and any due one. With neither able to refresh, every partition
    /// that can (Go's explicit unscheduled refresh). Nothing without a refresh token.
    pub async fn refresh_now(&self, credential: &Credential) -> Result<MetadataPatch, ExecError> {
        let (outer, siwc) = rejected_partitions(credential, credential, &DiskRotation::default(), Utc::now());
        self.grants(credential, outer, siwc, false).await.result(credential)
    }

    /// A management refresh (Go `IsForceRefreshContext`): every partition that can refresh.
    pub async fn refresh_all(&self, credential: &Credential) -> Result<MetadataPatch, ExecError> {
        let (outer, siwc) = all_partitions(credential, Utc::now());
        self.grants(credential, outer, siwc, false).await.result(credential)
    }

    /// Go `refreshLocked` with the credential file (`disk`) read under its lock: a token
    /// partition another process rotated there replaces the `memory` snapshot's before
    /// anything refreshes, so its refresh token is not spent twice, and the refresh
    /// `mode` asks for then runs on the result. The answer names each partition adopted
    /// or refreshed with its baselines, for [`RefreshPatch::apply`], and whether a grant
    /// was attempted; with one adopted, a failed refresh still answers with it.
    ///
    /// `refused` are the refresh tokens already refused for good: after the disk
    /// adoption, a partition still holding one is not refreshed (its token is never sent
    /// again), whatever `mode` asks. The second value is what is refused now: those still
    /// held, plus every partition this refresh had refused for good.
    pub async fn refresh_locked(
        &self,
        memory: &Credential,
        disk: &Map<String, Value>,
        mode: RefreshMode,
        refused: &Refused,
    ) -> (Result<RefreshPatch, ExecError>, Refused) {
        let rotation = DiskRotation::read(memory, disk);
        let mut adopted = memory.clone();
        rotation.patch.apply(&mut adopted.metadata);
        adopted.refresh_derived();
        let refused = refused.current(&adopted);
        // The partitions are picked on the adopted credential with its refused refresh
        // tokens removed, so no mode spends one.
        let renewable = refused.mask(&adopted);
        let now = Utc::now();
        let (outer, siwc) = match mode {
            RefreshMode::Due => due_partitions(&renewable, now),
            RefreshMode::Rejected => rejected_partitions(&refused.mask(memory), &renewable, &rotation, now),
            RefreshMode::Management => all_partitions(&renewable, now),
        };
        let granted = outer || siwc;
        let (refreshed, now_refused) = match mode {
            RefreshMode::Due => self.refresh_due(&adopted, outer, siwc, now).await,
            _ => {
                let grants = self.grants(&adopted, outer, siwc, false).await;
                let refused = grants.refused;
                (grants.result(&adopted), refused)
            }
        };
        let refused = refused.merged(&now_refused);
        let refresh = match refreshed {
            Ok(refresh) => refresh,
            Err(error) if rotation.outer || rotation.siwc => {
                tracing::warn!(
                    credential = %memory.id,
                    status = error.status,
                    "codex token refresh failed; keeping the tokens another process rotated on disk"
                );
                MetadataPatch::default()
            }
            Err(error) => return (Err(error), refused),
        };
        let mut after = adopted.metadata.clone();
        refresh.apply(&mut after);
        let outer = rotation.outer || fields(&after, &OUTER) != fields(&adopted.metadata, &OUTER);
        let siwc = rotation.siwc || fields(&siwc_map(&after), &SIWC) != fields(&siwc_map(&adopted.metadata), &SIWC);
        let patch = RefreshPatch {
            granted,
            outer: outer.then(|| Partition::new(&memory.metadata, disk, &after, &OUTER)),
            siwc: siwc.then(|| Partition::new(&siwc_map(&memory.metadata), &siwc_map(disk), &siwc_map(&after), &SIWC)),
        };
        (Ok(patch), refused)
    }

    /// Go `refreshLocked`: the outer tokens, then the Sign in with ChatGPT partition, one
    /// after the other, each with its own outcome: a partition that fails while the other
    /// refreshes keeps the other's patch, and each refresh token refused for good is
    /// recorded for its partition alone. A `scheduled` (due) refresh retains a transient
    /// SIWC failure for five minutes, as every outer failure is
    /// ([`CodexOAuth::refresh_siwc_as`]).
    async fn grants(&self, credential: &Credential, outer: bool, siwc: bool, scheduled: bool) -> Grants {
        let outer_token = codex_oauth::refresh_token(credential);
        let outer = if outer {
            Some(self.oauth.refresh_patch(outer_token).await)
        } else {
            None
        };
        let (siwc_token, client, device) = crate::codex_siwc::refresh_inputs(credential);
        let siwc = if siwc {
            Some(
                self.oauth
                    .refresh_siwc_as(siwc_token, client, device, scheduled)
                    .await
                    .map(|tokens| crate::codex_siwc::refresh_patch(credential, &tokens)),
            )
        } else {
            None
        };
        let mut grants = Grants {
            patch: MetadataPatch::default(),
            refreshed: false,
            failures: Vec::new(),
            refused: Refused::default(),
        };
        for (partition, result, token) in [("outer", outer, outer_token), ("SIWC", siwc, siwc_token)] {
            match result {
                Some(Ok(part)) => {
                    grants.refreshed = true;
                    grants.patch.set.extend(part.set);
                    grants.patch.remove.extend(part.remove);
                }
                Some(Err(error)) => {
                    if error.terminal {
                        let hash = token_hash(token);
                        match partition {
                            "outer" => grants.refused.outer = hash,
                            _ => grants.refused.siwc = hash,
                        }
                    }
                    grants.failures.push((partition, error));
                }
                None => {}
            }
        }
        grants
    }
}

/// Go `codexOuterCredentialKeys`.
const OUTER: [&str; 8] = [
    "id_token",
    "access_token",
    "refresh_token",
    "account_id",
    "last_refresh",
    "email",
    "expired",
    "plan_type",
];

/// Go `codexSIWCCredentialKeys`.
const SIWC: [&str; 10] = [
    "access_token",
    "refresh_token",
    "id_token",
    "client_id",
    "device_id",
    "expired",
    "expires_at",
    "last_refresh",
    "scope",
    "scopes",
];

/// Go `credentialFields`.
fn fields(metadata: &Map<String, Value>, keys: &[&str]) -> Map<String, Value> {
    keys.iter()
        .filter_map(|k| metadata.get(*k).map(|v| ((*k).to_owned(), v.clone())))
        .collect()
}

/// The `siwc` object, or nothing (Go's failed `map[string]any` assertion).
fn siwc_map(metadata: &Map<String, Value>) -> Map<String, Value> {
    metadata
        .get("siwc")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// Go `sameCredentialVersion` (`reflect.DeepEqual` on map reads: null reads as missing).
fn same_version(a: &Map<String, Value>, b: &Map<String, Value>) -> bool {
    [
        "access_token",
        "refresh_token",
        "id_token",
        "account_id",
        "client_id",
        "device_id",
    ]
    .iter()
    .all(|k| a.get(*k).filter(|v| !v.is_null()) == b.get(*k).filter(|v| !v.is_null()))
}

/// One partition of a refresh: its fields in the snapshot the refresh started from, in
/// the file as read under the lock, and after the refresh.
#[derive(Debug, Clone)]
struct Partition {
    before: Map<String, Value>,
    disk_before: Map<String, Value>,
    after: Map<String, Value>,
}

impl Partition {
    fn new(before: &Map<String, Value>, disk: &Map<String, Value>, after: &Map<String, Value>, keys: &[&str]) -> Self {
        Self {
            before: fields(before, keys),
            disk_before: fields(disk, keys),
            after: fields(after, keys),
        }
    }

    /// Go `matchesBaselineOrAfter`: the baseline `target` still holds (the file's, the
    /// snapshot's, or the refresh's own result), or `None` when another writer
    /// replaced the partition meanwhile.
    fn baseline(&self, target: &Map<String, Value>) -> Option<&Map<String, Value>> {
        if !self.disk_before.is_empty() && same_version(target, &self.disk_before) {
            return Some(&self.disk_before);
        }
        if same_version(target, &self.before) {
            return Some(&self.before);
        }
        (!self.after.is_empty() && same_version(target, &self.after)).then_some(&self.after)
    }

    /// Go `applyCredentialFields`: the fields the refresh changed against `base`, as Go
    /// holds them (its maps decode numbers as float64).
    fn apply(&self, base: &Map<String, Value>, doc: &mut BTreeMap<String, GoValue>) {
        for (key, value) in &self.after {
            if base.get(key) != Some(value) {
                let go = serde_json::to_vec(value)
                    .ok()
                    .and_then(|text| GoValue::parse_f64(&text))
                    .unwrap_or_else(|| GoValue::from_json(value));
                doc.insert(key.clone(), go);
            }
        }
    }
}

/// Go `CredentialRefreshPatch`: the partitions a refresh adopted from disk or refreshed,
/// each with the versions it may still be applied over.
#[derive(Debug, Clone, Default)]
pub struct RefreshPatch {
    /// A grant was attempted (Go `refreshLocked` then writes the file itself).
    granted: bool,
    outer: Option<Partition>,
    siwc: Option<Partition>,
}

impl RefreshPatch {
    /// A grant was attempted for this refresh; otherwise it only adopted from disk.
    pub fn granted(&self) -> bool {
        self.granted
    }

    /// No partition was adopted or refreshed: nothing to write.
    pub fn is_empty(&self) -> bool {
        self.outer.is_none() && self.siwc.is_none()
    }

    /// Go `CredentialRefreshPatch.CanApplyTo`: some partition still holds a version the
    /// refresh may be applied over in `metadata`.
    pub fn can_apply_to(&self, metadata: &Map<String, Value>) -> bool {
        self.outer.as_ref().is_some_and(|p| p.baseline(metadata).is_some())
            || self
                .siwc
                .as_ref()
                .is_some_and(|p| p.baseline(&siwc_map(metadata)).is_some())
    }

    /// Go `CredentialRefreshPatch.Apply` on the credential file as read under its lock:
    /// `target` is its JSON, `doc` the same document as Go decodes it, which takes the
    /// changes. A partition another writer replaced since the refresh began keeps that
    /// writer's version, without blocking the other partition; every other field stays
    /// as the file has it.
    pub fn apply(&self, target: &Map<String, Value>, doc: &mut BTreeMap<String, GoValue>) {
        if let Some(outer) = &self.outer
            && let Some(base) = outer.baseline(target)
        {
            outer.apply(base, doc);
        }
        if let Some(siwc) = &self.siwc
            && let Some(base) = siwc.baseline(&siwc_map(target))
        {
            let mut merged = doc
                .remove("siwc")
                .and_then(|v| v.into_object().ok())
                .unwrap_or_default();
            siwc.apply(base, &mut merged);
            doc.insert("siwc".into(), GoValue::Object(merged));
        }
    }
}

/// The token partitions another process rotated in the credential file (Go
/// `refreshLocked`'s disk check), and the patch that adopts them.
#[derive(Debug, Default)]
struct DiskRotation {
    patch: MetadataPatch,
    outer: bool,
    siwc: bool,
}

impl DiskRotation {
    /// A partition is rotated when the file holds a non-blank access or refresh token
    /// that differs from the snapshot's (trimmed). A rotated outer partition brings
    /// Go's listed fields the file has, a rotated `siwc` its whole map; `use_siwc` always
    /// follows the file when the file has it. Go also copies `type`; here the provider
    /// a credential was loaded as never changes, and what is written is the file's own
    /// document ([`RefreshPatch::apply`]), so the adopted copy only feeds the grants.
    fn read(credential: &Credential, disk: &Map<String, Value>) -> Self {
        let text =
            |m: &Map<String, Value>, key: &str| m.get(key).and_then(Value::as_str).unwrap_or("").trim().to_owned();
        let rotated = |disk: &Map<String, Value>, memory: &Map<String, Value>| {
            ["refresh_token", "access_token"].iter().any(|key| {
                let on_disk = text(disk, key);
                !on_disk.is_empty() && on_disk != text(memory, key)
            })
        };
        let mut patch = MetadataPatch::default();
        let outer = rotated(disk, &credential.metadata);
        if outer {
            for key in OUTER {
                if let Some(value) = disk.get(key) {
                    patch.set.insert(key.into(), value.clone());
                }
            }
        }
        let disk_siwc = disk.get("siwc").and_then(Value::as_object).filter(|m| !m.is_empty());
        let siwc = disk_siwc.is_some_and(|d| rotated(d, &siwc_map(&credential.metadata)));
        if let Some(d) = disk_siwc.filter(|_| siwc) {
            patch.set.insert("siwc".into(), Value::Object(d.clone()));
        }
        if let Some(value) = disk.get("use_siwc")
            && credential.metadata.get("use_siwc") != Some(value)
        {
            patch.set.insert("use_siwc".into(), value.clone());
        }
        Self { patch, outer, siwc }
    }
}

/// The partitions a due refresh takes (Go `CodexOuterNeedsRefresh` and
/// `CodexSIWCNeedsRefresh`).
fn due_partitions(credential: &Credential, now: chrono::DateTime<Utc>) -> (bool, bool) {
    (
        codex_oauth::refresh_due(credential, now),
        crate::codex_siwc::refresh_due(credential, now) == Some(true),
    )
}

/// The partitions a management refresh takes: every one that can refresh.
fn all_partitions(credential: &Credential, now: chrono::DateTime<Utc>) -> (bool, bool) {
    (
        !codex_oauth::refresh_token(credential).is_empty(),
        crate::codex_siwc::refresh_due(credential, now).is_some(),
    )
}

/// Go `authOuterAccessToken`.
fn outer_access_token(credential: &Credential) -> &str {
    ["access_token", "accessToken"]
        .iter()
        .filter_map(|k| credential.str(k).map(str::trim))
        .find(|t| !t.is_empty())
        .unwrap_or_default()
}

/// The partitions a refresh after a rejected token takes. On the `memory` snapshot whose
/// token was rejected: the partition requests use (Sign in with ChatGPT when `use_siwc`
/// is on) and any due one; with neither able to refresh, every partition that can (Go's
/// explicit unscheduled refresh), except one `rotation` adopted from disk unless it is
/// due there. In `adopted`, the partition requests use is forced only while it still
/// holds the rejected access token (Go compares the token with `RejectedAccessToken`):
/// a rotated refresh token alone does not replace it.
fn rejected_partitions(
    memory: &Credential,
    adopted: &Credential,
    rotation: &DiskRotation,
    now: chrono::DateTime<Utc>,
) -> (bool, bool) {
    let active_siwc = crate::codex_siwc::enabled(memory);
    let pick = |c: &Credential, outer_replaced: bool, siwc_replaced: bool| {
        let outer = !codex_oauth::refresh_token(c).is_empty()
            && ((!active_siwc && !outer_replaced) || codex_oauth::refresh_due(c, now));
        let siwc = crate::codex_siwc::refresh_due(c, now).is_some_and(|due| due || (active_siwc && !siwc_replaced));
        (outer, siwc)
    };
    if pick(memory, false, false) != (false, false) {
        return pick(
            adopted,
            outer_access_token(adopted) != outer_access_token(memory),
            crate::codex_siwc::access_token(adopted) != crate::codex_siwc::access_token(memory),
        );
    }
    let outer =
        !codex_oauth::refresh_token(adopted).is_empty() && (!rotation.outer || codex_oauth::refresh_due(adopted, now));
    let siwc = crate::codex_siwc::refresh_due(adopted, now).is_some_and(|due| due || !rotation.siwc);
    (outer, siwc)
}

#[cfg(test)]
#[path = "codex_refresh_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "codex_refresh_head_tests.rs"]
mod head_tests;
