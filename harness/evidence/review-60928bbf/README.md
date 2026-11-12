# Replay of the 60928bbf review findings

This directory holds two kinds of evidence. Keep them apart: the fresh differential is the
acceptance run, and the replay re-evaluates saved inputs.

## Fresh differential (2f2b1209)

`./harness/accept` ran on the clean commit `2f2b1209`. Receipts are in `../receipts/`, and
the compact captures are `../2026-11-12-{historical,selective}.json.gz`. `toolchain.txt`
records the sandbox identity: uid 1000, all capability sets zero, rustc and cargo 1.99.0,
go 1.27.2, CMake 3.31.6, Python 3.13.5. The run used a private HOME and auth directory,
fake credentials, loopback mocks, offline registries, no external network,
`CARGO_BUILD_JOBS=2` and `nice -n 19`.

| Step | Result |
| --- | --- |
| Historical v8.0.10 `6fecc6e` | 50/50, 0 skips, 5 named divergences |
| Selective v8.0.67 `8bba7a77` | 62/62, 0 skips, 17 named divergences |
| Mutation control | 19/19 historical, 30/30 selective |
| Cargo groups | 32/32 in 9 executables, every record bound to its artifact |
| Combine | pass |

The selective run hit 17 divergences, where earlier runs hit 19. The two not hit are the
optional wall-clock entries `openai-created-timestamps` and `credential-refresh-timestamps`.
They register only when Go and Rust record different seconds. The reviewed 60928bbf harness
reports the same 17 on this capture.

`cli-probes.py` (output in `cli-probes.json`) runs one probe per finding through the actual
`evaluate` and `combine` commands on these fresh captures and receipts. The unedited inputs
exit 0, and every probe exits 1.

## Saved replay (old-head witness)

`replay-output.txt` comes from `replay.py`. It runs the same probes under the reviewed
harness (60928bbf) and under the fixed harness:

- Capture probes. The reviewer's edits are applied to 60928bbf's own committed captures,
  and each harness evaluates the whole capture. The edits are: typed generated values on
  either side (finding 2), unsafe `disabled` states (finding 3, fixed by the parent at
  ec167b25), and failed upstream writes under complete golden replacements (finding 4).
  Two further probes cover type-blind comparison outside predicates: a historical
  `disabled: 0` and `input_tokens` 13 → 13.0.
- Combine probes. The reviewer's Cargo record edits (finding 1) and the `rust_revision`
  deletion are applied to a complete receipt set in each harness's own schema. The reviewed
  harness gets the reviewer's fresh 60928bbf receipts (`reviewed-receipts/`). The fixed
  harness gets this revision's receipts (`../receipts/`).

Each unedited baseline passes under its own harness. The reviewed harness accepts every
probe, and the fixed harness rejects each one. To reproduce:

```sh
mkdir -p /tmp/h1-r4/old
git archive 60928bbf harness .github/workflows/ci.yml crates/cpa-exec/src/codex_contract.rs \
  crates/cpa-exec/tests/fixtures/codex_contract/manifest.json | tar -x -C /tmp/h1-r4/old
python3 -I harness/evidence/review-60928bbf/replay.py "$PWD" /tmp/h1-r4/old \
  harness/evidence/review-60928bbf/reviewed-receipts harness/evidence/receipts
```

The fixed harness's `combine` requires each recorded test executable on disk with its
recorded hash. Its combine baseline therefore reproduces only while the tested binaries
are still in `target/`. After a rebuild, the baseline fails on that check, not on a
schema mismatch.

`harness/test_acceptance.py` (`CargoBindingTests`, `GeneratedValueTests`,
`StrictTypeTests`, `WriteResultTests`) covers the same gaps on synthetic records.
