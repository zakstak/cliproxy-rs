//! Tool aliases for Anthropic thread continuations (claude_executor_tool_state.go,
//! v8.0.27: 678a925, a2976eb, e7b2226).
//!
//! A continuation (`thread.type: continue` with a `previous_message_id`) may omit its
//! tool declarations, so it carries nothing to alias, yet the upstream still answers
//! with the aliased tool names. The aliases of each thread turn are therefore kept,
//! keyed by its upstream message ID, and a continuation without tools restores them
//! from here. Without that state the client has to replay the whole conversation, so
//! the request fails with Anthropic's own 404.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use cpa_core::exec::{ExecError, FailureScope};

use super::alias::Reverse;
use super::signals::may_contain;
use crate::rawjson;

/// `claudeOAuthToolAliasStateLimit`: entries kept, oldest key evicted first.
const LIMIT: usize = 1024;

/// `claudeThreadNotFoundErrorMessage`.
pub const NOT_FOUND: &str = "No thread state was found for the requested previous_message_id. Replay the full conversation with thread create to start a new Thread.";

/// `claudeOAuthToolAliasStore`: one per executor, shared by every request it serves.
///
/// Bounded at [`LIMIT`] keys with no expiry, as in Go. A turn's alias map (one entry
/// per declared tool) is built once, when its request's tools are remapped; the turn
/// and the continuations that reuse it share that map under their keys.
#[derive(Default)]
pub(crate) struct Aliases(Mutex<State>);

#[derive(Default)]
struct State {
    entries: HashMap<String, Arc<Reverse>>,
    order: VecDeque<String>,
}

impl Aliases {
    fn load(&self, key: &str) -> Option<Arc<Reverse>> {
        let state = self.0.lock().expect("thread alias store");
        state.entries.get(key).cloned()
    }

    fn save(&self, keys: &[String], aliases: &Arc<Reverse>) {
        let mut state = self.0.lock().expect("thread alias store");
        for key in keys {
            if !state.entries.contains_key(key) {
                state.order.push_back(key.clone());
            }
            state.entries.insert(key.clone(), aliases.clone());
        }
        while state.order.len() > LIMIT {
            if let Some(oldest) = state.order.pop_front() {
                state.entries.remove(&oldest);
            }
        }
    }

    /// `prepareClaudeOAuthToolNamesForRequest`'s continuation half: the aliases of the
    /// turn a tool-less continuation continues, or `None` when `body` is not one.
    pub(crate) fn for_continuation(&self, thread: &Thread, body: &str) -> Option<Result<Arc<Reverse>, ExecError>> {
        if !thread.continues || thread.previous.is_empty() {
            return None;
        }
        let tools = rawjson::get(body, "tools");
        if tools.kind() == gjson::Kind::Array && !tools.array().is_empty() {
            return None;
        }
        Some(self.load(&format!("message:{}", thread.previous)).ok_or_else(not_found))
    }

    /// `rememberClaudeOAuthToolAliases` once the upstream answered `message_id`.
    /// Per completed thread turn: one lock, up to two keys and two reference counts;
    /// requests without a `thread` pay nothing.
    pub(crate) fn remember(&self, thread: &Thread, aliases: &Arc<Reverse>, message_id: &str) {
        let keys: Vec<String> = [thread.previous.as_str(), message_id]
            .iter()
            .filter(|id| !id.is_empty())
            .map(|id| format!("message:{id}"))
            .collect();
        if !keys.is_empty() {
            self.save(&keys, aliases);
        }
    }
}

/// A request that belongs to a thread (`thread.type` set), read before the body is
/// rewritten.
pub(crate) struct Thread {
    continues: bool,
    previous: String,
}

impl Thread {
    /// Per request: a substring scan of `body`; the lookups run only for a body that
    /// may hold a `thread` key.
    pub(crate) fn of(body: &str) -> Option<Self> {
        if !may_contain(body, &["\"thread\""]) {
            return None;
        }
        let kind = rawjson::get(body, "thread.type");
        (!kind.str().is_empty()).then(|| Self {
            continues: kind.str() == "continue",
            previous: rawjson::string(body, "thread.previous_message_id"),
        })
    }
}

/// A non-null `thread` field, which takes one of the request's four breakpoints.
pub(crate) fn present(body: &str) -> bool {
    may_contain(body, &["\"thread\""]) && {
        let thread = rawjson::get(body, "thread");
        thread.exists() && thread.kind() != gjson::Kind::Null
    }
}

/// `newClaudeThreadNotFoundError`: request-scoped, so no credential is cooled or
/// rotated for it.
fn not_found() -> ExecError {
    ExecError::local(404, FailureScope::Request, NOT_FOUND)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reverse(alias: &str, name: &str) -> Arc<Reverse> {
        Arc::new([(alias.to_owned(), name.to_owned())].into_iter().collect())
    }

    /// Go TestClaudeOAuthToolAliasEvictionReturnsThreadNotFound: the oldest key goes
    /// once more than the limit are kept.
    #[test]
    fn oldest_turn_is_evicted_past_the_limit() {
        let store = Aliases::default();
        let create = Thread::of(r#"{"thread":{"type":"create"}}"#).unwrap();
        let aliases = reverse("alias", "Read");
        store.remember(&create, &aliases, "first-message");
        let continuation = r#"{"thread":{"type":"continue","previous_message_id":"first-message"}}"#;
        let lookup = |body: &str| store.for_continuation(&Thread::of(body).unwrap(), body);
        // The continuation shares the turn's map; nothing is copied.
        assert!(Arc::ptr_eq(&lookup(continuation).unwrap().unwrap(), &aliases));
        for i in 0..LIMIT {
            store.remember(&create, &Arc::default(), &format!("message-{i}"));
        }
        let error = lookup(continuation).unwrap().unwrap_err();
        assert_eq!((error.status, error.scope), (404, FailureScope::Request));
        assert_eq!(error.body, NOT_FOUND);
        // The newest turns are all still there.
        let last = format!(
            r#"{{"thread":{{"type":"continue","previous_message_id":"message-{}"}}}}"#,
            LIMIT - 1
        );
        assert!(lookup(&last).unwrap().is_ok());
    }

    #[test]
    fn declared_tools_and_other_threads_take_the_normal_path() {
        let store = Aliases::default();
        let lookup = |body: &str| Thread::of(body).and_then(|t| store.for_continuation(&t, body));
        for body in [
            r#"{"thread":{"type":"continue","previous_message_id":"m"},"tools":[{"name":"Read"}]}"#,
            r#"{"thread":{"type":"continue","previous_message_id":""}}"#,
            r#"{"thread":{"type":"create"}}"#,
            r#"{"messages":[{"role":"user","content":"\"thread\""}]}"#,
        ] {
            assert!(lookup(body).is_none(), "{body}");
        }
        // An empty tools array still needs the stored aliases, and an escaped key is
        // still the key.
        for body in [
            r#"{"thread":{"type":"continue","previous_message_id":"m"},"tools":[]}"#,
            r#"{"t\u0068read":{"type":"continue","previous_message_id":"m"}}"#,
        ] {
            assert!(lookup(body).is_some(), "{body}");
        }
        assert!(present(r#"{"t\u0068read":{}}"#) && !present(r#"{"thread":null}"#));
    }
}
