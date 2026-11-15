# Codex request identity and transport

This page defines identity headers and TLS policy for Codex upstream calls. The
[Codex wire contract](CONTRACT.md) is the authority, based on
[`openai/codex@44ccf394573c`](https://github.com/openai/codex/tree/44ccf394573cabb95e1f751a59f9d76f6f79faf1)
and OpenAI's documentation. [Differences from Go](../DIFFERENCES-FROM-GO.md) records
operator-visible changes.

## Header policy

A native exchange has Responses as both its source and response format (`codex` or
`openai-response`). Formats decide the identity policy. An OpenAI SDK posting to
`/v1/responses` keeps its own identity under the same rule as Codex CLI.

| Header | Rule |
| --- | --- |
| `Authorization` | Comes from the selected upstream credential: OAuth access token, API key, or SIWC token. The proxy client key never goes upstream. |
| `chatgpt-account-id` | Comes from the selected OAuth credential. A caller cannot select an account with this header. SIWC sends no account header. |
| `User-Agent` | The caller's value on a native exchange, otherwise `cliproxy-rs/<version>`. A missing native value uses the proxy's value. |
| `originator` | The caller's value on a native exchange. Otherwise `cliproxy-rs` for OAuth credentials, omitted for API keys. SIWC omits it. |
| Codex protocol metadata | Each call site's allowlist forwards client protocol headers. The [contract](CONTRACT.md#identity-and-headers) lists the source headers. |
| WebSocket `OpenAI-Beta` | The caller's value when it names `responses_websockets=`, otherwise `responses_websockets=2026-02-06`. |
| `x-codex-routing-hint` | Built from the final body's model and tier on OAuth credentials, subject to permitted operator headers. |

Cross-API translations, Images requests, and token grants identify the proxy.
Alpha Search keeps the caller's `User-Agent` and `originator`, with proxy defaults.
Live calls use the proxy's `User-Agent`; their allowlist can forward the caller's
`Originator`, with `cliproxy-rs` as the direct-session default. SIWC keeps the native
caller's `User-Agent` or uses the proxy's, without an originator or account header.

The first-party client builds `User-Agent` from its own originator, version, OS,
architecture, and terminal (`login/src/auth/default_client.rs:43-177` at the pin).
The proxy forwards that claim only for requests the caller formulated. It has no
setting that makes proxy-generated requests claim another client's identity.

## Operator settings

| Setting | Outcome |
| --- | --- |
| `upstream.codex.disable-codex-cloaking`, credential `codex_disable_cloaking` | No identity effect. `true` retains the native WebSocket session-header effect described in the [settings contract](CONTRACT.md#settings). |
| `oauth.providers.codex.header-defaults.user-agent`, legacy `codex-header-defaults.user-agent` | A non-empty value is refused at startup, reload, and Management API writes. `header-defaults.beta-features` remains supported. |
| Operator `User-Agent` or `Originator` headers on a Codex credential | The request fails with a credential-scoped 500 naming the header and credential, without its value. Nothing is sent upstream. |
| Operator `Authorization` or `chatgpt-account-id` copied from a request header or `$CPA-SESSION-ID` | Refused in the same way. Static operator values remain supported. |
| Any operator header copied from `$Authorization`, `$Proxy-Authorization`, `$X-Api-Key`, `$X-Goog-Api-Key`, or `$Cookie` | Refused to prevent forwarding the proxy client key or other client credentials. |
| `oauth.providers.codex.signin-with-chatgpt-identifier` | Names the agent on the SIWC consent screen only. It never sets request identity. |
| Catalog `override_header` | Identity and credential header names are skipped. Other permitted headers apply. |

## Transport

The pinned first-party HTTP client uses reqwest with rustls and aws-lc-rs
(`http-client/Cargo.toml:16`, `http-client/src/client_builder.rs:391-432`,
`utils/rustls-provider/src/lib.rs:10-31`). Its WebSocket client uses
`tokio-tungstenite` with rustls (`Cargo.toml:541-543`). Neither the source nor
OpenAI's documentation requires a Chrome ClientHello.

cliproxy-rs uses BoringSSL through wreq for `https://chatgpt.com`, without a browser
emulation profile. It does not copy the first-party rustls fingerprint. The local
`codex_tls_tests::chatgpt_clienthello_has_no_browser_fingerprint` regression checks
for browser-only extensions and preserves HTTP/2, HTTP/1.1, and TLS 1.3/1.2 support.

The chatgpt.com client uses an explicit configured proxy or connects directly. It
does not use environment proxies or follow redirects. Client caches are bounded
64-entry LRUs, keyed by effective proxy, keep-alive setting, and handshake bound.
`requests.tls-handshake-timeout-seconds` bounds connection setup.

Ordinary requests reuse up to two idle chatgpt.com connections per host and proxy
for 90 seconds, with HTTP/2 multiplexing. Set
`oauth.providers.codex.chatgpt-keep-alive: false` to use one connection per request.
Requests under client authority always use their own connection. See
[HTTP connection reuse](CONTRACT.md#http-connection-reuse).

Other origins and Responses WebSockets use the standard transport selected by
`codex_tls::Transport` and `proxy`.

## Implementation

- `codex_identity::Origin::of` classifies the exchange. `user_agent` and `originator`
  implement the identity policy.
- Codex entry points run `codex_identity::check_credential` and
  `codex_siwc::require_token` before sending.
- Operator headers go through `codex_request::custom_headers` and
  `codex_identity::permitted_attributes`.
- `codex_request::http_headers` and `ws_headers` take the identity origin.
- Codex HTTP calls select the client through `codex_tls::Transport`.

## Evidence limits

Local header and TLS tests establish the proxy's behaviour. They do not establish
that chatgpt.com or SIWC accepts the proxy's identity, ClientHello, or every model
under that identity. The pinned catalog's model-specific identity overrides are
not evidence that impersonation is required. Endpoint acceptance remains unknown
where the [contract](CONTRACT.md#endpoint-profiles) has no provider evidence.
