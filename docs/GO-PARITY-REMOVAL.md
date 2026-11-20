# Go parity test migration

Implementation for [#198](https://github.com/zakstak/cliproxy-rs/issues/198), following the
[original inventory](GO-PARITY-TEST-INVENTORY.md) at
`f6bff59c4c1646747caced0fbb08388fd66626e0`.

The inventory classified 552 Rust test entry points: 70 with Go-only expectations to
remove or narrow, 444 with useful coverage to replace, and 38 independently justified
tests to keep. Those are original entry-point counts, not fixture-case counts or a
promise to retain the same test structure. Each original entry keeps its value and
replacement difficulty in the inventory. The scope reports below map old tests to
current behavioral coverage and identify discarded obligations.

The scope reports record implementation and independent coverage reviews.
[#198](https://github.com/zakstak/cliproxy-rs/issues/198) tracks landing status.

## What changed

Tests now specify supported behavior through Rust tables, local HTTP/WebSocket peers,
reference models, semantic JSON assertions and exact passthrough-byte checks.
Historical corpora supply inputs with their original provenance. Recorded Go outputs,
recorder population checks, expectation overlays, decoder prose and incidental JSON
layout no longer decide correctness.

The global `clippy.toml` ban on `str::to_lowercase` and `str::to_uppercase` is removed.
Casing follows the input contract: Unicode text uses Rust text methods; ASCII protocol
tokens use ASCII methods. Existing saved identity and configuration comparisons retain
the compatibility behavior their callers need. The Unicode table source remains as
production provenance; it is not a test oracle or a contributor toolchain requirement.

Obsolete output fixtures, Go recording adapters and regeneration instructions are
retired with their consumers. The historical parity audit, probe and reference-check
scripts and their 16 bookkeeping tests are retired. Earlier audit receipts are
available in Git history. `AGENTS.md`, `CONTRIBUTING.md` and compatibility documentation describe the
provider-first testing policy.

## Behavior fixes found during migration

- A bare mapped IPv6 trusted-proxy entry now trusts exactly that host in mapped and
  canonical form. It previously produced a broad IPv6 `/32` and could admit an
  unrelated forwarded-loopback claim. Explicit CIDRs retain their meaning.
- Responses-to-Claude translation now preserves `tool_choice: "none"` by sending
  Claude's explicit `none` choice. Keeping tools while dropping this choice could
  enable a tool invocation the caller prohibited.
- xAI OAuth uses checked expiry arithmetic. Unrepresentable token expiries remain
  unknown; device deadlines and poll intervals are bounded without integer wrapping.

[DIFFERENCES-FROM-GO.md](DIFFERENCES-FROM-GO.md) records operator-visible changes.

## Coverage accounting

| Scope | Original-to-current accounting |
| --- | --- |
| Configuration, credentials and management | [Core and management contracts](go-parity-removal/core.md) |
| Shared JSON, query, payload, signature and protocol helpers | [Common](go-parity-removal/common.md) |
| Claude execution, OAuth, headers, cache, tools and streaming | [Claude](go-parity-removal/claude.md) and [recorded-family disposition](go-parity-removal/claude-cases.md) |
| Codex HTTP, OAuth, images, SIWC, WebSocket and compression | [Codex executor](go-parity-removal/codex-exec.md) and [Realtime helpers](go-parity-removal/codex-live.md) |
| Generic execution, Muse and xAI HTTP/OAuth | [Other executors](go-parity-removal/other-exec.md) |
| xAI WebSocket conversations and reconnects | [xAI WebSocket](go-parity-removal/xai-ws.md) |
| CLI, discovery, TUI, Home and storage | [CLI/Home/store](go-parity-removal/cli-home-store.md) |
| Translation, tools, media and streaming state | [Translation](go-parity-removal/translate.md) |
| Server routes, media, logging, usage, security and tunnel behavior | [Server](go-parity-removal/server.md) |
| Model capabilities, registry, sessions and scheduler | [Additional server modules](go-parity-removal/server-extra.md) |
| Conversation-prefix selection and lineage | [LCP](go-parity-removal/lcp.md) |
| Legacy management routes and persisted configuration | [Legacy contracts](go-parity-removal/legacy.md) |
| Responses WebSocket, Realtime and testing policy | [WebSocket/Realtime](go-parity-removal/root.md) |
| Optional feature and platform checks | [Optional verification](go-parity-removal/optional-verification.md) |

The LCP reference model checks 18,000 generated operations. Reversing production
longest-trajectory selection made the model fail, and the production source was then
restored. Independent cross-scope reviews also checked for lost coverage: rejected
translation inputs, namespace recovery, signed reasoning, streaming failure/finish
states, media request destinations and payloads, streaming media framing, model
metadata, and exact SDP target addresses.

## Verification

The workspace run exercised 2,702 tests with no skips: 2,701 passed and one failed
because a pre-existing cooldown fixture could expire during disk IO before restore.
The test setup now confirms restored state before waiting for expiry. The migration
assertions and production cooldown behavior are unchanged. An injected restore delay
reproduced the failure before the correction and passed afterward. The entire affected
target then passed all 16 tests. Other workspace targets were unchanged after that
run. See the [server receipt](go-parity-removal/server.md) for the cause and correction.

Final checks:

- Workspace `cargo fmt --all --check`: passed after the fixture correction.
- Workspace Clippy, all targets, `-D warnings`: passed after the fixture correction.
- Workspace tests and the corrected target: all 2,702 tests covered by the receipts
  above, with no remaining failures or ignored tests.
- Optional `media-relay` server library: 651 passed, none ignored.
- PostgreSQL integration: 6 passed with skipping disabled.
- Offline Claude cache replay: 9 passed.
- Unsupported Windows MSVC and Apple Darwin build-policy checks: passed.
- `git diff --check`: passed.

Receipts are in `/tmp/cliproxy-parity-migration/`: `workspace-final-3.log`,
`model-cooldown-keys-final.log`, `clippy-post-cooldown.log`,
`fmt-post-cooldown.log`, `media-relay-final.log`, `postgres-real-2.log`,
`cache-replay-final.log` and `unsupported-targets-final.log`.

These are local mock and filesystem checks. They do not establish live provider
acceptance or a running deployment. No Go build, differential comparison, benchmark
or real provider request was used.

Run tests with external networking denied, loopback enabled, a private test runtime,
and `CARGO_BUILD_JOBS=2` on a shared machine. Set `CPA_TEST_PG_BIN` to PostgreSQL's
`bin` directory and `CPA_TEST_NO_SKIP=1` so database cluster tests execute. A namespace
that maps the current user and keeps its namespace capabilities allows loopback setup
while PostgreSQL runs as a non-root user. Drop namespace capabilities after
loopback setup so permission tests exercise ordinary filesystem access:

```sh
unshare --user --map-current-user --net --keep-caps sh -c '
  ip link set lo up
  exec setpriv --bounding-set=-all --inh-caps=-all --ambient-caps=-all \
    env CARGO_BUILD_JOBS=2 nice -n 19 cargo test --offline --workspace --locked
'
```

Use the installed CMake binary if the shell's CMake shim has no selected version.
Downloaded test-only PostgreSQL binaries can be extracted under a temporary directory;
no system database service is needed.

## Integration with current fork

Before landing, this migration was rebased onto `846a6db9`, retaining the intervening
schema, Codex and OAuth fixes. PRs #197 and #202 had already removed `harness/`,
`bench/`, `.github/`, `.config/` and obsolete documentation. Those removals remain
in effect. Earlier scope receipts describe the original migration baseline; archived
material and the unsupported-target script they mention are available in Git history.
The Claude OAuth first-party evidence remains at
[`crates/cliproxy-internal/tests/providers/reference/claude/oauth.md`](../crates/cliproxy-internal/tests/providers/reference/claude/oauth.md).

Post-rebase validation is recorded in the landing PR.
