# Codex request identity and transport

This document defines who a Codex upstream request says it comes from, which headers the
caller and the selected credential control, and how the proxy connects to `chatgpt.com`.
It is the deliverable of [#26](https://github.com/zakstak/cliproxy-rs/issues/26), under
tracker [#20](https://github.com/zakstak/cliproxy-rs/issues/20).

The authority is the first-party Codex client at
[`openai/codex@44ccf394573c`](https://github.com/openai/codex/tree/44ccf394573cabb95e1f751a59f9d76f6f79faf1)
and the Codex wire contract in `docs/codex/CONTRACT.md` (#21, PR #31, reviewed at
`389556556ec5`), section "Identity and headers". Go v8.0.35 is not the authority for
Codex identity. Go recordings stay as historical regression evidence, and every test that
compares against one encodes the differences below explicitly. Line references point at
the pinned first-party commit.

## Header policy

| Header | Set by | Rule |
| --- | --- | --- |
| `Authorization` | The selected credential | Its OAuth access token, API key or Sign in with ChatGPT token. The caller's `Authorization` (the proxy client key) never goes upstream. |
| `chatgpt-account-id` | The selected credential | Its `account_id`, on OAuth credentials. A caller value never goes upstream, so a caller cannot select or switch accounts. |
| `User-Agent` | The caller, or the proxy | The caller's own value on a native exchange; otherwise `cliproxy-rs/<version>`. |
| `originator` | The caller, or the proxy | The caller's own value on a native exchange; otherwise `cliproxy-rs` on OAuth credentials and none on API keys, the same presence as Go. |
| Codex protocol metadata | The caller | Forwarded by each call site's existing allowlist: `x-codex-beta-features`, `x-codex-turn-state`, `x-codex-turn-metadata`, `x-client-request-id`, `x-codex-window-id`, `session-id`, `thread-id`, `version`, `x-openai-internal-codex-responses-lite`, `x-responsesapi-include-timing-metrics` (WebSocket) and the live allowlist. #26 does not change these lists. |
| `OpenAI-Beta` on the WebSocket handshake | The caller, or the proxy | Unchanged: the caller's value when it names `responses_websockets=`, else `responses_websockets=2026-02-06` (`core/src/client.rs:176`, 1582-1585). |
| `x-codex-routing-hint` | The proxy, or an operator header | Unchanged: built from the final body's model and tier on OAuth credentials. |

A *native exchange* is a request whose source and response formats are both Responses
(`codex` or `openai-response`). The formats decide, not what the caller claims to be:
an OpenAI SDK posting `/v1/responses` keeps its own `User-Agent`, exactly as a Codex
client does. Everything else is formulated by the proxy and carries the proxy's identity:
cross-API translations (Chat Completions, Claude and Gemini into Codex), the Images API,
live calls and token grants.

`crates/cpa-exec/src/codex_identity.rs` implements the policy, and every Codex call site
goes through it.

### Why these values

The first-party client builds `User-Agent` as
`{originator}/{version} ({os} {os version}; {arch}) {terminal}`
(`login/src/auth/default_client.rs:152-177`) and sends `originator` from
`DEFAULT_ORIGINATOR = "codex_cli_rs"` or its override (lines 43-118). Both describe the
program and machine that built the request. A proxy that relays a native request leaves
them as the caller sent them. A request the proxy builds itself names the proxy, because
claiming `codex-tui` on macOS 26.5.2 with iTerm 3.6.11 is false on every host. The pinned
client also treats `originator` as an identity claim: `is_first_party_originator`
(lines 135-141) recognises only Codex's own values.

### Operator settings that can no longer set identity

| Setting | Before (Go v8.0.35) | Now |
| --- | --- | --- |
| Cloaking, default on (`upstream.codex.disable-codex-cloaking: false`) | Overwrote `User-Agent` and `originator` on every OAuth and API-key request with `codex-tui/0.154.0 (Mac OS 26.5.2; arm64) iTerm.app/3.6.11 (codex-tui; 0.154.0)` and `codex-tui` | Removed. No value of the setting, and no `codex_disable_cloaking` credential attribute, changes the identity. |
| `upstream.codex.disable-codex-cloaking: true` | Stopped the overwrite, and made a native WebSocket handshake carry the caller's own session headers | Still accepted. It now only keeps the session-header effect, which belongs to the WebSocket migration ([#24](https://github.com/zakstak/cliproxy-rs/issues/24)). |
| `oauth.providers.codex.header-defaults.user-agent` (legacy `codex-header-defaults.user-agent`) | Set the `User-Agent` of OAuth requests ahead of the caller's | Refused when non-empty: at startup, on a reload (the running config stays) and on a Management API write. `header-defaults.beta-features` stays. |
| An operator header `User-Agent` or `Originator` on a Codex credential (config `headers`, an auth file's `headers`, Home or a plugin store) | Applied over the identity | The Codex request fails with a credential-scoped 500 that names the header and the credential, never the value. Nothing goes upstream. |
| An operator header `Authorization` or `chatgpt-account-id` whose value comes from the request (`$Header` or `$CPA-SESSION-ID`) | Let the caller choose the upstream credential or account | Refused the same way. A static operator value still applies. |
| An operator header whose value copies a client credential header (`$Authorization`, `$Proxy-Authorization`, `$X-Api-Key`, `$X-Goog-Api-Key` or `$Cookie`) | Sent the proxy client key upstream under another name | Refused the same way. Copying any other client header still works. |
| `oauth.providers.codex.signin-with-chatgpt-identifier` | Was the consent screen's agent name (`agent_name_hint` in the authorize URL) and the Sign in with ChatGPT request `User-Agent` | Keeps only the consent-screen label. It never reaches a request header, so it cannot claim another client. |
| A model catalog `override_header` naming `User-Agent`, `Originator`, `Authorization` or `chatgpt-account-id` | Applied. The pinned catalog gives `gpt-5.6-luna` the `codex-tui` macOS identity | Those names are skipped. Other override headers still apply. |

No setting replaces cloaking: the proxy has no opt-in to claim another client's identity,
and it sends no probe or keepalive request to test whether an identity is accepted.

### Call sites

| Codex-owned call | Identity |
| --- | --- |
| Responses over HTTP (`execute`, buffered, streamed, compact) | Caller on a native exchange, else proxy |
| Responses WebSocket handshake | Caller on a native exchange, else proxy |
| Images API, direct and through the image tool | Proxy |
| Alpha Search | Caller's `User-Agent` and `originator`, else proxy (Go forced `originator: codex_cli_rs` over the caller's) |
| Live calls, sidebands and hangups (HTTP and WebSocket) | Proxy `User-Agent` (Go sent `Go-http-client/1.1`); the caller's `Originator` from the live allowlist, else `cliproxy-rs` on direct sessions (Go claimed `Codex Desktop`) |
| OAuth token exchange, refresh and device flow; Sign in with ChatGPT token grants, refresh and binding | Proxy `User-Agent` (Go sent `Go-http-client/1.1`) |
| Sign in with ChatGPT Responses requests (HTTP and WebSocket) | Caller's `User-Agent` on a native exchange, else proxy; no `originator` and no account header, as before. Go sent `<signin-with-chatgpt-identifier> (<GOOS>; <GOARCH>)`, an operator-chosen identity (default `CLIProxyAPI`). |

## Transport

### First-party construction

The pinned client sends every Codex HTTP request through `codex-http-client`, a reqwest
client on rustls:

- `http-client/Cargo.toml:16` enables reqwest's `rustls-tls` and `rustls-tls-native-roots`.
  The workspace pins `reqwest = "0.12"` with default features, so HTTP/2 is available, and
  `rustls = "0.23.46"` with the `aws_lc_rs` provider (`Cargo.toml:473-480`).
- `http-client/src/client_builder.rs:391-432` builds a plain `reqwest::Client::builder()`:
  `use_rustls_tls()`, optional custom roots or client identity, a redirect policy, an
  optional connect timeout and a Cloudflare cookie store. It sets no cipher order, no
  extension order, no GREASE and no fingerprint profile.
- `utils/rustls-provider/src/lib.rs:10-31` installs aws-lc-rs as the process provider.
- The WebSocket transport is `tokio-tungstenite 0.28` with `rustls-tls-native-roots`
  (`Cargo.toml:541-543`).

Neither the first-party source nor OpenAI's documentation shows an endpoint that
requires a particular ClientHello.

### What changed

| | Go v8.0.35 | cliproxy-rs before #26 | cliproxy-rs now | First-party Codex (from source, not measured) |
| --- | --- | --- | --- | --- |
| TLS library for `https://chatgpt.com` | uTLS `HelloChrome_Auto` (Chrome 133) | BoringSSL configured as Chrome 133 | BoringSSL with wreq's defaults, no emulation profile | rustls 0.23 on aws-lc-rs |
| GREASE, permuted extensions, ALPS, certificate compression, ECH GREASE | Yes | Yes | None | None in the configuration reqwest builds (rustls has no GREASE or ALPS; certificate compression and ECH are opt-in features) |
| ALPN | `h2`, `http/1.1` | `h2`, `http/1.1` | `h2`, `http/1.1` | `h2`, `http/1.1` (reqwest default with HTTP/2) |
| TLS versions | 1.3, 1.2 | 1.3, 1.2 | 1.3, 1.2 | 1.3, 1.2 (rustls default) |
| WebSocket | gorilla, Go standard TLS | wreq standard client | wreq standard client (unchanged) | tokio-tungstenite, rustls |
| Other Codex origins (API-key base URLs, `api.openai.com`, mocks) | Go standard transport | wreq standard client | wreq standard client (unchanged) | reqwest, rustls |

`crates/cpa-exec/src/codex_tls_tests.rs` `chatgpt_clienthello_has_no_browser_fingerprint`
captures the production client's ClientHello on a local listener on every test run and
fails if GREASE, ALPS, certificate compression, ECH GREASE or a varying extension order
comes back, or if ALPN or TLS 1.3/1.2 is lost. The ClientHello recorded at `HEAD` of this
change:

```text
cipher suites (wire order)  1301 1302 1303 c02b c02f c02c c030 cca9 cca8 c009 c013 c00a
                            c014 c023 c024 c028 009c 009d 002f 003c 0035 003d 0033 0039
                            0067 006b 009e 009f
extensions (wire order)     0000 server_name, 0017 extended_master_secret,
                            ff01 renegotiation_info, 000a supported_groups,
                            000b ec_point_formats, 0023 session_ticket, 0010 ALPN (h2, http/1.1),
                            000d signature_algorithms, 0033 key_share, 002d psk_key_exchange_modes,
                            002b supported_versions (1.3, 1.2)
key shares                  11ec X25519MLKEM768 (1216 bytes), 001d X25519 (32 bytes)
order                       identical on every connection
```

The proxy does not copy the rustls ClientHello either. Reproducing another client's
fingerprint is the impersonation #26 removes; the proxy uses its own TLS stack as
configured.

### What stays

These behaviours keep their existing tests, which use local TLS and proxy mocks only:

- The `chatgpt.com` client is still separate from the standard client and is cached per
  effective proxy, keep-alive setting and TLS handshake bound, in a 64-entry LRU.
- It dials directly or through the configured proxy, never an environment proxy, and
  follows no redirect itself.
- `requests.tls-handshake-timeout-seconds` still bounds its connection setup.
- `oauth.providers.codex.chatgpt-keep-alive` still selects between one connection per
  request (the default, as Go) and up to two idle connections per host and proxy for 90
  seconds, with HTTP/2 multiplexing, pools isolated per proxy, and stale connections
  replaced.
- Custom `base_url` endpoints, credential and global proxies, request timeouts and the
  WebSocket dialer are unchanged.

## Interfaces for later migrations

#23, #24 and #25 move complete native paths. They reuse this policy instead of building
headers of their own:

- `codex_identity::Origin::of(&ExecRequest)` classifies a request. A migrated native path
  passes `Origin::Caller`, and a translation passes `Origin::Proxy`.
- `codex_identity::user_agent` and `codex_identity::originator` give the two identity
  headers; `USER_AGENT` and `ORIGINATOR` are the proxy's own values.
- `codex_identity::check_credential` runs at each Codex entry point, next to
  `codex_siwc::require_token`. A new entry point calls both.
- `codex_request::custom_headers` applies operator headers through
  `codex_identity::permitted_attributes`. Code that applies `header:` attributes on a
  Codex path uses it, never `cpa_common::headers::custom_headers` directly.
- `codex_request::http_headers` and `codex_request::ws_headers` take the `Origin`.
- `codex_tls::Transport::for_url` picks the `chatgpt.com` client. A new Codex HTTP caller
  uses it.

Ownership after #26:

| Area | Owner |
| --- | --- |
| Identity headers, credential and account headers, operator header refusal, the `chatgpt.com` TLS client | #26 (this document) |
| Which Codex protocol headers each native path forwards, including `x-codex-installation-id`-style additions | #23 (HTTP), #24 (WebSocket), #25 |
| Session headers on the WebSocket handshake and the remaining `disable-codex-cloaking` effect | #24 |
| Translated-route identity | #27 keeps `Origin::Proxy` |

## Unproven gates

This change was tested only against local mocks. Nothing here was sent to a real account.

- Whether `chatgpt.com/backend-api/codex` accepts requests that carry
  `User-Agent: cliproxy-rs/<version>` and `originator: cliproxy-rs` is not measured. A
  native caller's own identity passes through unchanged, so Codex clients are unaffected
  by this question.
- Whether `chatgpt.com` (including its Cloudflare front) accepts the proxy's BoringSSL
  ClientHello without a browser fingerprint is not measured. The first-party client's
  plain rustls ClientHello is evidence that a non-browser fingerprint is a supported
  configuration, not proof of acceptance for this one.
- Whether the backend serves `gpt-5.6-luna` to a request whose originator is not
  `codex-tui` is unknown. The pinned catalog's override suggests it might be gated, but
  no first-party source requires it. A native caller's own originator still passes.
- Whether the Sign in with ChatGPT endpoint accepts `cliproxy-rs/<version>` (or a native
  caller's own `User-Agent`) in place of Go's `<identifier> (<os>; <arch>)` is not
  measured. No source shows that the endpoint requires the identifier in the
  `User-Agent`; the binding's `agent_name_hint` still carries it. Every Responses
  behaviour on that profile is unknown in the contract.
- The live media relay's forward-proxy `CONNECT` line (`crates/cpa-server/src/realtime/dialer.rs`)
  still says `Go-http-client/1.1`. It goes to the operator's proxy, not to OpenAI.
- `docs/codex/CONTRACT.md` lists the `codex-tui` identity and the Chrome profile as
  deviations owned by #26. After PR #31 lands, its "Identity and headers" and "Known
  deviations at the baseline" sections need to point here.
