# Selective comparison ledger: Go v8.0.67

Started 2026-11-12 for [issue #67](https://github.com/zakstak/cliproxy-rs/issues/67) (H1) of the
[selective upstream tracker](https://github.com/zakstak/cliproxy-rs/issues/66).

This ledger sits beside the frozen reference. It does not move it. The non-Codex default reference
is still Go **v8.0.35** (`ae3ee3be303fb07fb8173279923413005bf32177`, [checklist](checklist.md)). The
historical Claude wire recording is still Go **v8.0.10** (`6fecc6e5567912661654a4eaf9b8f5436facd1c2`).
Go **v8.0.67** (`8bba7a77c6bcbfd993516d26606bfecaa4ff49d0`) is a separate comparison target. Running
it adopts nothing: each feature is adopted by its own issue, with its own evidence, and then removes
its divergence below. Codex is never compared against Go; [codex/CONTRACT.md](../codex/CONTRACT.md)
and first-party `openai/codex` at `44ccf394573c` decide it.

This ledger is historical. Issue [#45](https://github.com/zakstak/cliproxy-rs/issues/45) retired
the Go differential harness and its `harness/manifest.json`, which held these pins and the named
divergences. The page records the receipt as it stood; a change no longer needs to update it.

## Adopted from v8.0.67

- C8 [#76](https://github.com/zakstak/cliproxy-rs/issues/76): 256-bit base64url OAuth state
  and the Console scope on authorization only (Go `ea0b4712`), backed by the Claude Code 2.1.322
  [bundle source](claude-code-2.1.322-oauth.md). No harness case covers the login flow;
  `crates/cpa-exec` tests pin it against fake endpoints.
- X5 [#88](https://github.com/zakstak/cliproxy-rs/issues/88), policy only: outside the Grok CLI
  chat proxy, an xAI request carries the caller's `User-Agent`, else the Grok CLI's (Go
  `5026f365`). Loopback tests in `crates/cpa-exec` pin it on HTTP and on the WebSocket
  handshake. The selective cases send no client User-Agent, so `X5-grok-user-agent` now names
  only the Grok CLI version: 1.0.46 in v8.0.67, 1.0.44 here. That version change stays deferred.

## Not adopted in this batch

`claude-cloak-tool-alias` and `claude-cloak-tool-alias-restore`: the Claude OAuth MCP alias seed
change of Go `e48d780f` is not an authorized feature under C1, C2 or plugin work. Tracker #66
records it as not adopted. cliproxy-rs keeps the frozen v8.0.35 behaviour, which the executed
v8.0.35 comparison below confirms. The comparison stays exact: on `cloak-tools-beta`,
`tool-roundtrip` and `tool-stream-roundtrip` only the alias spelling may differ, the same
number of times, and v8.0.67 does not restore the scripted alias. This is not a general
exemption for alias or cloaking differences.

## Named divergences on the selective target

Observed with the 62-case selective suite: the 50 historical Claude cases plus 12 Grok and Muse
cases over a plain-HTTP loopback upstream. Every entry is an exact predicate in the manifest:
the specific values each side must show, with everything else compared byte for byte.
Credential ownership and the native-byte golden are checked on every case regardless.

| Manifest ID | Kind | Owner | Evidence | What v8.0.67 does differently |
| --- | --- | --- | --- | --- |
| `C1-claude-client-profile` | pending | [#69](https://github.com/zakstak/cliproxy-rs/issues/69) | Go `b4e7dbd2` | Claude Code 2.1.321 User-Agent, Stainless 0.128.0 and v26.12.1, `thinking-token-count` beta position, billing-header version |
| `C2-claude-native-detection` | pending | [#69](https://github.com/zakstak/cliproxy-rs/issues/69), C1 baseline/version observation; historical ID retained | Executed: Go `b4e7dbd2`'s version gate, not C2's detection commits | Cloaks a request carrying the four strong native signals of a 2.1.280 caller. Its wire correctness is unknown, and the entry grants no adoption. Go must match its recorded v8.0.67 request exactly, and cliproxy-rs must still send the v8.0.10-recorded native bytes |
| `K1-claude-catalog` | pending | [#68](https://github.com/zakstak/cliproxy-rs/issues/68) | Catalog diff | Lists `claude-fable-5-2` |
| `I4-cors-allow-headers` | pending | [#104](https://github.com/zakstak/cliproxy-rs/issues/104) | Observed | Explicit `Access-Control-Allow-Headers` list instead of `*` |
| `I3-unauthenticated-connection-close` | pending | [#103](https://github.com/zakstak/cliproxy-rs/issues/103) | Observed | `Connection: close` on an unauthenticated rejection |
| `M1-muse-identity-headers` | pending | [#81](https://github.com/zakstak/cliproxy-rs/issues/81) | Observed | muse-build 1.4.14 non-interactive identity and session headers |
| `P2-claude-connection-reuse` | deferred | [#107](https://github.com/zakstak/cliproxy-rs/issues/107) | Go `2c5b01f4` | One more TLS handshake (a session resumption) for a later request on the same credential |
| `X5-grok-user-agent` | deferred | [#88](https://github.com/zakstak/cliproxy-rs/issues/88) | Observed; caller User-Agent policy from Go `5026f365` | Grok CLI version 1.0.46 in the API-key path's default User-Agent. cliproxy-rs adopted the policy (the caller's User-Agent, else the Grok CLI's) and keeps 1.0.44 |
| `P1-response-compression-vary` | deferred | [#106](https://github.com/zakstak/cliproxy-rs/issues/106) | `response_compression.go` | `Vary: Accept-Encoding` on every response |
| `claude-cloak-tool-alias`, `-restore` | not adopted | tracker [#66](https://github.com/zakstak/cliproxy-rs/issues/66) (decision above) | Executed: Go `e48d780f` against its parent; v8.0.35 matches cliproxy-rs | Alias secret is `CallerScope(api key)`, so the alias spelling differs and the v8.0.10 alias is no longer restored |
| `error-retry-after` | pending | R1 [#94](https://github.com/zakstak/cliproxy-rs/issues/94), production adoption and integration | Executed: Go `9a35fa12` against its parent; v8.0.35 sends none | Adds a whole-second `Retry-After` from a local retry deadline to failed attempts |
| `credential-disabled-absent` | required absence | H1 [#67](https://github.com/zakstak/cliproxy-rs/issues/67), parent tracker [#66](https://github.com/zakstak/cliproxy-rs/issues/66) | Executed bisect: Go `091ff27f`, which is already in v8.0.35 | After an abandoned stream, v8.0.67, like v8.0.35, leaves `disabled` absent, and cliproxy-rs must too. Both sides must leave the field absent, so `false` on both sides fails. H1 #67 authored the production fix; the [audit item](checklist.md#unresolved-frozen-reference-discrepancies) stays open until real Go acceptance against the fixed head executes |

Shared entries: `go-openai-model-order` (Go builds the OpenAI model list by ranging over a map), `openai-created-timestamps` (wall-clock `created` within 5 seconds),
`credential-refresh-timestamps`, `compaction-capsule-generated` and the `plain-upstream-host-label`
harness boundary. Global rules: downstream header-name case and order
([ledger](../DIFFERENCES-FROM-GO.md#deliberate-differences)), chunk boundaries, Content-Length,
generated values. On the historical target, `cors-expose-etag`, `claude-responses-compact`,
`claude-catalog-v8.0.35` and `credential-disabled-persist-v8.0.10` record behaviour cliproxy-rs
follows from v8.0.35 rather than v8.0.10. For historical compaction, cliproxy-rs is held to a golden recorded from Go v8.0.35
itself (`harness/run --target policy --golden`). Its request matched v8.0.35 byte for byte,
except for the minted request id.

Ownership routes the work; it does not adopt the behavior or accept it as correct.
R1 #94 owns Retry-After production adoption. H1 #67 authored the persistence fix, and with
tracker #66 keeps that audit open until real Go acceptance executes. C1 #69 owns only the baseline/version observation;
C2 #70 detection work and provider wire correctness remain open. A `missing-audit`, pending,
unowned or unattributed entry stays visible in receipts until its owning decision is resolved.

## Executed attribution evidence (v8.0.35 and bisect pairs)

[`harness/evidence/attribution-20261112.json.gz`](https://github.com/zakstak/cliproxy-rs/blob/f6bff59c4c1646747caced0fbb08388fd66626e0/harness/evidence/attribution-20261112.json.gz)
records what the attributions above rest on. It is separate from the acceptance receipts:
the historical v8.0.10 and selective v8.0.67 pins, their goldens and receipts do not change.
The real collector ran selected cases against Go builds of single commits, each built
offline from a private scratch clone with a clean VCS stamp. It ran in the same
loopback-only, network-denied sandbox, with fake credentials and the cached Rust driver.
Rust results are the same cached binary throughout.

| Question | Control (parent) | Commit | v8.0.35 (`ae3ee3be`) |
| --- | --- | --- | --- |
| MCP alias spelling and restore | `b2fda98e`: as cliproxy-rs | `e48d780f`: v8.0.67 spelling, no restore | as cliproxy-rs |
| `Retry-After` on six failed attempts | `c3767123`: none | `9a35fa12`: all six | none |
| `disabled` after an abandoned stream | `a2718314`: `false` (3/3) | `091ff27f`: absent (3/3) | absent |
| 2.1.280 caller with native signals | `19a0a854` (with three C2 detection commits): native | `b4e7dbd2`: cloaked; 2.1.321 and later native | native |

The disabled-persistence commit was found by a first-parent bisect of `8a01bc02..4f2e5270`
(`8a01bc02` itself still persists `false`). The native-signals variants change only the
caller's User-Agent version (2.1.279, 2.1.280, 2.1.290, 2.1.320, 2.1.321, 2.1.322) and send
no Stainless headers, so the cached cliproxy-rs driver cloaks the newer versions as
documented in [DIFFERENCES-FROM-GO.md](../DIFFERENCES-FROM-GO.md). This classifies Go
behaviour. It does not show that cloaking any genuine client is correct.

## Not compared here

- Codex: covered by the `codex-contract` cargo group against the first-party contract.
- Removed providers (#49) and native plugins (#47): the `refusals` cargo group asserts that they
  stay refused. They are excluded historical scope and do not count as active coverage.
- TLS fingerprints for Grok and Muse: their selective cases use a plain-HTTP upstream.
