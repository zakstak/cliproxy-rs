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

    /// Go `RecordIterations`: the serving model a `fallback_message` iteration names,
    /// and, when no block marked the hops, whether the hops it implies added entries.
    pub(crate) fn iterations(
        &mut self,
        iterations: &Res<'_>,
        default_from: &[u8],
        boundary: Option<(&str, i64)>,
    ) -> (Option<Vec<u8>>, bool) {
        if !iterations.is_array() {
            return (None, false);
        }
        let mut from = crate::common::trim_space(default_from).to_vec();
        let mut served = None;
        let mut hops = vec![];
        iterations.each(|_, it| {
            let model = model_at(&it, "model");
            if model.is_empty() {
                return true;
            }
            match &*it.get("type").bytes() {
                b"message" => from = model,
                b"fallback_message" => {
                    if !self.explicit
                        && let Some(hop) = metadata(None, &from, &model, boundary)
                    {
                        hops.push(hop);
                    }
                    from = model.clone();
                    served = Some(model);
                }
                _ => {}
            }
            true
        });
        if served.is_none() || self.explicit || hops.len() <= self.entries.len() {
            return (served, false);
        }
        self.entries = hops;
        (served, true)
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
