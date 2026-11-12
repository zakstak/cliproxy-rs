# Opus test-owner evidence for `84a40b27` (#67, H1)

This directory holds author and test-owner evidence. It is not an independent review and
does not accept H1.

The tested source is clean `84a40b27a0dfef9afd6aab5328c740c46ea0cd7d`. Its two commits on
top of `1e0aa543` are:

- `40c5776f`: binds combine to cargo's current artifacts without compiling;
- `84a40b27`: the executed attribution ledger, with the alias seed not adopted.

The evidence commit that adds this directory changes only `harness/evidence/`.

## What ran, and what it shows

All Rust results are **cached Rust artifacts, not fresh Rust builds**.
`CARGO_INCREMENTAL` was left unset, which matches the author's existing profile fingerprints
(`7333259681545804295` for the local crates). An outer `RUSTC_WRAPPER`
([`outer-rustc-guard.sh`](outer-rustc-guard.sh)) refused every real compilation. It refused
nothing in any run, so every Rust binary is the one built at `02:25`–`02:39` on 2026-11-12.

With `CARGO_INCREMENTAL=0`, the profile hash changes and the whole local crate chain
rebuilds. That was the cause of the earlier reviewer run that recompiled the driver's
dependencies. An authorized rebuild still uses `CARGO_INCREMENTAL=0`, two jobs and
`nice 19`, and needs new matching evidence.

Every run used the sandbox in [`sandbox.sh`](sandbox.sh):

- uid 1000, all capability sets 0, environment cleared;
- a private `HOME` and empty `auth-dir`, with the real home, `/run/user`, the shared source
  cache and parent scratch hidden;
- loopback only, with egress `ENETUNREACH`;
- the source and `.git/index` read-only;
- `nice 19` and two jobs.

[`heavy.sh`](heavy.sh) holds `/tmp/cliproxy-selective-heavy-build.lock` and checks that both
`/home/agent` and `/tmp` have at least 8,000,000,000 bytes free *after* locking.

Toolchain: rustc and cargo 1.99.0 (`RUSTUP_HOME=/usr/local/rustup`), Go 1.27.2,
Python 3.13.5 and CMake 3.31.6. No provider account, network, release or deployment was
involved; all credentials are fake.

### Fresh acceptance at `84a40b27` (receipts in `../receipts/`)

`./harness/accept` exited 0. Results:

- historical 50/50 and selective 62/62, with 0 failed, 0 infrastructure, 0 skips and
  0 stale;
- mutation controls 19/19 and 30/30;
- cargo groups 32/32 across 9 test binaries, each fresh before and after and each with the
  committed SHA-256;
- combine `pass`, with its own cargo query available: all 9 targets current and 0 refusals.

The captures are `../2026-11-12-historical.json.gz` and `../2026-11-12-selective.json.gz`.
Named divergences vary by run only among optional `nondeterministic` wall-clock entries.

### Combine fix: real-receipt probes

Each probe is an actual `harness/acceptance.py combine` run on edited copies of real
fresh receipts. An attack counts only with exit 1 and its intended problem
(`INTENDED()` in [`combine-attacks-v2.py`](combine-attacks-v2.py)).

| Head | Controls (3) | Same-crate substitutions (2) | Other attacks (145) |
| --- | --- | --- | --- |
| `1e0aa543` (old, [`results-v1-1e0aa543.json`](results-v1-1e0aa543.json)) | exit 0 | **exit 0: wrongly accepted** | exit 1 |
| `40c5776f` (fix, clean tree, [`witness-40c5776f-before-edit.json`](witness-40c5776f-before-edit.json)) | exit 0 | exit 1, current-artifact mismatch only | not counted (see below) |
| `84a40b27` (final, [`results-v2-84a40b27.json`](results-v2-84a40b27.json)) | exit 0, cargo queried | exit 1, current-artifact mismatch only | exit 1, intended reason, 145/145 |

The substitutions name real non-H1 builds on disk with their true hashes:
`cpa_core-eae3190c02f55b84` (`83647e06…`) and `cpa_exec-f827e5f864028b25` (`0cf321a3…`).

Some runs are not counted:

- The first v1 attempt is **INVALID**. It ran the child with `python3 -I`, every call
  crashed importing `judge`, and the controls caught it.
- The full v2 run against the `40c5776f` receipts is **CONTAMINATED**. The worktree was
  edited at 09:05 while it ran. Only its five probes that finished before the edit
  (08:46–08:50, clean `40c5776f`) are kept as the witness above.

Both runs are retained in the test-owner scratch.

Python tests: 63 pass. These include compile-free tests of target-set mismatch, missing,
stale or extra artifacts, an unavailable query, refusals, source change and changed bytes,
plus the wrapper itself. [`fix-mutants.py`](fix-mutants.py) removes each of the ten new
guards in turn, and the tests catch every one ([`fix-mutants.txt`](fix-mutants.txt)).
[`live-query-cache-only.json`](live-query-cache-only.json) is one live query record: its
argv, the cargo environment recorded, each target's cargo profile, and source identity.

### Integrity

The snapshots record the 12 protected executables: 9 cargo test binaries, the
differential driver and both original Go binaries. Their hashes are identical in the
original snapshot ([`snapshot-before.json`](snapshot-before.json)) and before and after the
final runs. HEAD, status and index are unchanged across the final runs. Fresh Go builds of
both pins went to scratch and are byte-identical to the originals.

## Executed attribution (`../attribution-20261112.json.gz`)

These are separate from acceptance, and they change no pin or golden. The real collector
ran single-commit Go builds, built offline from a private shared clone with clean stamps.
They used a RAM-only Go build-cache overlay
([`attr-ephemeral.sh`](attr-ephemeral.sh)) after one v8.0.35 build had added
130,809,847 bytes to the shared Go cache. Results are in
`docs/parity-audit/selective-v8.0.67.md`:

- **Alias seed.** `e48d780f` against its parent flips the alias. v8.0.35 matches
  cliproxy-rs exactly. It is recorded as not adopted by tracker #66.
- **Retry-After.** `9a35fa12` against its parent. v8.0.35 sends none.
- **Disabled persistence.** A seven-step first-parent bisect finds `091ff27f`, which is
  inside v8.0.35. The parent persists `false` and the commit leaves the field absent, 3/3
  each. **cliproxy-rs differs from the frozen v8.0.35 reference here, and no ledger entry
  records that yet.**
- **Native signals.** The cloaking follows `b4e7dbd2`'s version gate, not the C2 detection
  commits. Wire correctness stays unknown.

## Limits

The current-artifact query shows that cargo now reports these cached artifacts as fresh
for this source. It is not a build, not a cryptographic attestation, and does not prove that
results were produced. A rustc wrapper cannot refuse a build-script *run*. The selected
cargo tests do not cover the whole workspace, clippy, Go/PostgreSQL comparisons, Codex
contract unknowns or live behaviour. Grok and Muse use plain HTTP.
