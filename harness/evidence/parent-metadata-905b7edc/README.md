# H1 metadata correction evidence at `905b7edc`

This directory records parent implementation evidence. H1 acceptance still requires a new dedicated Sol review.
The tested source is clean `905b7edcec014820fd454858d5d89f78d0d62f13`.
The commit adding this directory changes only `harness/evidence/`.

The manifest and selective ledger now follow the ownership decisions in issue #67.
`error-retry-after` belongs to R1 #94. The frozen-v8.0.35 disabled-persistence discrepancy is an explicit unresolved `missing-audit` item under H1 #67 and parent tracker #66.
The baseline-version observation belongs to C1 #69, with its historical case ID preserved. C2 #70 detection, provenance and wire correctness remain open.
The alias-seed change remains not adopted.

## Validation

- A JSON-value comparison confirms that predicates, pins, cases, groups, reasons and invariants are unchanged. See `metadata-preservation.json`.
- The new `missing-audit` kind requires an issue, evidence, attribution and ledger, and is restricted to the selective target. It does not relax predicate validation. The schema regression rejects absent fields and historical or mixed targets.
- All 67 Python tests pass. The old schema rejects the new audit kind. This is schema regression evidence, not a runtime reproduction of the metadata ownership finding.
- `./harness/accept` exits 0 on the clean tested source. Historical captures pass 50/50, selective captures pass 62/62, mutation controls catch 19/19 and 30/30, and 32 cached Rust tests across nine executables pass. Combine reports all nine current artifacts. See `receipts/` and `logs/accept.txt`.
- The selective receipt records hits for all three corrected entry IDs. Ownership and kind are metadata in `ownership-inputs.json`; differential receipts report IDs rather than copying this metadata.
- HEAD, source, index and all 12 protected executable hashes match before and after acceptance.

No fresh Rust or Go build is claimed. The strict outer compiler guard refused nothing during acceptance. The existing build-script execution limitation remains.
Production behavior, inventory totals, comparison predicates and policy pins are unchanged.
The disabled-persistence discrepancy is still unresolved. No deliberate-difference policy or production fix follows from this metadata correction.

## Isolation and reproduction

`check.sh` takes the shared lock and checks at least 8,000,000,000 bytes free on both filesystems after locking.
The admitted run had 8,385,871,872 bytes free on `/home/agent` and 8,078,360,576 bytes on `/tmp`.
`sandbox.sh` enforces UID 1000, zero capabilities, private HOME and auth, loopback-only networking, read-only source and index, and read-only protected executables.
`CARGO_INCREMENTAL` stays unset to preserve the cached profile. The outer wrapper refuses real Rust compilation.
The scripts retain Ember scratch paths and the private cache at `/home/agent/.cache/h1-parent-r7`.
The captured runs are synthetic loopback observations. Original receipts and captures remain in that private cache.

H1 acceptance, the unresolved frozen-reference audit item, wire correctness, combined workspace qualification and provider gates remain open.
