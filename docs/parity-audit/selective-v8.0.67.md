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
cases over a plain-HTTP loopback upstream. Every entry is an exact predicate in the manifest:
the specific values each side must show, with everything else compared byte for byte.
Credential ownership and the native-byte golden are checked on every case regardless.

| Manifest ID | Kind | Owner | Evidence | What v8.0.67 does differently |
| --- | --- | --- | --- | --- |
| `C1-claude-client-profile` | pending | [#69](https://github.com/zakstak/cliproxy-rs/issues/69) | Go `b4e7dbd2` | Claude Code 2.1.321 User-Agent, Stainless 0.128.0 and v26.12.1, `thinking-token-count` beta position, billing-header version |
| `C2-claude-native-detection` | pending | [#70](https://github.com/zakstak/cliproxy-rs/issues/70) | Observed only | Cloaks a request carrying the four strong native signals of a 2.1.280 caller. Its wire correctness is unknown, and the entry grants no adoption. Go must match its recorded v8.0.67 request exactly, and cliproxy-rs must still send the v8.0.10-recorded native bytes |
| `K1-claude-catalog` | pending | [#68](https://github.com/zakstak/cliproxy-rs/issues/68) | Catalog diff | Lists `claude-fable-5-2` |
| `T2-messages-max-output-tokens` | pending | [#80](https://github.com/zakstak/cliproxy-rs/issues/80) | Go `01b24f1f` | Adds `max_output_tokens` from Claude `max_tokens` for Grok and Muse |
| `I4-cors-allow-headers` | pending | [#104](https://github.com/zakstak/cliproxy-rs/issues/104) | Observed | Explicit `Access-Control-Allow-Headers` list instead of `*` |
| `I3-unauthenticated-connection-close` | pending | [#103](https://github.com/zakstak/cliproxy-rs/issues/103) | Observed | `Connection: close` on an unauthenticated rejection |
| `M1-muse-identity-headers` | pending | [#81](https://github.com/zakstak/cliproxy-rs/issues/81) | Observed | muse-build 1.4.14 non-interactive identity and session headers |
| `P2-claude-connection-reuse` | deferred | [#107](https://github.com/zakstak/cliproxy-rs/issues/107) | Go `2c5b01f4` | One more TLS handshake (a session resumption) for a later request on the same credential |
| `X5-grok-user-agent` | deferred | [#88](https://github.com/zakstak/cliproxy-rs/issues/88) | Observed | `xai-grok-workspace/1.0.46` User-Agent on the API-key path |
| `P1-response-compression-vary` | deferred | [#106](https://github.com/zakstak/cliproxy-rs/issues/106) | `response_compression.go` | `Vary: Accept-Encoding` on every response |
| `claude-cloak-tool-alias`, `-restore` | unowned | none (tracker [#66](https://github.com/zakstak/cliproxy-rs/issues/66)) | Go `e48d780f` | Alias secret is `CallerScope(api key)`, so the alias spelling differs and the v8.0.10 alias is no longer restored |
| `error-retry-after` | unattributed | none (tracker #66) | Code path `sdk/api/handlers/handlers_errors.go:127-138`; commit not bisected (candidates referenced by #94 and #111) | Adds a whole-second `Retry-After` from a local retry deadline to failed attempts |
| `credential-disabled-persist` | unattributed | none (tracker #66) | No commit located | Leaves `disabled` absent after an abandoned stream where cliproxy-rs and v8.0.10 persist `false` |

Shared entries: `go-openai-model-order` (Go builds the OpenAI model list by ranging over a map), `openai-created-timestamps` (wall-clock `created` within 5 seconds),
`credential-refresh-timestamps`, `compaction-capsule-generated` and the `plain-upstream-host-label`
harness boundary. Global rules: downstream header-name case and order
([ledger](../DIFFERENCES-FROM-GO.md#deliberate-differences)), chunk boundaries, Content-Length,
generated values. On the historical target, `cors-expose-etag`, `claude-responses-compact`
and `claude-catalog-v8.0.35` record behaviour cliproxy-rs follows from v8.0.35 rather than
v8.0.10. For historical compaction, cliproxy-rs is held to a golden recorded from Go v8.0.35
itself (`harness/run --target policy --golden`). Its request matched v8.0.35 byte for byte,
except for the minted request id.

An unowned or unattributed entry is visible, not accepted as correct. It stays in every
receipt until an issue decides it from provider evidence.

## Not compared here

- Codex: covered by the `codex-contract` cargo group against the first-party contract.
- Removed providers (#49) and native plugins (#47): the `refusals` cargo group asserts that they
  stay refused. They are excluded historical scope and do not count as active coverage.
- TLS fingerprints for Grok and Muse: their selective cases use a plain-HTTP upstream.
