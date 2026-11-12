//! The Muse Code client identity sent on Meta `/v1/responses` requests (#81).
//!
//! Provenance: the official Muse Code 1.4.15-R8339.1 linux-x86_64 binary (SHA-256
//! `abe0a72878c62b570c6eed6c17c5fc3c520a990ecf040511741c00092312a3d5`, matching the
//! checksum in Meta's release manifest at
//! `lookaside.facebook.com/lookaside/muse/download/?channel=muse&version=1.4.15-R8339.1&file=manifest.json`),
//! run with `muse exec` against a loopback mock with fake credentials in a network-denied
//! namespace on 2026-11-12. See docs/DIFFERENCES-FROM-GO.md, "Muse Code client identity".
//! What that capture showed for the main agent's requests:
//! - `User-Agent` and `X-Client-Id` below; `muse exec` is the `tbh:exec` surface.
//! - `X-Tbh-Session-Id`, `X-Meta-Ai-Gateway-Session-Id`, `X-Tbh-Thread-Id` and
//!   `X-Tbh-Root-Session-Id` carry one UUIDv7, the session's, and `X-Tbh-Agent-Role` is
//!   `main`. The id is the same on every turn of a session (`--session-id`) and new for a
//!   new session.
//! - With an account login, `X-Tbh-Contributor-Mode` is `1` exactly when the request's
//!   model is a `-contributor` model and `0` otherwise; with an API key it is never sent.
//!
//! Not sent, though the client sends them: `traceparent` (a fresh W3C trace per request;
//! no source says Meta needs it) and the side-agent headers (`X-Tbh-Parent-Thread-Id`,
//! other agent roles). `X-FB-Routing-Control` is sent by the client only when
//! `MUSE_WWW_ROUTING` is set, so it is not part of this profile.

use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use cpa_common::thinking::parse_suffix;
use cpa_core::credential::Credential;
use cpa_core::exec::ExecRequest;
use cpa_core::gostr::GoStr;
use sha2::{Digest, Sha256};

/// The captured client's `User-Agent`.
pub const USER_AGENT: &str =
    "muse-build/1.4.15 (non-interactive; linux-x86_64; build c7a95b745e7eb209c8446981803d89c497d7cae0)";
/// The captured client's surface (`muse exec`).
pub const CLIENT_SURFACE: &str = "tbh:exec";

// ponytail: explicit client sessions keep their Muse session id for the life of the
// process, up to this many sessions, oldest dropped first; a restart or an evicted
// session starts a new Muse session, as a new Muse Code session would.
const SESSION_CAPACITY: usize = 4096;

#[derive(Default)]
struct Sessions {
    ids: HashMap<SessionKey, String>,
    order: VecDeque<SessionKey>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct SessionKey {
    credential: String,
    authorization: [u8; 32],
    caller: String,
    session: String,
}

static SESSIONS: LazyLock<Mutex<Sessions>> = LazyLock::new(Mutex::default);

/// An RFC 9562 UUIDv7: the current Unix time in milliseconds, then random bits.
pub(crate) fn uuid_v7() -> String {
    let mut bytes = *uuid::Uuid::new_v4().as_bytes();
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default();
    bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..]);
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    // The variant bits are v4's (RFC 9562 `10`).
    uuid::Uuid::from_bytes(bytes).to_string()
}

/// The Muse session id for this request: one per explicit client session and
/// credential, kept while the process runs; a fresh one per request otherwise.
///
/// Only the client's own conversation signals count (Go `extractExplicitSessionIDs`). The
/// derived, first-messages and conversation-prefix sessions are guesses that can join
/// unrelated clients, so a request without an explicit session is its own Muse session,
/// as in Go v8.0.67. A plain user identity is skipped while searching for a
/// conversation, before falling back to a transport-owned execution session.
/// Credential authorization and caller fingerprints fence retained sessions without
/// retaining credentials. A DCA token survives ordinary minted-key refresh, whereas
/// another account's DCA token (or a changed API key) starts a separate session.
pub(crate) fn session_id(credential: &Credential, req: &ExecRequest, original: &[u8]) -> String {
    let Some(info) =
        cpa_common::session::extract_conversation_info(&req.headers, original, req.execution_session.as_deref())
    else {
        return uuid_v7();
    };
    let Some(authorization) = authorization_scope(credential) else {
        return uuid_v7();
    };
    let key = SessionKey {
        credential: credential.id.clone(),
        authorization,
        caller: cpa_common::session::caller_scope(&req.caller.principal),
        session: info.session_id,
    };
    let mut sessions = SESSIONS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(id) = sessions.ids.get(&key) {
        return id.clone();
    }
    let id = uuid_v7();
    if sessions.order.len() >= SESSION_CAPACITY
        && let Some(oldest) = sessions.order.pop_front()
    {
        sessions.ids.remove(&oldest);
    }
    sessions.order.push_back(key.clone());
    sessions.ids.insert(key, id.clone());
    id
}

/// The login root, not a minted key that normal account refresh replaces. Without
/// a DCA token the effective API key is the only available authorization identity;
/// changing it conservatively starts a new session.
fn authorization_scope(credential: &Credential) -> Option<[u8; 32]> {
    let (kind, secret) = match crate::meta::dca_token(credential) {
        Some(dca) => ("dca", dca),
        None => ("api-key", crate::meta::creds(credential).1),
    };
    if secret.is_empty() {
        return None;
    }
    let mut hash = Sha256::new();
    hash.update(b"cliproxy-rs muse session authorization\0");
    hash.update(kind.as_bytes());
    hash.update(b"\0");
    hash.update(secret.as_bytes());
    Some(hash.finalize().into())
}

/// Go `isMetaContributorModel`: the model, without a thinking suffix, ends in
/// `-contributor`.
pub(crate) fn is_contributor_model(model: &str) -> bool {
    parse_suffix(model.trim())
        .model_name
        .trim()
        .go_lower()
        .ends_with("-contributor")
}

/// Whether the credential is a Muse account login (a DCA token or an OAuth record),
/// whose requests carry `X-Tbh-Contributor-Mode`, rather than an API key.
pub(crate) fn is_account_login(credential: &Credential) -> bool {
    cpa_core::registry::dynamic::auth_kind(credential) == Some("oauth") || crate::meta::dca_token(credential).is_some()
}

/// The identity headers for one request, in the order the client sends them.
/// `effective_model` is the model the prepared body sends, after aliases and payload
/// rules.
pub(crate) fn headers(credential: &Credential, session: &str, effective_model: &str) -> Vec<(&'static str, String)> {
    let mut out = vec![
        ("User-Agent", USER_AGENT.to_owned()),
        ("X-Client-Id", CLIENT_SURFACE.to_owned()),
    ];
    if is_account_login(credential) {
        let mode = if is_contributor_model(effective_model) {
            "1"
        } else {
            "0"
        };
        out.push(("X-Tbh-Contributor-Mode", mode.to_owned()));
    }
    for name in [
        "X-Tbh-Session-Id",
        "X-Meta-Ai-Gateway-Session-Id",
        "X-Tbh-Thread-Id",
        "X-Tbh-Root-Session-Id",
    ] {
        out.push((name, session.to_owned()));
    }
    out.push(("X-Tbh-Agent-Role", "main".to_owned()));
    out
}
