//! Who a Codex upstream request says it comes from (#26).
//!
//! The first-party contract (`docs/codex/CONTRACT.md`, "Identity and headers") is the
//! authority here, not Go v8.0.35. Go overwrote every OAuth request with a fixed
//! `codex-tui` macOS/iTerm identity ("cloaking"). This module replaces that with one
//! explicit policy for every Codex-owned caller: Responses HTTP and WebSocket, Images,
//! Alpha Search, live calls and the OAuth/SIWC token endpoints.
//!
//! | Header | Who sets it |
//! | --- | --- |
//! | `Authorization`, `chatgpt-account-id` | The selected credential only. A caller value, or an operator header that copies one, never reaches upstream. |
//! | `User-Agent`, `originator` | The caller, on a native Responses exchange ([`Origin::Caller`]); otherwise the proxy's own [`USER_AGENT`] and [`ORIGINATOR`]. Operator config and the model catalog cannot set them. |
//! | Codex protocol metadata (`x-codex-*`, `session-id`, `thread-id`, `OpenAI-Beta`, ...) | Forwarded from the caller by each call site's allowlist; unchanged by this module. |
//!
//! docs/DIFFERENCES-FROM-GO.md lists the resulting differences from Go.

use std::borrow::Cow;
use std::collections::BTreeMap;

use cpa_core::credential::Credential;
use cpa_core::exec::{ExecError, ExecRequest, FailureScope};
use cpa_core::format::Format;
use http::HeaderMap;

/// The proxy's own `User-Agent`, for requests it formulates itself.
pub const USER_AGENT: &str = concat!("cliproxy-rs/", env!("CARGO_PKG_VERSION"));

/// The proxy's own `originator`, where the profile carries one and the caller sent none.
pub const ORIGINATOR: &str = "cliproxy-rs";

/// Headers only the selected credential sets.
const CREDENTIAL_HEADERS: [&str; 2] = ["authorization", "chatgpt-account-id"];

/// Headers that name the client. Only a native caller or the proxy itself sets them.
const IDENTITY_HEADERS: [&str; 2] = ["user-agent", "originator"];

/// Client request headers that can carry a proxy client key (`cpa-server` `access.rs`
/// reads `Authorization`, `X-Goog-Api-Key` and `X-Api-Key`) or another caller credential.
const CLIENT_CREDENTIAL_HEADERS: [&str; 5] = [
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "x-goog-api-key",
    "cookie",
];

/// Who formulated the request body that goes upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    /// A Responses client talking Responses to a Codex upstream (Codex, an OpenAI SDK, a
    /// custom agent). The proxy relays its request, so its own `User-Agent` and
    /// `originator` describe the request truthfully.
    Caller,
    /// The proxy built the request: a cross-API translation, the Images API, live calls
    /// or a token grant. The caller's identity would describe a request it never made.
    Proxy,
}

impl Origin {
    /// Decided by the formats on both sides, never by what the caller claims to be.
    pub fn of(req: &ExecRequest) -> Self {
        let responses = |f: Format| matches!(f, Format::Codex | Format::OpenAIResponse);
        if responses(req.source_format) && responses(req.response_format) {
            Origin::Caller
        } else {
            Origin::Proxy
        }
    }
}

fn client_value<'a>(client: &'a HeaderMap, name: &str) -> Option<&'a str> {
    client
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

/// The upstream `User-Agent`: the caller's on a native exchange, else the proxy's.
pub(crate) fn user_agent(origin: Origin, client: &HeaderMap) -> &str {
    match origin {
        Origin::Caller => client_value(client, "user-agent").unwrap_or(USER_AGENT),
        Origin::Proxy => USER_AGENT,
    }
}

/// The upstream `originator`: the caller's on a native exchange; otherwise the proxy's
/// when `default` says the profile carries one (Go sent it on OAuth credentials only).
pub(crate) fn originator(origin: Origin, client: &HeaderMap, default: bool) -> Option<&str> {
    let caller = match origin {
        Origin::Caller => client_value(client, "originator"),
        Origin::Proxy => None,
    };
    caller.or(default.then_some(ORIGINATOR))
}

/// Why an operator header may not go upstream on a Codex credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Violation {
    /// `header:User-Agent` or `header:Originator`: an operator-chosen client identity.
    Identity(String),
    /// `header:Authorization` or `header:chatgpt-account-id` with a value taken from the
    /// request (`$Header` or `$CPA-SESSION-ID`): the caller would pick the credential.
    CallerCredential(String),
    /// A value copied from a client credential header such as `$Authorization`: the
    /// proxy client key would travel upstream.
    ClientKey { header: String, source: String },
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Violation::Identity(name) => write!(
                f,
                "header {name:?} is not supported on Codex credentials: Codex requests send the caller's identity or cliproxy-rs's own"
            ),
            Violation::CallerCredential(name) => write!(
                f,
                "header {name:?} may not take its value from the request: only the selected credential authenticates upstream"
            ),
            Violation::ClientKey { header, source } => write!(
                f,
                "header {header:?} copies client header {source:?}: client keys never travel upstream"
            ),
        }
    }
}

/// Header names are ASCII protocol tokens, so an ASCII comparison is exact.
fn is_one_of(name: &str, set: &[&str]) -> bool {
    set.iter().any(|n| name.eq_ignore_ascii_case(n))
}

/// Classifies one `header:<name>` attribute by its raw (unresolved) value, mirroring how
/// `cpa_common::headers::custom_headers` resolves it.
fn classify(name: &str, raw: &str) -> Option<Violation> {
    let value = raw.trim();
    if is_one_of(name, &IDENTITY_HEADERS) {
        return Some(Violation::Identity(name.to_owned()));
    }
    let variable = value.strip_prefix('$').map(str::trim);
    let session = value.to_ascii_uppercase().contains("$CPA-SESSION-ID");
    if is_one_of(name, &CREDENTIAL_HEADERS) && (variable.is_some() || session) {
        return Some(Violation::CallerCredential(name.to_owned()));
    }
    match variable {
        Some(source) if is_one_of(source, &CLIENT_CREDENTIAL_HEADERS) => Some(Violation::ClientKey {
            header: name.to_owned(),
            source: source.to_owned(),
        }),
        _ => None,
    }
}

fn header_attributes(credential: &Credential) -> impl Iterator<Item = (&str, &str, &str)> {
    credential.attributes.iter().filter_map(|(key, value)| {
        let name = key.strip_prefix("header:")?.trim();
        (!name.is_empty() && !value.trim().is_empty()).then_some((key.as_str(), name, value.as_str()))
    })
}

/// The first operator header on `credential` that the policy refuses.
pub(crate) fn violation(credential: &Credential) -> Option<Violation> {
    header_attributes(credential).find_map(|(_, name, value)| classify(name, value))
}

/// Refuses a Codex request whose credential carries an operator header the policy
/// forbids, wherever the attribute came from (config, an auth file's `headers` or
/// Home). The error names the header, never its value.
pub(crate) fn check_credential(credential: &Credential) -> Result<(), ExecError> {
    match violation(credential) {
        None => Ok(()),
        Some(v) => Err(ExecError::local(
            500,
            FailureScope::Credential,
            format!("codex credential {}: {v}", credential.id),
        )),
    }
}

/// The credential's attributes without the forbidden `header:` entries: a second line
/// behind [`check_credential`], so no call site can forward one.
pub(crate) fn permitted_attributes(credential: &Credential) -> Cow<'_, BTreeMap<String, String>> {
    if violation(credential).is_none() {
        return Cow::Borrowed(&credential.attributes);
    }
    let forbidden: Vec<String> = header_attributes(credential)
        .filter(|(_, name, value)| classify(name, value).is_some())
        .map(|(key, _, _)| key.to_owned())
        .collect();
    let mut attributes = credential.attributes.clone();
    attributes.retain(|key, _| !forbidden.contains(key));
    Cow::Owned(attributes)
}

/// Whether a model catalog `override_header` may set `name`. The catalog can carry
/// protocol headers, but no client identity and no credential.
pub(crate) fn model_override_permitted(name: &str) -> bool {
    !is_one_of(name.trim(), &IDENTITY_HEADERS) && !is_one_of(name.trim(), &CREDENTIAL_HEADERS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential(headers: &[(&str, &str)]) -> Credential {
        let mut credential = Credential {
            id: "codex-identity.json".into(),
            provider: "codex".into(),
            source: cpa_core::credential::Source::File("/fake/codex-identity.json".into()),
            disabled: false,
            label: String::new(),
            attributes: Default::default(),
            metadata: Default::default(),
            revision: 0,
        };
        for (name, value) in headers {
            credential.attributes.insert(format!("header:{name}"), (*value).into());
        }
        credential
    }

    #[test]
    fn proxy_identity_names_this_proxy_and_its_version() {
        assert_eq!(USER_AGENT, format!("cliproxy-rs/{}", env!("CARGO_PKG_VERSION")));
        for claim in ["codex", "Mac OS", "iTerm", "Chrome", "Go-http-client", "CLIProxyAPI"] {
            assert!(!USER_AGENT.contains(claim), "{claim}");
            assert!(!ORIGINATOR.contains(claim), "{claim}");
        }
    }

    #[test]
    fn origin_follows_the_formats_not_the_claimed_client() {
        let mut client = HeaderMap::new();
        client.insert("user-agent", "my-agent/1".parse().unwrap());
        client.insert("originator", "my_app".parse().unwrap());
        assert_eq!(user_agent(Origin::Caller, &client), "my-agent/1");
        assert_eq!(originator(Origin::Caller, &client, false), Some("my_app"));
        assert_eq!(user_agent(Origin::Proxy, &client), USER_AGENT);
        assert_eq!(originator(Origin::Proxy, &client, true), Some(ORIGINATOR));
        assert_eq!(originator(Origin::Proxy, &client, false), None);
        let empty = HeaderMap::new();
        assert_eq!(user_agent(Origin::Caller, &empty), USER_AGENT);
        assert_eq!(originator(Origin::Caller, &empty, true), Some(ORIGINATOR));
        assert_eq!(originator(Origin::Caller, &empty, false), None);
    }

    #[test]
    fn operator_headers_cannot_set_identity_or_borrow_caller_credentials() {
        let refused = [
            ("User-Agent", "codex-tui/0.154.0 (Mac OS 26.5.2; arm64)", "User-Agent"),
            ("originator", "codex-tui", "originator"),
            ("Authorization", "$Authorization", "Authorization"),
            ("chatgpt-account-id", "$X-Account", "chatgpt-account-id"),
            ("Chatgpt-Account-Id", "acct-$CPA-SESSION-ID", "Chatgpt-Account-Id"),
            ("X-Forward", "$authorization", "X-Forward"),
            ("X-Forward", "$ X-Api-Key", "X-Forward"),
            ("X-Forward", "$X-Goog-Api-Key", "X-Forward"),
            ("X-Forward", "$Cookie", "X-Forward"),
        ];
        for (name, value, named) in refused {
            let c = credential(&[(name, value), ("X-Fixed", "kept")]);
            let error = check_credential(&c).expect_err(name);
            let message = String::from_utf8_lossy(&error.body).into_owned();
            assert_eq!(error.scope, FailureScope::Credential);
            assert!(message.contains(named), "{message}");
            assert!(!message.contains("Mac OS"), "values stay out of errors: {message}");
            let kept = permitted_attributes(&c);
            assert!(!kept.contains_key(&format!("header:{name}")), "{name}={value}");
            assert_eq!(kept.get("header:X-Fixed").map(String::as_str), Some("kept"));
        }
        let allowed = credential(&[
            ("Authorization", "Bearer operator-static"),
            ("chatgpt-account-id", "acct-static"),
            ("X-Session", "$CPA-SESSION-ID"),
            ("X-From-Client", "$X-Request-Id"),
        ]);
        assert!(check_credential(&allowed).is_ok());
        assert!(matches!(permitted_attributes(&allowed), Cow::Borrowed(_)));
    }

    #[test]
    fn model_overrides_cannot_set_identity_or_credentials() {
        for name in ["user-agent", "Originator", " Authorization ", "chatgpt-account-id"] {
            assert!(!model_override_permitted(name), "{name}");
        }
        assert!(model_override_permitted("x-codex-beta-features"));
    }
}
