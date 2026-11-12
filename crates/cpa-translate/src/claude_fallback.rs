//! Anthropic server-side refusal fallback on the translated routes (#71, Go v8.0.67
//! fa11a573 `ClaudeFallbackChain`), from the two signals Anthropic documents
//! (anthropic-sdk-python 5139cf2e, `types/beta/beta_fallback_block.py`,
//! `beta_fallback_message_iteration_usage.py`):
//!
//! - a `fallback` content block marks each hop where a model ran and declined, with
//!   `from`, `to` and `trigger`; it streams as a `content_block_start`/`stop` pair with
//!   no deltas, and the SDK relabels the message's `model` to the last block's
//!   `to.model`;
//! - a `fallback_message` entry in `usage.iterations` says a fallback model served the
//!   response, and names it; a turn served by a fallback model from the start has this
//!   entry and no block.
//!
//! Go also infers a hop when `message_start` names a model other than the requested
//! one. Anthropic documents no such signal (an alias resolving to a snapshot looks the
//! same), so that inference is not ported.

use cpa_common::json::{self as gj, Res};
use cpa_core::gostr::GoStr;

/// The ordered hops of one message, as Go's `{from, to, trigger}` metadata objects.
#[derive(Default)]
pub(crate) struct FallbackChain {
    entries: Vec<Vec<u8>>,
    /// A `fallback` block was seen; iterations then only name the serving model.
    explicit: bool,
}

/// Go `BuildClaudeFallbackMetadata`: the block's own `from`, `to` and `trigger` objects
/// (unknown fields kept), or the models given, with an optional boundary position.
fn metadata(block: Option<&Res<'_>>, from: &[u8], to: &[u8], boundary: Option<(&str, i64)>) -> Option<Vec<u8>> {
    let mut out = b"{}".to_vec();
    let mut any = false;
    for (key, fallback) in [("from", from), ("to", to)] {
        let own = block.map(|b| b.get(key)).filter(Res::is_object);
        if let Some(own) = own {
            gj::set_raw(&mut out, key, &own.raw);
            any = true;
        } else if !fallback.is_empty() {
            gj::set_str(&mut out, &format!("{key}.model"), fallback);
            any = true;
        }
    }
    if let Some(trigger) = block.map(|b| b.get("trigger")).filter(Res::is_object) {
        gj::set_raw(&mut out, "trigger", &trigger.raw);
        any = true;
    }
    if !any {
        return None;
    }
    if let Some((key, index)) = boundary {
        gj::set_int(&mut out, key, index);
    }
    Some(out)
}

fn model_at(value: &Res<'_>, path: &str) -> Vec<u8> {
    crate::common::trim_space(&value.get(path).bytes()).to_vec()
}

impl FallbackChain {
    /// A `fallback` content block. Returns the model that serves from here on, when
    /// the block names one, and whether a hop was recorded.
    pub(crate) fn block(
        &mut self,
        block: &Res<'_>,
        default_from: &[u8],
        boundary: Option<(&str, i64)>,
    ) -> (Option<Vec<u8>>, bool) {
        let to = model_at(block, "to.model");
        let mut from = model_at(block, "from.model");
        if from.is_empty() {
            from = default_from.to_vec();
        }
        if !self.explicit {
            // Blocks are authoritative: hops derived from iterations give way.
            self.entries.clear();
        }
        self.explicit = true;
        let recorded = match metadata(Some(block), &from, &to, boundary) {
            Some(entry) => {
                self.entries.push(entry);
                true
            }
            None => false,
        };
        ((!to.is_empty()).then_some(to), recorded)
    }

    /// The serving model a `fallback_message` iteration names and, when no block marked
    /// the hops, whether the hops the iterations imply added entries.
    ///
    /// Anthropic documents a `message` entry for each declined hop and for the serving
    /// model's earlier tool-loop iterations, and one `fallback_message` entry for the
    /// iteration that completed the response (`beta_fallback_message_iteration_usage.py`).
    /// So the hops run from the requested model through each other model that ran, in
    /// order, to the serving model. The serving model's own `message` entries and
    /// repeats of the model before are not hops, and the first model that ran is the
    /// requested one under whatever name Anthropic reports, so an alias resolving to
    /// another name never makes a hop. Each snapshot replaces the derived chain together
    /// with its serving model; identical snapshots do not announce the same hops again.
    pub(crate) fn iterations(
        &mut self,
        iterations: &Res<'_>,
        default_from: &[u8],
        boundary: Option<(&str, i64)>,
    ) -> (Option<Vec<u8>>, bool) {
        if !iterations.is_array() {
            return (None, false);
        }
        // (type is fallback_message, model) of each entry that names a model.
        let mut ran: Vec<(bool, Vec<u8>)> = vec![];
        iterations.each(|_, it| {
            let model = model_at(&it, "model");
            let kind = it.get("type").bytes();
            if !model.is_empty() && matches!(&*kind, b"message" | b"fallback_message") {
                ran.push((&*kind == b"fallback_message", model));
            }
            true
        });
        let Some(served) = ran.iter().rev().find(|(fallback, _)| *fallback).map(|(_, m)| m.clone()) else {
            return (None, false);
        };
        if self.explicit {
            return (Some(served), false);
        }
        let mut chain = vec![crate::common::trim_space(default_from).to_vec()];
        let mut previous: Option<&[u8]> = None;
        for (_, model) in &ran {
            if same_model(model, &served) {
                continue;
            }
            match previous {
                // The first model that ran is the requested one.
                None => {}
                Some(prev) if same_model(model, prev) => {}
                Some(_) => chain.push(model.clone()),
            }
            previous = Some(model);
        }
        if !chain.last().is_some_and(|last| same_model(last, &served)) {
            chain.push(served.clone());
        }
        let hops: Vec<Vec<u8>> = chain
            .windows(2)
            .filter_map(|pair| metadata(None, &pair[0], &pair[1], boundary))
            .collect();
        if hops == self.entries {
            return (Some(served), false);
        }
        self.entries = hops;
        (Some(served), true)
    }

    pub(crate) fn latest(&self) -> Option<&[u8]> {
        self.entries.last().map(Vec::as_slice)
    }

    pub(crate) fn all(&self) -> Option<Vec<u8>> {
        (!self.entries.is_empty()).then(|| gj::join(&self.entries))
    }

    /// Writes `{prefix}fallback` and `{prefix}fallbacks` when a hop was recorded.
    pub(crate) fn write(&self, out: &mut Vec<u8>, prefix: &str) {
        if let (Some(latest), Some(all)) = (self.latest(), self.all()) {
            gj::set_raw(out, &format!("{prefix}fallback"), latest);
            gj::set_raw(out, &format!("{prefix}fallbacks"), all);
        }
    }
}

/// Go `ClaudeModelSameOrSnapshot`: the same model, or one is the other's eight-digit
/// dated snapshot.
fn same_model(a: &[u8], b: &[u8]) -> bool {
    let lower = |m: &[u8]| String::from_utf8_lossy(crate::common::trim_space(m)).go_lower();
    let (a, b) = (lower(a), lower(b));
    let snapshot = |base: &str, candidate: &str| {
        candidate
            .strip_prefix(base)
            .and_then(|rest| rest.strip_prefix('-'))
            .is_some_and(|date| date.len() == 8 && date.bytes().all(|c| c.is_ascii_digit()))
    };
    !a.is_empty() && (a == b || snapshot(&a, &b) || snapshot(&b, &a))
}
