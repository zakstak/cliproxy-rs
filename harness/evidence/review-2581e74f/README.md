# Replay of the 2581e74f review findings

`replay-output.txt` comes from `replay.py`, run against the reviewed harness (2581e74f) and the fixed harness.

- Replacement probes. The reviewer's three edits are applied to 2581e74f's own committed
  captures and evaluated by each harness: compaction caller text, compaction `max_tokens`
  64000 -> 1, and C2 Go caller text.
- Combine probes. The reviewer's record edits keep every aggregate total unchanged. They are
  applied to a complete receipt set: the reviewer's fresh 2581e74f receipts
  (`reviewed-receipts/`) for the reviewed harness, and this revision's receipts
  (`../receipts/`) for the fixed harness.

Each unedited baseline passes under its own harness. The reviewed harness accepts every
probe, and the fixed harness rejects each one. Reproduce it with:

```sh
mkdir -p /tmp/h1-r3/old
git archive 2581e74f harness .github/workflows/ci.yml crates/cpa-exec/src/codex_contract.rs \
  crates/cpa-exec/tests/fixtures/codex_contract/manifest.json | tar -x -C /tmp/h1-r3/old
python3 -I harness/evidence/review-2581e74f/replay.py "$PWD" /tmp/h1-r3/old \
  harness/evidence/review-2581e74f/reviewed-receipts harness/evidence/receipts
```

`harness/test_acceptance.py` covers the same gaps as in-tree regressions on synthetic records.
