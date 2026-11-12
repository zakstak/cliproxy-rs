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

[`harness/manifest.json`](../../harness/manifest.json) is the source of truth for these pins and
for every named divergence. This page summarizes the first receipt. When an issue adopts or
rejects a behaviour, update the manifest entry and this table in the same change.

## Adopted from v8.0.67

None yet.

## Named divergences on the selective target

Observed with the 62-case selective suite: the 50 historical Claude cases plus 12 Grok and Muse
cases over a plain-HTTP loopback upstream. Request bodies for Grok and Muse match byte for byte,
except where an entry below says otherwise.

| Manifest ID | Kind | Owner | What v8.0.67 does differently |
| --- | --- | --- | --- |
| `C1-claude-client-profile` | pending | [#69](https://github.com/zakstak/cliproxy-rs/issues/69) | Claude Code 2.1.321 tuple: User-Agent, Stainless package and runtime versions, beta order, billing-header version |
| `C2-claude-native-detection` | pending | [#70](https://github.com/zakstak/cliproxy-rs/issues/70) | Cloaks a request carrying the four strong native signals of a 2.1.280 caller |
| `K1-claude-catalog` | pending | [#68](https://github.com/zakstak/cliproxy-rs/issues/68) | Lists `claude-fable-5-2` |
| `I4-cors-allow-headers` | pending | [#104](https://github.com/zakstak/cliproxy-rs/issues/104) | Explicit `Access-Control-Allow-Headers` list instead of `*` |
| `I3-unauthenticated-connection-close` | pending | [#103](https://github.com/zakstak/cliproxy-rs/issues/103) | `Connection: close` on an unauthenticated rejection |
| `M1-muse-identity-headers` | pending | [#81](https://github.com/zakstak/cliproxy-rs/issues/81) | muse-build 1.4.14 non-interactive identity and session headers |
| `X5-grok-user-agent` | deferred | [#88](https://github.com/zakstak/cliproxy-rs/issues/88) | `xai-grok-workspace/1.0.46` User-Agent on the API-key path |
| `P1-response-compression-vary` | deferred | [#106](https://github.com/zakstak/cliproxy-rs/issues/106) | `Vary: Accept-Encoding` on every response |
| `claude-cloak-tool-alias` | unclassified | [#66](https://github.com/zakstak/cliproxy-rs/issues/66) | Different cloak alias for the same client key and tool |
| `claude-tls-connection-reuse` | unclassified | [#66](https://github.com/zakstak/cliproxy-rs/issues/66) | New TLS connection for a later request on the same credential (candidate P2, #107) |
| `credential-disabled-persist` | unclassified | [#66](https://github.com/zakstak/cliproxy-rs/issues/66) | Does not persist `disabled: false` after an abandoned stream |
| `error-retry-after` | unclassified | [#66](https://github.com/zakstak/cliproxy-rs/issues/66) | Adds its own `Retry-After: 31` to failed upstream attempts (candidates R1 #94, R2 #95) |
| `messages-max-tokens-to-responses` | unclassified | [#66](https://github.com/zakstak/cliproxy-rs/issues/66) | Maps Claude `max_tokens` to `max_output_tokens` for Grok and Muse (candidate T2 #80) |

Provider-neutral entries are shared with the historical target: `downstream-header-framing`
([ledger](../DIFFERENCES-FROM-GO.md#deliberate-differences)), `upstream-generated-request-ids`,
`credential-refresh-timestamps`, plus `compaction-capsule-generated` and the `plain-upstream-host-label`
harness boundary. On the historical target, `cors-expose-etag`, `claude-responses-compact` and
`claude-catalog-v8.0.35` record behaviour cliproxy-rs follows from v8.0.35 rather than v8.0.10.

An unclassified entry is visible, not accepted. It stays in every receipt until an issue
attributes it to a Go commit and decides it from provider evidence.

## Not compared here

- Codex: covered by the `codex-contract` cargo group against the first-party contract.
- Removed providers (#49) and native plugins (#47): the `refusals` cargo group asserts that they
  stay refused. They are excluded historical scope and do not count as active coverage.
- TLS fingerprints for Grok and Muse: their selective cases use a plain-HTTP upstream.
