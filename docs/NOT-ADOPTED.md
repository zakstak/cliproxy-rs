# Features this fork does not adopt

These Go v8.0.67 features were researched under the
[selective upstream tracker](https://github.com/zakstak/cliproxy-rs/issues/66) and declined.
Each section names the Go feature, the current Rust behavior, the reason, and what would
reopen the decision. Paths refer to this repository.

Two rules decide most of these. The proxy stays transparent to providers: beyond the
rewrites it already carries, such as Claude cloaking
(`crates/cpa-exec/src/claude/cloak.rs`), it adds no new tools, instructions or replayed
provider content to requests. And a feature needs a retained client or deployment that
uses it.

## Claude web-search result replay (C6, #74)

Go caches Claude web-search results and replays them, encrypted, into later requests. The
v8.0.67 fixes scope that cache to the upstream credential and drop citations whose results
are missing.

Rust has no web-search result cache. Its only web-search edit removes empty
`allowed_domains` and `blocked_domains` lists from `web_search_*` tools
(`sanitize_for_upstream` in `crates/cpa-exec/src/claude.rs`). The fixes have nothing to
apply to.

**Reason:** adding the cache would make the proxy send provider content from an earlier
response in a later request. That breaks provider transparency.

**Reconsider when:** a retained client fails without replayed search results, and the
provider documents replay as the client's job that a proxy may take on.

## Persistent Claude session date pins (C7, #75)

Go stores the date pinned to each cloaked Claude session in a bbolt key-value store, so
the pin survives a restart.

Rust keeps the pin in the session's in-memory continuity entry (`pin_date` in
`crates/cpa-exec/src/claude/session.rs`). The entry expires an hour after the session's
last request and is evicted at 4,096 entries. Only cloaked requests read the pin
(`crates/cpa-exec/src/claude.rs`, the `pin_date` call inside the `cloak` branch).

**Reason:** a lost pin matters only for a cloaked session that started on an earlier date
and continues after a restart. That session loses its cached prefix once. Persistence
would add a store, retention and corruption handling, and rules for several processes, to
save that one cache miss.

**Reconsider when:** restarts during long cloaked sessions are frequent enough that the
cache misses show up in usage data.

## Muse reminder tool (M3, #83)

Go adds a Muse reminder decision tool to every Muse request that carries tools.

Rust's Meta executor edits tools the caller sent, for example removing
`search_content_types` from `web_search` tools, which Meta rejects. It adds no tools and
has no Muse reminder tool (`crates/cpa-exec/src/meta.rs`).

**Reason:** the proxy would add a tool the caller did not send. That breaks provider
transparency. Go cites one Muse 1.4.12 capture; no checked official source says the tool
is required.

**Reconsider when:** Meta documents the tool as required, or the official Muse Code client
is shown to add it to its own requests. In that case the client sends it, and the proxy
still should not.

## Durable Codex refresh claims and receipts (A5, #93)

Go writes a durable claim before it sends a Codex token refresh and a receipt after, so
that after a crash no process reuses a refresh token whose outcome is unknown. Its claims
never expire, and its receipts contain the new credentials.

Rust has no durable refresh journal. On one node, a Codex refresh runs as one transaction
under a per-credential preparation lock and the credential file's `<file>.lock`, reads the
file again under that lock, and adopts tokens another process already rotated
(`refresh_codex_file` in `crates/cpa-server/src/runtime.rs`). Shutdown waits for a grant
in flight to be written. Claude refresh is single-flighted per token endpoint and refresh
token (`crates/cpa-exec/src/oauth.rs`). A4
([#92](https://github.com/zakstak/cliproxy-rs/issues/92)) made renewal and quarantine
correct. A crash between the refresh request and the file write can still lose the new
tokens.

**Reason:** a real guarantee needs a new persistence protocol across the file, Git, object
and PostgreSQL stores, with operator recovery for stuck claims and protected storage for
receipts. That is extra-large work, and Go's version keeps credentials in receipts and
claims that never expire. The locking above covers single-node deployments.

**Reconsider when:** this fork runs several nodes against one shared remote store, or a
lost refresh after a crash is observed.

## Weighted routing fairness with session affinity (R3, #96)

Go's weighted routing counts requests that session affinity pins, so each account's share
of all requests follows its weight.

Rust's `weighted-round-robin` advances its counters only on requests it picks itself.
A request bound to an account by session affinity skips the pick (`pick_legacy` and the
LCP branch in `crates/cpa-server/src/scheduler.rs`). With affinity on, weights split new
conversations, not requests. The default strategy is `soonest-reset`
(`crates/cpa-server/src/scheduler.rs`, [Several accounts](MULTI-ACCOUNT.md)).

**Reason:** no deployment of this fork uses weighted routing; they use the
`soonest-reset` default. Request-level fairness under affinity needs its own state model
and starvation bounds.

**Reconsider when:** a deployment needs a request-level weighted split with session
affinity on.

## Full-chain compression (P1, #106)

Go compresses Codex upstream requests with zstd, compresses downstream HTTP responses,
negotiates downstream WebSocket compression, and resends a request uncompressed after a
400 or 415.

Rust already decodes compressed traffic on these paths: zstd client request bodies on the
OpenAI-format routes (`decode_body` in `crates/cpa-server/src/openai.rs`), compressed upstream responses
(`crates/cpa-exec/src/upstream.rs`) and permessage-deflate frames on the upstream Codex
socket (`crates/cpa-exec/src/codex_ws_deflate.rs`). It sends upstream requests and
downstream responses uncompressed (`hop_by_hop` in `crates/cpa-server/src/respond.rs`
drops `Content-Encoding`). A zstd body from the first-party Codex client
([Codex contract](codex/CONTRACT.md#transport-matrix)) is decoded and sent upstream
uncompressed.

**Reason:** this fork runs on LAN and tailnet gateways, where bandwidth is not the limit.
Encoders add memory per open stream. The resend after a 400 or 415 can run a request twice
when the provider did execute the first one.

**Reconsider when:** a deployment serves clients over a slow link and measures a benefit,
or a provider requires compressed requests.

## Per-account connection pools (P2, #107)

Go can give each API key its own upstream connection pool. It later disabled default
connection reuse for Claude (Go `2c5b01f4`).

Rust shares clients per effective proxy in a bounded 64-entry LRU, not per credential
(`crates/cpa-exec/src/proxy.rs`, `crates/cpa-exec/src/tls.rs`). Codex reuses chatgpt.com
connections by default since [#151](https://github.com/zakstak/cliproxy-rs/pull/151)
(`CHATGPT_KEEP_ALIVE_DEFAULT` in `crates/cpa-exec/src/codex_request.rs`).

**Reason:** no measured benefit. One pool per credential grows connections and file
descriptors with every rotated or added account, and needs retirement rules for each.

**Reconsider when:** a measurement shows a handshake or latency cost that per-credential
pools remove, with memory and descriptor bounds.

## Home cluster rebalance and latency probing (D2, #109)

Go adds planned rebalance handoff between Home nodes, latency probes, reused-turn usage
and release fencing.

Rust's Home mode is a node that takes its config and credential dispatch from a Home
control plane (`crates/cpa-home/src/lib.rs`). It has none of these coordination messages.

**Reason:** this is an extra-large distributed-systems feature that needs a mixed-version
protocol design. No deployment of this fork runs a multi-node Home cluster.

**Reconsider when:** this fork runs as several Home nodes and rebalancing is needed.

## Draft gateway discovery and mDNS cache (D3, #110)

Go replaces its discovery TXT schema with the nine keys of the individual Internet-Draft
`draft-su-ai-gateway-discovery-00`, rewrites its mDNS browser with per-interface bounded
caches to resolve split responses, and deprecates `advertise-management` (the draft never
advertises management endpoints).

Rust keeps Go v8.0.35's discovery: the `discover` command and an advertisement that is off
unless `server.discovery.enabled` is set. `advertise-management` defaults to `false`
(`crates/cliproxy/src/discovery/mod.rs`).

**Reason:** this fork's deployments do not use LAN discovery. The draft is a `-00`
individual draft that can still change, and switching schemas changes what peers and
`discover` read. Management is already not advertised by default.

**Reconsider when:** a deployment uses LAN discovery, or the draft is adopted by a working
group.

## Named WebSocket lanes, `stream_id` (FP6, #120)

The OpenAI Responses WebSocket guide documents concurrent named lanes on one socket,
selected by `stream_id`.

Rust has no `stream_id` state. A Codex socket has one queue of pending creates, and a
`response.created` starts the oldest one (`crates/cpa-exec/src/codex_duplex.rs`). The
[Codex contract](codex/CONTRACT.md#transport-matrix) lists lanes as not used by the pinned
client.

**Reason:** current official codex-rs does not send `stream_id`, so no retained client
needs it. Correct support needs pending queues, active responses, interrupts, failures and
usage per lane: an extra-large state model.

**Reconsider when:** a first-party client sends `stream_id`.
