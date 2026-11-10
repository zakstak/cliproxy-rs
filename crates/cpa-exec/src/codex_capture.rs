//! Codex upstream wire capture (Go `helps.RecordAPI*` and `AppendAPI*` at the Codex call
//! sites). One [`Wire`] per attempt carries the call's capture sink and the selected
//! account's identity, so every physical request is recorded with Go's provider and auth
//! fields. Without an observer every call is a no-op. A native route binds the wire to the
//! authentication values its upstream request or socket carried ([`Wire::bind`]), and the
//! sink keeps everything it records scrubbed of them (#25).

use cpa_core::credential::Credential;
use cpa_core::exec::{CaptureEvent, CaptureSink, SentCredentials, UpstreamRequest};
use http::HeaderMap;

/// Go `http.Header` keys for a header map built like Go's: canonical spelling, values
/// in order, duplicates kept.
pub(crate) fn http_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
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

/// Go's WebSocket dial headers: canonical keys except those Go sets case-preserved
/// (`setHeaderCasePreserved`).
// ponytail: a client header Go copies verbatim when cloaking is disabled keeps gin's
// canonical spelling here, which is what Go's map holds after gin parsed it anyway.
fn websocket_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            let name = match name.as_str() {
                "session_id" => "session_id".to_owned(),
                "chatgpt-account-id" => "ChatGPT-Account-ID".to_owned(),
                other => crate::proxy::canonical_header(other),
            };
            (name, String::from_utf8_lossy(value.as_bytes()).into_owned())
        })
        .collect()
}

/// Go `helps.WebsocketUpgradeRequestURL`: `ws` to `http`, `wss` to `https`.
fn upgrade_url(url: &str) -> String {
    let url = url.trim();
    match url.split_once("://") {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("ws") => format!("http://{rest}"),
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("wss") => format!("https://{rest}"),
        _ => url.to_owned(),
    }
}

/// One attempt's capture: the call's sink and the selected account.
#[derive(Clone, Default)]
pub(crate) struct Wire {
    sink: CaptureSink,
    /// Go `EmitWebSocketResponseEvent`, called with every `AppendCodexAPIWebsocketResponse`.
    websocket: Option<cpa_core::exec::WebSocketEmitter>,
    auth_id: String,
    auth_label: String,
    auth_type: &'static str,
    auth_value: String,
    /// The values the request or socket being recorded carried ([`Wire::bind`]).
    sent: SentCredentials,
}

impl Wire {
    pub(crate) fn new(sink: &CaptureSink, credential: &Credential) -> Self {
        if !sink.enabled() {
            return Self::default();
        }
        let (auth_type, auth_value) = cpa_core::registry::dynamic::account_info(credential);
        Self {
            sink: sink.clone(),
            websocket: None,
            auth_id: credential.id.clone(),
            auth_label: credential.label.clone(),
            auth_type,
            auth_value,
            sent: SentCredentials::default(),
        }
    }

    /// From here on, the exchange recorded is the one whose upstream request or socket
    /// carried `sent`: a native request's own headers, then the socket that serves the
    /// turn (its handshake's values, which a reused socket keeps).
    pub(crate) fn bind(&mut self, sent: &SentCredentials) {
        self.sent = sent.clone();
    }

    fn record(&self, event: CaptureEvent<'_>) {
        self.sink.record_sent(event, &self.sent);
    }

    /// Reports the WebSocket frames this wire records to the attempt's observer.
    pub(crate) fn with_websocket(mut self, websocket: Option<cpa_core::exec::WebSocketEmitter>) -> Self {
        self.websocket = websocket;
        self
    }

    pub(crate) fn enabled(&self) -> bool {
        self.sink.enabled()
    }

    fn info<'a>(
        &'a self,
        url: &'a str,
        method: &'a str,
        headers: &'a [(String, String)],
        body: &'a [u8],
    ) -> UpstreamRequest<'a> {
        UpstreamRequest {
            url,
            method,
            headers,
            body,
            provider: "codex",
            auth_id: &self.auth_id,
            auth_label: &self.auth_label,
            auth_type: self.auth_type,
            auth_value: &self.auth_value,
        }
    }

    /// `RecordAPIRequest` just before a POST.
    pub(crate) fn request(&self, url: &str, headers: &HeaderMap, body: &[u8]) {
        if self.enabled() {
            let headers = http_pairs(headers);
            self.record(CaptureEvent::Request(self.info(url, "POST", &headers, body)));
        }
    }

    /// `RecordAPIResponseMetadata` once the response headers arrived.
    pub(crate) fn metadata(&self, status: u16, headers: &HeaderMap) {
        if self.enabled() {
            let headers = http_pairs(headers);
            self.record(CaptureEvent::ResponseMetadata(status, &headers));
        }
    }

    /// `AppendAPIResponseChunk`.
    pub(crate) fn chunk(&self, bytes: &[u8]) {
        self.record(CaptureEvent::ResponseChunk(bytes));
    }

    /// `RecordAPIResponseError`.
    pub(crate) fn error(&self, text: &str) {
        self.record(CaptureEvent::ResponseError(text));
    }

    /// `RecordAPIResponseError` with an executor error's Go text (`statusErr.Error()`
    /// is its body).
    pub(crate) fn exec_error(&self, error: &cpa_core::exec::ExecError) {
        if self.enabled() {
            self.error(&String::from_utf8_lossy(&error.body));
        }
    }

    /// `RecordAPIWebsocketRequest` for a dial or turn: Go's full request log.
    pub(crate) fn ws_request(&self, url: &str, headers: &HeaderMap, body: &[u8]) {
        if self.enabled() {
            let headers = websocket_pairs(headers);
            self.record(CaptureEvent::WebsocketRequest(self.info(
                url,
                "WEBSOCKET",
                &headers,
                body,
            )));
        }
    }

    /// `RecordAPIWebsocketRequest` for a frame the duplex writes: Go logs only the URL,
    /// method, body, provider and account ID there.
    pub(crate) fn ws_frame(&self, url: &str, body: &[u8]) {
        if self.enabled() {
            self.record(CaptureEvent::WebsocketRequest(UpstreamRequest {
                url,
                method: "WEBSOCKET",
                headers: &[],
                body,
                provider: "codex",
                auth_id: &self.auth_id,
                auth_label: "",
                auth_type: "",
                auth_value: "",
            }));
        }
    }

    /// `RecordAPIWebsocketHandshake` after a new dial.
    pub(crate) fn ws_handshake(&self, status: u16, headers: &HeaderMap) {
        if self.enabled() {
            let headers = http_pairs(headers);
            self.record(CaptureEvent::WebsocketHandshake(status, &headers));
        }
    }

    /// `AppendCodexAPIWebsocketResponse`.
    /// `AppendCodexAPIWebsocketResponse`, and the `EmitWebSocketResponseEvent`
    /// observation the caller awaits before it handles the message, if any.
    pub(crate) fn ws_response_observation(
        &self,
        payload: &str,
    ) -> Option<futures_util::future::BoxFuture<'static, ()>> {
        self.record(CaptureEvent::WebsocketResponse(payload.as_bytes()));
        let websocket = self.websocket.clone()?;
        let payload = payload.to_owned();
        Some(Box::pin(async move { websocket.emit(payload.as_bytes()).await }))
    }

    /// `RecordAPIWebsocketError` with an executor error's Go text, and its transport
    /// [`Diagnostic`](cpa_core::exec::Diagnostic) when it has one (the request log redacts
    /// that before writing it).
    pub(crate) fn ws_exec_error(&self, stage: &str, error: &cpa_core::exec::ExecError) {
        if self.enabled() {
            self.record(CaptureEvent::WebsocketError {
                stage,
                error: &String::from_utf8_lossy(&error.body),
                cause: error.diagnostic.as_ref().map(cpa_core::exec::Diagnostic::as_str),
            });
        }
    }

    /// `RecordAPIWebsocketUpgradeRejection`: a refused upgrade is an HTTP attempt, the GET
    /// to the HTTP form of the URL with `Connection` and `Upgrade` added when missing.
    pub(crate) fn upgrade_rejection(
        &self,
        ws_url: &str,
        request: &HeaderMap,
        status: u16,
        headers: &HeaderMap,
        body: &[u8],
    ) {
        if !self.enabled() {
            return;
        }
        let mut pairs = websocket_pairs(request);
        for (name, value) in [("Connection", "Upgrade"), ("Upgrade", "websocket")] {
            let present = request
                .get(name)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| !v.trim().is_empty());
            if !present {
                pairs.retain(|(n, _)| n != name);
                pairs.push((name.to_owned(), value.to_owned()));
            }
        }
        let url = upgrade_url(ws_url);
        self.record(CaptureEvent::Request(self.info(&url, "GET", &pairs, &[])));
        self.metadata(status, headers);
        self.chunk(body);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrade_url_maps_websocket_schemes() {
        assert_eq!(upgrade_url("wss://host/v1/responses"), "https://host/v1/responses");
        assert_eq!(upgrade_url("WS://host/x"), "http://host/x");
        assert_eq!(upgrade_url("https://host"), "https://host");
    }
}
