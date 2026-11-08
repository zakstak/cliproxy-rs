//! HTTP helpers of the executors that speak OpenAI wire formats (xAI, and the Codex Images
//! API): Go standard-transport clients per proxy, Go's plain `statusErr` and upstream
//! capture.

use bytes::Bytes;
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_core::exec::{CaptureEvent, CaptureSink, ExecError, ExecRequest, FailureScope, UpstreamRequest, UsageSink};
use http::HeaderMap;

use crate::proxy::{GoClients, Hooks, Proxy};

pub(crate) use crate::proxy::{GoHeaders, Upstream};

/// Go standard-transport clients keyed by effective proxy (crate::proxy, M4-0029).
pub(crate) struct Clients(GoClients);

impl Clients {
    pub(crate) fn new(default: wreq::Client) -> Self {
        Self(GoClients::with_default(default))
    }

    pub(crate) fn for_credential(&self, credential: &Credential, cfg: &Config) -> wreq::Client {
        self.0.get(&Proxy::effective(credential, cfg))
    }
}

impl Default for Clients {
    fn default() -> Self {
        Self(GoClients::new(Hooks::default()))
    }
}

/// Go `http.Client.Do` for one POST, redirects included.
pub(crate) async fn send(
    client: &wreq::Client,
    url: &str,
    headers: GoHeaders,
    body: Bytes,
) -> Result<Upstream, ExecError> {
    crate::proxy::send(client, url, headers, body, None).await
}

/// Go `io.ReadAll` of a response body; read errors fail the call.
pub(crate) async fn read_all(upstream: Upstream) -> Result<Bytes, ExecError> {
    crate::proxy::read_all(upstream.body, usize::MAX, false).await
}

/// Go `bufio.Scanner` lines with the executor's token limit.
pub(crate) fn lines(
    upstream: Upstream,
    max: usize,
) -> futures_util::stream::BoxStream<'static, Result<Bytes, ExecError>> {
    crate::proxy::lines(upstream.body, max)
}

/// `reporter.PublishFailure(err)`, called before any failure frame reaches the client (a
/// client-facing writer may stop at that frame and never take the error): Go's status
/// (none for a transport fault) and the error text, as the server records a failure.
pub(crate) fn publish_failure(usage: &UsageSink, error: &ExecError) {
    let status = if error.scope == FailureScope::Transport {
        0
    } else {
        error.status
    };
    let body = if error.body.is_empty() {
        format!("status {}", error.status)
    } else {
        String::from_utf8_lossy(&error.body).into_owned()
    };
    usage.publish_failure(status, &body);
}

// --- upstream request capture (helps/logging_helpers.go) --------------------------------

/// Response headers as Go's `http.Header`: canonical names, every value in order.
pub(crate) fn header_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            (
                crate::proxy::canonical_header(name.as_str()),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect()
}

/// One physical upstream HTTP attempt's capture: `RecordAPIRequest` just before the
/// send, then `RecordAPIResponseMetadata`, `AppendAPIResponseChunk` and
/// `RecordAPIResponseError` at Go's sites. A no-op without an observer.
#[derive(Clone, Default)]
pub(crate) struct Capture(CaptureSink);

impl Capture {
    /// `RecordAPIRequest` with the selected account's identity, as Go's call sites fill it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn request(
        req: &ExecRequest,
        credential: &Credential,
        provider: &str,
        url: &str,
        method: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Self {
        let sink = req.capture().clone();
        if sink.enabled() {
            let (auth_type, auth_value) = cpa_core::registry::dynamic::account_info(credential);
            sink.record(CaptureEvent::Request(UpstreamRequest {
                url,
                method,
                headers,
                body,
                provider,
                auth_id: &credential.id,
                auth_label: &credential.label,
                auth_type,
                auth_value: &auth_value,
            }));
        }
        Self(sink)
    }

    /// `RecordAPIResponseMetadata`.
    pub(crate) fn metadata(&self, status: u16, headers: &HeaderMap) {
        if self.0.enabled() {
            self.0
                .record(CaptureEvent::ResponseMetadata(status, &header_pairs(headers)));
        }
    }

    /// `AppendAPIResponseChunk`.
    pub(crate) fn chunk(&self, bytes: &[u8]) {
        self.0.record(CaptureEvent::ResponseChunk(bytes));
    }

    /// `RecordAPIResponseError`.
    pub(crate) fn error(&self, error: &ExecError) {
        if self.0.enabled() {
            self.0
                .record(CaptureEvent::ResponseError(&String::from_utf8_lossy(&error.body)));
        }
    }

    /// The outcome of the send: response metadata, or the transport error.
    pub(crate) fn sent(&self, result: &Result<Upstream, ExecError>) {
        match result {
            Ok(upstream) => self.metadata(upstream.status, &upstream.headers),
            Err(error) => self.error(error),
        }
    }

    /// A whole-body read: the body, or the read error.
    pub(crate) fn read(&self, result: &Result<Bytes, ExecError>) {
        match result {
            Ok(body) => self.chunk(body),
            Err(error) => self.error(error),
        }
    }
}

/// Go's plain `statusErr`: never credential-scoped, so a 429 cools only the model
/// (`IsCredentialScoped` is false); other failures by status.
pub(crate) fn scope_for(status: u16) -> FailureScope {
    match status {
        429 => FailureScope::Model,
        401 | 402 | 403 | 408 | 500.. => FailureScope::Credential,
        _ => FailureScope::Request,
    }
}

/// A plain Go error (no status code): no status, scope or retry semantics, which the
/// server models as a transport fault; Go's handler answers 500 with its text.
pub(crate) fn plain_err(message: impl Into<String>) -> ExecError {
    ExecError::local(500, FailureScope::Transport, message)
}

/// `statusErr{code, msg}`: an empty message reads `status N`.
pub(crate) fn status_err(status: u16, message: impl Into<String>) -> ExecError {
    let message = message.into();
    let message = if message.is_empty() {
        format!("status {status}")
    } else {
        message
    };
    ExecError::local(status, scope_for(status), message)
}
