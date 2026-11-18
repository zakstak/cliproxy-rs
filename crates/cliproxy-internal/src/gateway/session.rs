//! A request's session identity for affinity and executors (Go `session.Enrich` and
//! `SessionAffinitySelector.Pick`'s identity step). The extraction rules live in
//! `crate::protocol::session` so executors share them.

use crate::format::Format;
use crate::protocol::session::{self as identity, Meta};
use crate::text::GoStr;
use axum::http::HeaderMap;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Session {
    /// Go `ExtractSessionID`: the affinity session and `ExecRequest::session`.
    pub id: Option<String>,
    /// The parent or prompt-cache conversation alias (Go fallback ID).
    pub parent: Option<String>,
    /// The session forked from `parent`.
    pub fork: bool,
    /// Go `derived_session_id` (see `ExecRequest::derived_session`).
    pub derived: Option<String>,
    /// A client header, body field or execution session named the session (Go
    /// `extractExplicitSessionIDs`). Without one, session affinity asks the LCP matcher
    /// first (`crate::gateway::lcp`).
    pub explicit: bool,
    /// Where `id` came from, for the usage record (not in Go): an explicit source
    /// ([`identity::SessionInfo::source`]), `derived` (Go `derived_session_id`: the
    /// leading instructions and first user input) or `messages` (the first-messages
    /// hash); empty without `id`.
    pub source: &'static str,
    /// A Codex Guardian reviewer (`X-Openai-Subagent: guardian`, first-party
    /// openai/codex `core/src/client.rs`): a subagent that may inherit its parent's
    /// binding on another model.
    pub guardian: bool,
    /// The matched client key's caller scope ([`identity::caller_scope`]): bindings a
    /// child inherits from its parent must come from the same caller.
    pub caller: String,
    /// `codex:<thread>` for a request naming a Codex thread ([`identity::codex_thread`]):
    /// the parent identity a Guardian reviewer names, which can differ from `id` (a named
    /// agent resolves to `codex:<root>:agent:<name>`).
    pub thread: Option<String>,
}

/// Resolves the session of a request in `format` from its client headers and body.
/// `caller` is the matched client key; it scopes the derived identity.
pub fn resolve(format: Format, headers: &HeaderMap, body: &[u8], execution: Option<&str>, caller: &str) -> Session {
    let scope = identity::caller_scope(caller);
    let derived = identity::derived_id(format, headers, body, execution, &scope);
    let meta = Meta {
        execution_session: execution,
        derived: derived.as_deref(),
    };
    let (mut primary, mut parent, fork, mut source) = identity::explicit_session(headers, body, &meta);
    let explicit = !primary.is_empty();
    if !explicit {
        (primary, parent) = identity::session_ids(headers, body, &meta);
        source = match primary.as_str() {
            "" => "",
            id if id.starts_with("derived:") => "derived",
            _ => "messages",
        };
    }
    let some = |s: String| (!s.is_empty()).then(|| identity::bound_session_identity(&s));
    let guardian = headers
        .get("x-openai-subagent")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().go_eq_fold("guardian"));
    let thread = identity::codex_thread(headers);
    Session {
        id: some(primary),
        parent: some(parent),
        fork,
        derived,
        explicit,
        source,
        guardian,
        caller: scope,
        thread: (!thread.is_empty()).then(|| identity::bound_session_identity(&format!("codex:{thread}"))),
    }
}

#[cfg(test)]
mod tests {
    use crate::format::Format;
    use crate::protocol::session as identity;
    use axum::http::{HeaderName, HeaderValue};

    /// #113: first-party Codex (openai/codex@44ccf394573c `core/src/client.rs`,
    /// `core/src/responses_metadata.rs`, `core/src/codex_delegate.rs`) sends a Guardian
    /// reviewer's request with the root's `session-id`, its own `thread-id`,
    /// `x-openai-subagent: guardian` and `x-codex-parent-thread-id`. It resolves to a
    /// subagent of the parent's own session key, marked as a Guardian reviewer, with the
    /// caller's scope.
    #[test]
    fn codex_guardian_reviewers_name_their_parents_session() {
        let headers = |pairs: &[(&'static str, &str)]| {
            let mut map = super::HeaderMap::new();
            for (name, value) in pairs {
                map.insert(*name, HeaderValue::from_str(value).unwrap());
            }
            map
        };
        let root = "019a0000-0000-7000-8000-000000000001";
        let parent = super::resolve(
            Format::OpenAIResponse,
            &headers(&[("session-id", root), ("thread-id", root)]),
            b"{}",
            None,
            "client-key",
        );
        assert_eq!(parent.id.as_deref(), Some(format!("codex:{root}").as_str()));
        assert!(!parent.guardian);
        let review = super::resolve(
            Format::OpenAIResponse,
            &headers(&[
                ("session-id", root),
                ("thread-id", "019a0000-0000-7000-8000-000000000002"),
                ("x-openai-subagent", "guardian"),
                ("x-codex-parent-thread-id", root),
            ]),
            b"{}",
            None,
            "client-key",
        );
        assert!(review.guardian);
        assert_eq!(review.parent, parent.id, "the parent's own session key");
        assert_ne!(review.id, parent.id);
        assert_eq!(review.caller, identity::caller_scope("client-key"));
        assert!(identity::is_subagent_session(
            review.id.as_deref().unwrap(),
            review.parent.as_deref().unwrap()
        ));
        let other = super::resolve(
            Format::OpenAIResponse,
            &headers(&[("session-id", root), ("x-openai-subagent", "review")]),
            b"{}",
            None,
            "client-key",
        );
        assert!(!other.guardian, "only the Guardian reviewer");
        // A named agent resolves to `codex:<root>:agent:<name>`; its reviewer names its
        // thread, which the session carries.
        let worker_thread = "019a0000-0000-7000-8000-000000000005";
        let metadata = format!(r#"{{"session_id":"{root}","thread_id":"{worker_thread}","agent_name":"worker"}}"#);
        let worker = super::resolve(
            Format::OpenAIResponse,
            &headers(&[
                ("session-id", root),
                ("thread-id", worker_thread),
                ("x-openai-subagent", "collab_spawn"),
                ("x-codex-turn-metadata", &metadata),
            ]),
            b"{}",
            None,
            "client-key",
        );
        assert_eq!(
            worker.id.as_deref(),
            Some(format!("codex:{root}:agent:worker").as_str())
        );
        let worker_review = super::resolve(
            Format::OpenAIResponse,
            &headers(&[
                ("session-id", root),
                ("thread-id", "019a0000-0000-7000-8000-000000000006"),
                ("x-openai-subagent", "guardian"),
                ("x-codex-parent-thread-id", worker_thread),
            ]),
            b"{}",
            None,
            "client-key",
        );
        assert_eq!(worker_review.parent, worker.thread, "named by the worker's thread");
    }

    #[test]
    fn request_session_resolution_owns_explicit_precedence_and_caller_isolation() {
        let headers = |pairs: &[(&str, &str)]| {
            let mut out = super::HeaderMap::new();
            for (name, value) in pairs {
                out.append(
                    HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    HeaderValue::from_str(value).unwrap(),
                );
            }
            out
        };
        for (header, prefix) in [
            ("x-claude-code-session-id", "claude:"),
            ("session-id", "codex:"),
            ("x-session-id", "header:"),
            ("x-session-affinity", "affinity:"),
            ("x-slot-session-id", "slot:"),
            ("x-task-id", "task:"),
            ("x-conversation-id", "conv:"),
            ("x-thread-id", "thread:"),
            ("x-client-request-id", "clientreq:"),
        ] {
            for format in [Format::OpenAI, Format::Claude, Format::OpenAIResponse] {
                let actual = super::resolve(
                    format,
                    &headers(&[(header, " root ")]),
                    br#"{"session_id":"body"}"#,
                    Some("execution"),
                    "fake-client-key",
                );
                assert_eq!(actual.id, Some(format!("{prefix}root")));
                assert!(actual.explicit);
                assert_eq!(actual.source, "header");
                assert!(actual.derived.is_none());
                assert!(actual.parent.is_none());
                assert!(!actual.caller.is_empty());
                assert!(!actual.caller.contains("fake-client-key"));
            }
        }
        for (body, expected) in [
            (r#"{"session_id":"body"}"#, "session:body"),
            (r#"{"thread_id":"body"}"#, "thread:body"),
            (r#"{"task_id":"body"}"#, "task:body"),
            (r#"{"prompt_cache_key":"body"}"#, "pck:body"),
        ] {
            let actual = super::resolve(
                Format::OpenAI,
                &headers(&[]),
                body.as_bytes(),
                Some("execution"),
                "caller",
            );
            assert_eq!(actual.id.as_deref(), Some(expected));
            assert!(actual.explicit);
            assert_eq!(actual.source, "body");
        }
        let actual = super::resolve(Format::OpenAI, &headers(&[]), b"{}", Some(" execution "), "caller");
        assert_eq!(actual.id.as_deref(), Some("execution:execution"));
        assert!(actual.explicit);
        assert_eq!(actual.source, "execution");
        let child = super::resolve(
            Format::Claude,
            &headers(&[
                ("x-claude-code-session-id", "root"),
                ("x-claude-code-agent-id", "worker"),
                ("x-claude-code-parent-agent-id", "planner"),
            ]),
            b"{}",
            None,
            "caller",
        );
        assert_eq!(child.id.as_deref(), Some("claude:root:agent:worker"));
        assert_eq!(child.parent.as_deref(), Some("claude:root:agent:planner"));
        assert!(!child.fork);
        let aliases = super::resolve(
            Format::OpenAIResponse,
            &headers(&[]),
            br#"{"prompt_cache_key":"prompt","conversation":{"id":"conversation"}}"#,
            None,
            "caller",
        );
        assert_eq!(aliases.id.as_deref(), Some("pck:prompt"));
        assert_eq!(aliases.parent.as_deref(), Some("conv:conversation"));
        for format in [Format::OpenAI, Format::Claude, Format::OpenAIResponse] {
            let body = if format == Format::OpenAIResponse {
                br#"{"instructions":"policy","input":[{"role":"user","content":"hello"}]}"#.as_slice()
            } else {
                br#"{"messages":[{"role":"system","content":"policy"},{"role":"user","content":"hello"}]}"#.as_slice()
            };
            let a = super::resolve(format, &headers(&[]), body, None, "caller-a");
            let again = super::resolve(format, &headers(&[]), body, None, "caller-a");
            let b = super::resolve(format, &headers(&[]), body, None, "caller-b");
            assert_eq!(a, again);
            assert!(!a.explicit);
            assert_eq!(a.source, "derived");
            assert!(a.id.as_ref().unwrap().starts_with("derived:"));
            assert!(a.derived.is_some());
            assert_ne!(a.id, b.id);
            assert_ne!(a.caller, b.caller);
        }
        for body in [b"".as_slice(), b"{}", b"{broken", b"null"] {
            let actual = super::resolve(Format::OpenAI, &headers(&[]), body, None, "");
            assert!(actual.id.is_none());
            assert!(!actual.explicit);
            assert_eq!(actual.source, "");
        }
    }
}
