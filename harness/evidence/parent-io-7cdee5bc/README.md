# H1 recorded-artifact I/O correction at `7cdee5bc`

This is parent implementation evidence. A new dedicated independent Sol review is required for H1 acceptance.
The tested source is clean `7cdee5bc946c116acce9425a258bb6ef14139ec1`. The commit adding this directory changes only evidence.

## Behavior

The on-disk recorded-artifact stat/hash boundary now catches `OSError` and records unavailable artifact provenance.
`combine` continues checking the receipt's structure, reports `unavailable`, writes the receipt and exits 2 when availability is the only issue. It skips the Cargo query in that case.
Known record inconsistencies and readable hash mismatches still produce `fail` and exit 1, including when another artifact is unavailable. No unavailable path can pass.

## Validation

- The three new regression methods fail against `a830a060` production. Stat/hash I/O exceptions escape; the actual CLI on a synthetic mode-000 executable exits 1 with a traceback and no receipt. See `logs/old-regressions.txt`.
- All 70 Python tests pass on the fixed source. The new controls cover permission errors, disappearance between stat and hash, other read errors, inconsistent records and changed readable bytes.
- Clean-source `./harness/accept` passes: historical captures 50/50, selective 62/62, mutation controls 19/19 and 30/30, cached Rust tests 32/32 across nine executables, and all nine current artifacts verified by combine.
- Actual CLI on these fresh receipts, with a scratch mode-000 file mounted over the first recorded executable, writes an unavailable receipt and exits 2. No Cargo query runs and no traceback appears.
- A readable scratch file with changed bytes yields a failure receipt and exit 1, with no Cargo query. The unmodified positive control queries Cargo and passes with exit 0. See `real-cli-results.json` and the three `cli-*` directories.
- HEAD, source, index and all 12 protected executable hashes match before and after acceptance and the real CLI controls.

No original executable or its permissions changed. Negative controls mount synthetic files read-only in a nested namespace. No fresh Rust or Go build is claimed.

## Isolation and reproduction

`check.sh` takes the shared lock with one wait capped at 45 seconds, then checks at least 8,000,000,000 free bytes on `/home/agent` and `/tmp`.
The admitted run had 8,359,464,960 and 8,075,632,640 bytes free respectively.
The sandbox enforces UID 1000, zero capabilities, loopback-only networking, private HOME/auth, source/index read-only, and read-only protected executables. Real auth and `/run/user` are hidden.
Two jobs and nice 19 apply. `CARGO_INCREMENTAL` stays unset to match the cached profile, with a strict outer compiler guard refusing real compilation. No guard refusal occurred during acceptance.
The existing cached build-script execution limitation remains explicit.

Scripts retain Ember scratch paths. Original fresh captures and receipts are kept under `/home/agent/.cache/h1-parent-r8`; inputs use synthetic loopback observations only.
H1 acceptance, the unresolved frozen-reference disabled-persistence audit item, C2 wire correctness and combined workspace/provider qualification remain open.
