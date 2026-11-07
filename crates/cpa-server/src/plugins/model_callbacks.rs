//! Internal model execution for plugin callbacks. This uses the ordinary dispatcher,
//! not an HTTP loopback: no client key or synthetic keepalive reaches the upstream.

use std::sync::{Arc, Weak};

use cpa_core::exec::{Caller, Operation};
use cpa_core::format::Format;
use cpa_plugin::callbacks::RequestScope;
use cpa_plugin::client::CallbackError;
use cpa_plugin::hostmodel::{ModelExecutor, ModelOutput, ModelRequest, ModelResponse};
use cpa_plugin::streams::Chunk;
use futures_util::{StreamExt, future::BoxFuture};

use crate::Runtime;
use crate::dispatch::{Call, Done, Trace};

#[derive(Clone, Default)]
pub(crate) struct Context {
    pub skip_plugin: String,
    pub auth_id: String,
    pub proxy_url: String,
    pub scope: RequestScope,
}

tokio::task_local! {
    static CONTEXT: Context;
}

pub(crate) fn context() -> Option<Context> {
    CONTEXT.try_with(Clone::clone).ok()
}

/// The usage identity a request-bound callback scope carries: the call's caller, or the
/// parent scope's for a nested call, whose own caller is empty.
pub(crate) fn usage_identity(caller: Option<&Caller>, inherited: &RequestScope) -> (String, String) {
    match caller.filter(|c| !c.principal.is_empty() || !c.provider.is_empty()) {
        Some(c) => (c.principal.clone(), c.provider.clone()),
        None => (inherited.api_key.clone(), inherited.access_provider.clone()),
    }
}

/// Inside a nested `host.model.*` call, the parent request's usage identity (Go restores
/// the parent callback context). Usage records only: the nested call's caller stays
/// empty.
pub(crate) fn nested_usage_identity() -> Option<(String, String)> {
    CONTEXT
        .try_with(|c| (c.scope.api_key.clone(), c.scope.access_provider.clone()))
        .ok()
}

pub(crate) fn skip_plugin() -> String {
    CONTEXT.try_with(|c| c.skip_plugin.clone()).unwrap_or_default()
}

pub(crate) fn attach(rt: &Arc<Runtime>) {
    rt.plugins()
        .set_model_executor(Some(Arc::new(Executor(Arc::downgrade(rt)))));
}

struct Executor(Weak<Runtime>);

fn invalid(message: &str) -> CallbackError {
    CallbackError {
        message: message.into(),
        status: 400,
    }
}

impl ModelExecutor for Executor {
    fn execute(
        &self,
        mut req: ModelRequest,
        skip_plugin: String,
        scope: RequestScope,
    ) -> BoxFuture<'_, Result<ModelOutput, CallbackError>> {
        Box::pin(async move {
            let rt = self
                .0
                .upgrade()
                .ok_or_else(|| CallbackError::new("host model executor is unavailable"))?;
            let image = |s: &str| s.trim().eq_ignore_ascii_case("openai-image");
            let media = (image(&req.entry_protocol) || image(&req.exit_protocol)).then(|| {
                Arc::new(crate::dispatch::Media {
                    kind: crate::dispatch::MediaKind::Images,
                    pinned: None,
                    on_selected: None,
                    sse: false,
                    disallow_free: false,
                })
            });
            let parse = |s: &str| {
                if image(s) {
                    Some(Format::OpenAI)
                } else {
                    Format::parse(&cpa_plugin::executor::normalize_format(s))
                }
            };
            let entry = parse(&req.entry_protocol).ok_or_else(|| invalid("unsupported entry protocol"))?;
            let response = if req.exit_protocol.is_empty() {
                entry
            } else {
                parse(&req.exit_protocol).ok_or_else(|| invalid("unsupported exit protocol"))?
            };
            if req.headers.is_empty() {
                req.headers = scope.headers.clone();
            }
            if req.query.is_empty() {
                req.query = scope.query.clone();
            }
            let pairs: Vec<_> = req
                .query
                .iter()
                .flat_map(|(k, vs)| vs.iter().map(move |v| (k.as_str(), v.as_str())))
                .collect();
            let query = cpa_common::gostr::values_encode(&pairs);
            let trace = Trace::with_request_id(Some(scope.request_id.clone())).with_capture(scope.capture.clone());
            let cancel = scope.cancel.clone();
            let execution_session = scope.execution_session.clone();
            let request_path = if req.path.trim().is_empty() {
                scope.request_path.clone()
            } else {
                req.path
            };
            let context = Context {
                skip_plugin,
                auth_id: if req.auth_id.is_empty() {
                    scope.pinned_auth_id.clone().unwrap_or_default()
                } else {
                    req.auth_id
                },
                proxy_url: req.proxy_url,
                scope,
            };
            let call = Call {
                entry,
                response,
                operation: Operation::Generate,
                model: req.model,
                body: req.body,
                stream: req.stream,
                alt: Some(req.alt).filter(|s| !s.is_empty()),
                headers: super::interceptors::header_map(&req.headers),
                caller: Caller {
                    principal: String::new(),
                    source: "",
                    provider: String::new(),
                },
                forced_provider: Some(req.forced_provider).filter(|s| !s.is_empty()),
                selection_model: None,
                execution_session,
                request_path,
                peer: None,
                native: None,
                turn: None,
                media,
            };
            let result = CONTEXT
                .scope(
                    context,
                    super::execution::with_query(
                        &query,
                        crate::dispatch::run_with_bootstrap_retries(&rt, call, &trace),
                    ),
                )
                .await
                .map_err(|failure| CallbackError {
                    status: failure.status(),
                    message: failure.text(),
                })?;
            match result {
                Done::Buffered { headers, body } => Ok(ModelOutput::Buffered(ModelResponse {
                    status_code: 200,
                    headers: super::interceptors::go_header(&crate::respond::filter_upstream_headers(&headers)),
                    body,
                })),
                Done::Stream { headers, first, rest } => {
                    let chunks = futures_util::stream::iter(first.into_iter().map(Ok))
                        .chain(rest)
                        .map(|chunk| match chunk {
                            Ok(payload) => Chunk::data(payload),
                            Err(e) => Chunk::error(crate::classify::error_text(&e)),
                        });
                    // Cancellation must also drop the remaining dispatcher stream:
                    // that releases its credential lease and usage/lifecycle trackers.
                    let chunks = chunks.take_until(cancel.cancelled_owned()).boxed();
                    Ok(ModelOutput::Stream {
                        status_code: 200,
                        headers: super::interceptors::go_header(&crate::respond::filter_upstream_headers(&headers)),
                        chunks,
                    })
                }
            }
        })
    }
}
