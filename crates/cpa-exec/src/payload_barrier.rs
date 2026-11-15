//! Go's `helps.NewPayloadFinalizer` (868e137): the payload rules are the final
//! semantic change to a request body. Every built-in mutation runs first; after the
//! barrier only framing, serialization, signing and read-only checks may run.

use cpa_core::config::Config;
use cpa_core::exec::ExecRequest;

/// The context a finalizer captures before the built-in mutations.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Barrier<'a> {
    /// Go `e.Identifier()`, or "" where Go still calls `ApplyPayloadConfigWithRequest`.
    pub executor: &'a str,
    pub model: &'a str,
    pub protocol: &'a str,
    pub root: &'a str,
    /// The unconfigured body defaults check for existing fields.
    pub original: &'a [u8],
}

impl Barrier<'_> {
    /// Applies the rules to `body`; a rebuilt attempt passes its unconfigured body.
    pub(crate) fn apply(&self, cfg: &Config, req: &ExecRequest, body: Vec<u8>) -> Vec<u8> {
        let rules = cpa_common::payload::Rules::of(cfg);
        // Go `PayloadRequestedModel`.
        let requested = match req.requested_model.trim() {
            "" => req.model.trim(),
            requested => requested,
        };
        cpa_common::payload::apply(
            &rules,
            &cpa_common::payload::Request {
                target_executor: self.executor,
                model: self.model,
                requested_model: requested,
                protocol: self.protocol,
                from_protocol: req.source_format.as_str(),
                root: self.root,
                original: self.original,
                request_path: &req.request_path,
                headers: Some(&req.headers),
            },
            body,
        )
    }
}
