//! Codex transports (Go: helps/utls_client.go `NewUtlsHTTPClient`, as the Codex executor
//! and Alpha Search use it; codex_websockets_connection.go `newProxyAwareWebsocketDialer`).
//!
//! - `https://chatgpt.com` (OAuth inference, Alpha Search, live calls): this proxy's own
//!   TLS client, with no browser or client fingerprint. Go dials chatgpt.com with uTLS
//!   `HelloChrome_Auto`; the pinned first-party Codex client builds a plain reqwest client
//!   on rustls (`codex-rs/http-client/Cargo.toml:16`, `client_builder.rs:391-432` at
//!   openai/codex@44ccf394573c), and neither source shows an endpoint that requires a
//!   Chrome ClientHello, so the impersonation is gone (#26, docs/codex/IDENTITY.md).
//!   Dialled directly or through the configured proxy, never an environment proxy. By
//!   default one connection per request, closed with the body, as Go does. With
//!   `oauth.providers.codex.chatgpt-keep-alive` (a cliproxy-rs addition, off by default)
//!   the client keeps up to [`CHATGPT_IDLE_PER_HOST`] idle connections per host and proxy
//!   for 90 seconds (docs/DIFFERENCES-FROM-GO.md).
//! - Every other origin (API-key base URLs, local mocks) and the Responses WebSocket:
//!   Go's standard transport from [`crate::proxy`].
//!
//! Both are cached per effective proxy (and, for chatgpt.com, per keep-alive setting and
//! handshake bound) in bounded 64-entry LRUs.

use std::sync::Mutex;

use crate::proxy::{CACHE_CAPACITY, GoClients, Hooks, Proxy};

/// Idle chatgpt.com connections kept per host, in each per-proxy client, when keep-alive
/// is on: Go's default `MaxIdleConnsPerHost`, as its Anthropic transport keeps. HTTP/2
/// multiplexes on one.
pub(crate) const CHATGPT_IDLE_PER_HOST: usize = 2;

/// How long an idle chatgpt.com connection is kept.
const CHATGPT_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

type ChatgptKey = (Proxy, bool, Option<std::time::Duration>);

pub(crate) struct Transport {
    standard: GoClients,
    hooks: Hooks,
    /// chatgpt.com clients by effective proxy, keep-alive setting and TLS handshake
    /// bound, most recent first.
    chatgpt: Mutex<Vec<(ChatgptKey, wreq::Client)>>,
}

impl Transport {
    pub fn new(hooks: Hooks) -> Self {
        Self {
            standard: GoClients::new(hooks.clone()),
            hooks,
            chatgpt: Mutex::default(),
        }
    }

    /// Unproxied standard requests use `client` (tests point it at local mocks).
    pub fn with_default(client: wreq::Client) -> Self {
        Self {
            standard: GoClients::with_default(client),
            hooks: Hooks::default(),
            chatgpt: Mutex::default(),
        }
    }

    /// The client for one request to `url` (`fallbackRoundTripper.RoundTrip`).
    /// `keep_alive` is `chatgpt-keep-alive` and `handshake` the TLS handshake bound
    /// ([`crate::tls::handshake_timeout`]); both only affect chatgpt.com.
    pub fn for_url(
        &self,
        url: &str,
        proxy: &Proxy,
        keep_alive: bool,
        handshake: Option<std::time::Duration>,
    ) -> wreq::Client {
        if is_chatgpt(url) {
            self.chatgpt(&(proxy.clone(), keep_alive, handshake))
        } else {
            self.standard.get(proxy)
        }
    }

    /// Go's standard transport (the WebSocket dialer honours environment proxies too).
    pub fn standard(&self, proxy: &Proxy) -> wreq::Client {
        self.standard.get(proxy)
    }

    fn chatgpt(&self, key: &ChatgptKey) -> wreq::Client {
        let (proxy, keep_alive, handshake) = key;
        let keep_alive = *keep_alive;
        let mut cache = self.chatgpt.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !keep_alive {
            // Keep-alive is off for this request, so it was turned off (or never on): retire
            // every pooled client. Requests in flight hold their own clone and finish; when
            // the last clone goes, its pool closes the idle sockets and its timer stops.
            cache.retain(|((_, pooled, _), _)| !pooled);
        }
        if let Some(i) = cache.iter().position(|(k, _)| k == key) {
            let entry = cache.remove(i);
            let client = entry.1.clone();
            cache.insert(0, entry);
            return client;
        }
        // Go logs an unusable proxy dialer and dials directly.
        let client = self
            .build_chatgpt(proxy, keep_alive, *handshake)
            .or_else(|_| self.build_chatgpt(&Proxy::Direct, keep_alive, *handshake))
            .expect("chatgpt.com TLS client");
        cache.insert(0, (key.clone(), client.clone()));
        cache.truncate(CACHE_CAPACITY);
        client
    }

    fn build_chatgpt(
        &self,
        proxy: &Proxy,
        keep_alive: bool,
        handshake: Option<std::time::Duration>,
    ) -> wreq::Result<wreq::Client> {
        // wreq's default TLS options: BoringSSL as this proxy configures it, ALPN h2 and
        // http/1.1, no emulation profile (codex_tls_tests.rs records the ClientHello).
        let builder =
            crate::tls::bound_handshake(wreq::Client::builder(), handshake).redirect(wreq::redirect::Policy::none());
        let builder = if keep_alive {
            // Opt-in: the same ClientHello and headers, without a handshake per request.
            builder
                .pool_max_idle_per_host(CHATGPT_IDLE_PER_HOST)
                .pool_idle_timeout(CHATGPT_IDLE_TIMEOUT)
        } else {
            // Go opens a dedicated connection per request and closes it with the body.
            builder.pool_max_idle_per_host(0)
        };
        proxy.apply(self.hooks.apply(builder), false)?.build()
    }
}

/// `req.URL.Scheme == "https" && strings.EqualFold(req.URL.Hostname(), "chatgpt.com")`.
pub(crate) fn is_chatgpt(url: &str) -> bool {
    url::Url::parse(url)
        .is_ok_and(|u| u.scheme() == "https" && u.host_str().is_some_and(|h| h.eq_ignore_ascii_case("chatgpt.com")))
}

#[cfg(test)]
#[path = "codex_tls_tests.rs"]
mod tests;
