# Compatibility with CLIProxyAPI

This fork keeps four built-in upstreams (Claude, Codex, Grok and Muse). Compatibility means functional compatibility: CLIProxyAPI's `config.yaml`, credential files, client routes and Management API keep working for the clients and operators that use them, and provider documentation, first-party clients and verified wire behaviour decide provider behaviour ([DIFFERENCES-FROM-GO.md](DIFFERENCES-FROM-GO.md#compatibility-policy)). Go's parser quirks and exact wire output are not goals, and no Go comparison or benchmark gates a change ([#45](https://github.com/zakstak/cliproxy-rs/issues/45)). The removed providers' contracts (Kimi, Devin, Gemini, Gemini Interactions, Vertex AI, AI Studio, Antigravity and OpenAI-compatible upstreams) are out of scope; checklist and status entries that name them are historical. The audit below is a historical inventory against CLIProxyAPI [v8.0.35](https://github.com/router-for-me/CLIProxyAPI/tree/ae3ee3be303fb07fb8173279923413005bf32177), commit `ae3ee3be`. Its source-derived checklist tracks 1,951 contracts and test suites.

Most existing fixtures were recorded from the previous reference, `6fecc6e` (v8.0.10). The Claude prompt-cache, prompt-shaping, continuation and error fixtures include selected [v8.0.27](https://github.com/router-for-me/CLIProxyAPI/tree/bf9341629ef2300ac9d846a2e80fe2756f63a671) changes. Those tests retain their provenance; changed Go suites are partial until their current assertions are verified. The [reference update](parity-audit/reference-v8.0.35.md) records additions, retired declarations and count changes. The [upstream inventory](parity-audit/upstream-v8.0.35.md) lists every intervening commit and its porting status. Deliberate differences, including stable caller-system placement, remain in [DIFFERENCES-FROM-GO.md](DIFFERENCES-FROM-GO.md).

## What works today

- Client APIs: `POST /v1/messages` and `/v1/messages/count_tokens` (Anthropic), `POST /v1/chat/completions`, `/v1/completions`, `/v1/responses` and `/v1/responses/compact` (OpenAI), `GET /v1/models`, and the Codex paths under `/backend-api/codex/`. Streaming (SSE) and non-streaming, with format translation between the three protocols.
- Images, video and speech: `POST /v1/images/generations` and `/v1/images/edits` through Codex accounts and xAI, the `/v1/videos` and `/openai/v1/videos` routes for xAI video, and `POST /v1/audio/speech` and `/v1/tts` for xAI text-to-speech (Grok TTS).
- WebSocket: the Responses WebSocket on `GET /v1/responses` and `GET /backend-api/codex/responses`, used by Codex clients, including response steering (`upstream.codex.response-steering`) and `response.interrupt`. An open Codex or xAI upstream socket survives a reload that changes only `requests.proxy-url`, and removing a credential ends only the sessions dialed for it.
- Realtime and live through a Codex account: `/v1/realtime` (WebSocket and WebRTC calls), `/v1/live`, call sidebands and local ephemeral keys (`/v1/realtime/client_secrets`). The WebRTC media relay is an optional build feature (`cargo build --release -p cliproxy --features cpa-server/media-relay`) and is not in the release binaries.
- Providers: Claude (OAuth and API keys), Codex (OAuth and API keys), Meta Muse (device sign-in and API keys) and xAI Grok (OAuth and API keys). The Gemini (`/v1beta/...`), Interactions and AI Studio relay (`/v1/ws`) routes answer 410 with migration guidance ([Removed providers](DIFFERENCES-FROM-GO.md#removed-providers)).
- Account sign-in from the command line or the dashboard: Claude, Codex (browser or device code), Meta and xAI.
- Routing: round-robin, weighted and fill-first selection, retries, cooldowns, session affinity, model aliases and exclusions, payload rules, per-credential and global proxies.
- The v8 Management API for configuration, credentials, OAuth sign-in, quota checks (`/requests/api-call`), usage counters, logs and model catalogs, plus the dashboard at `/management.html`.
- HTTPS on the main port (`server.tls`), logging in Go's format to stdout or a rotating `main.log` with Go's per-request access log lines, request log files with client and upstream sections, LAN discovery (`-discover` and the `server.discovery` advertisement), `.env` loading, and remote model catalog updates as in Go (`--local-model` turns them off).
- No native plugins: this fork removed Go CLIProxyAPI's plugin loader, plugin hooks, plugin store and their routes (#47). A config with a `plugins` section is refused, and the plugin routes answer `410` ([DIFFERENCES-FROM-GO.md](DIFFERENCES-FROM-GO.md), [MIGRATING-FROM-GO.md](MIGRATING-FROM-GO.md#native-plugins)). The audit counts the plugin rows of M6 and the plugin routes, settings and suites as covered deliberate differences.
- Home mode (`-home-jwt`): bootstrap, config updates (whose provider API keys register their models, as in Go; requests still take their credentials from Home), request dispatch, usage, process and request logs, in-flight reporting and shared KV state. Home-managed plugins are not: a Home config with a `plugins` section is refused.
- The `PGSTORE_*`, `OBJECTSTORE_*` and `GITSTORE_*` storage backends, and the Redis-protocol usage subscriber on the main port.
- Config and credential files are watched and reloaded without a restart. A plaintext management key is hashed on first start, as Go does.
- The separate, optional `observability.pprof` listener follows Go v8.0.35 for startup, reload and shutdown, including active requests continuing after a drain timeout without starting a replacement until another config publication. Temporary accept errors retain the listener and back off. Its index, command line and symbol routes are always available when enabled. CPU profiles require the non-default `cpa-server/pprof` build feature on Unix; published release binaries exclude it and return no resolved symbol names. Go runtime profiles return plain-text `501` answers, as approved in [DIFFERENCES-FROM-GO.md](DIFFERENCES-FROM-GO.md).

The dashboard says when the server lacks an endpoint instead of failing: actions it cannot do are disabled and named, and pages it cannot load say which route is missing. What is still missing is listed under [Upcoming features](../README.md#upcoming-features), and the deliberate differences from Go are in [DIFFERENCES-FROM-GO.md](DIFFERENCES-FROM-GO.md).

## Where the audit stood

<!-- parity-summary:start -->
Audit of 2026-10-20 (v0.2.6, d1261f2) against CLIProxyAPI v8.0.35 (`ae3ee3be`).

| Milestone | Items | Covered | Partial | Missing |
|---|---:|---:|---:|---:|
| M1 | 151 | 74 | 72 | 5 |
| M2 | 189 | 119 | 70 | 0 |
| M3 | 424 | 222 | 196 | 6 |
| M4 | 589 | 262 | 322 | 5 |
| M5 | 331 | 225 | 105 | 1 |
| M6 | 267 | 238 | 29 | 0 |
| All | 1951 | 1140 | 794 | 17 |
<!-- parity-summary:end -->

An item is covered when it is implemented and a Rust test or a fixture recorded from the Go server checks it. Partial means it works but not every case is pinned by a test, or only part of it is implemented; most partial items are Go test files whose behaviour other tests cover without porting each case. Missing means it is not implemented. A deliberate difference from Go, listed in [DIFFERENCES-FROM-GO.md](DIFFERENCES-FROM-GO.md), counts as covered.

## What each milestone covers

| Milestone | Scope |
|---|---|
| M1 | Claude: OAuth sign-in, Messages passthrough, request shaping, model catalogs and client keys |
| M2 | Translation between the OpenAI Chat, Completions and Responses formats and Claude (Gemini and Interactions were removed) |
| M3 | The other providers: Codex, xAI and Meta (Antigravity, AI Studio, Vertex, Gemini keys, Kimi, Devin and OpenAI-compatible upstreams were removed) |
| M4 | Routing and operations: account selection, cooldowns, retries, payload rules, aliases, proxies, logging and the shared config |
| M5 | The Management API, WebSockets, realtime and live voice, and access control |
| M6 | Plugins (removed in this fork, #47), the terminal UI, LAN discovery and Home (cluster) mode |

Missing items now also include model catalog sources (their `models.*` settings are read and validated but do not yet choose a source), new configuration contracts. Most v8.0.35 Management API changes are in: ETag and If-Match on v8 config, auth indexes in v8 api-keys, model definitions by channel, credential patches applied to the current record and the stale-reload guard. So are `GET /v1/models/{model}` and the watcher's handling of half-written credential files. The static model catalog, which model definitions by channel serve, is v8.0.35's. So is binding SIWC tokens to a Codex credential from the v8 Management API, including Go's post-binding 401 recovery but without its registration-epoch check. So is the `--siwc-bind` command, which binds them from the command line. So are requests through the bound tokens (`use_siwc`) and their refresh on a five-minute lead, beside the outer tokens, and the refresh of a Codex credential file under its lock, which adopts tokens another process rotated on disk; not yet Home-mode SIWC refresh. So are the historical v8 config aliases and Go v8.0.35's direct v8 config save, which keeps an explicit `null` and the values as written. So are Go's background refresh after a Codex 401 (which replaces a rejected token that is not yet due), the clearing of unauthorized cooldowns after a refresh that keeps live quota blocks, and the 96-hour Codex refresh lead. Not yet: the background refresh after other providers' 401s, and keeping requests off a rejected or expired token while its refresh is pending. Parts of Home remain incomplete. The terminal UI exists, though some Go cases remain unported.

The audit combines a source scan with saved route probes and manual judgments. Rerunning it reuses those saved probes and judgments. Live-provider compatibility needs separate testing. Counts can lag runtime wiring; a method implemented in a host library may still be unused by the server. The feature descriptions above follow the server's actual call paths.

## The audit files

Everything behind the numbers is in [`docs/parity-audit/`](parity-audit). The audit is reference material, not a gate ([parity-audit/README.md](parity-audit/README.md)):

- [`checklist.md`](parity-audit/checklist.md) lists every item with a link to the Go source it comes from.
- [`status.md`](parity-audit/status.md) gives each item its status, the evidence (the Rust code and tests that cover it) and a note.
- [`manual.tsv`](parity-audit/manual.tsv) holds the judgments made by reading code, with their evidence.
- [`audit.py`](parity-audit/audit.py) rebuilds `status.md`, the table above, and the README parity sentence from the same totals. [`probe.py`](parity-audit/probe.py) starts the binary and requests every route in the checklist with a private test configuration, to see which ones are routed; its results are in `routes.json`.
- [`verify_reference.py`](parity-audit/verify_reference.py) checks the frozen checklist's IDs and retained text, test declarations, routes (method and full path), config leaves, CLI flags, RPC methods and citation bounds against a read-only Go clone. It does not establish Rust behavior coverage.

To refresh all three, run `python3 docs/parity-audit/audit.py` from the repository root. No change has to. The [v0.2.6 rebaseline](parity-audit/rebaseline.md) records the review against `6fecc6e`; the [v8.0.35 reference update](parity-audit/reference-v8.0.35.md) records the new target's inventory and count changes.
