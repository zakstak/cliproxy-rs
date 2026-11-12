# Replay of the f4d8e222 review findings

`replay-output.txt` comes from `replay.py`, run against the reviewed harness and against the
fixed harness. Both read the reviewed commit's own selective capture. Reproduce it with:

```sh
mkdir -p /tmp/h1-old
git archive f4d8e222 harness/acceptance.py harness/manifest.py harness/differential.py \
  harness/fixtures.py harness/tls_capture.py harness/manifest.json .github/workflows/ci.yml \
  crates/cpa-exec/src/codex_contract.rs crates/cpa-exec/tests/fixtures/codex_contract/manifest.json \
  | tar -x -C /tmp/h1-old
git show f4d8e222:harness/evidence/2026-11-12-selective.json.gz > /tmp/h1-old/capture.json.gz
python3 -I harness/evidence/review-f4d8e222/replay.py "$PWD" /tmp/h1-old /tmp/h1-old/capture.json.gz
```

The reviewed harness accepts all four holes, and the fixed harness rejects each one.
`harness/test_acceptance.py` holds the same scenarios as unit tests against synthetic captures.
