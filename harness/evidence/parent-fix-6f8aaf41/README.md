# Parent correction evidence for H1 at `6f8aaf41`

This directory records parent implementation evidence, not an independent review or H1 acceptance.
The tested source is clean `6f8aaf41bde420cf180a3e7a24b7c77aae1ad3cd`, on top of reviewed `2d4cc667`.
The commit adding this directory changes only `harness/evidence/`.

The compiler wrapper now accepts complete compiler-information command shapes.
Compilation commands containing `--cfg ___`, an appended `-vV`, output flags, or a probe name still fail.
Cargo startup errors and artifact read errors produce an unavailable query, so combine writes a receipt and exits 2.

## Validation

- The three new regressions fail against `2d4cc667` production files. The wrapper sends eight refused-command cases to the fake compiler, and missing Cargo exits 1 with a traceback. See `logs/old-regressions.txt`.
- All 66 Python tests pass on the correction. See `logs/all-python.txt`.
- `./harness/accept` exits 0 at the clean tested source. Historical captures pass 50/50, selective captures pass 62/62, mutation controls catch 19/19 and 30/30, and Cargo runs 32 tests across nine cached executables. Combine passes with all nine artifacts current. See `receipts/` and `logs/accept.txt`.
- The real Cargo control uses a separate, dependency-free project and a final fake compiler guard. No compilation command reaches that guard. Ordinary compilation is refused, and `RUSTFLAGS="--cfg ___"` is refused at the information probe. See `fake-compiler-cargo.json`.
- The actual combine CLI, using the five fresh receipts with Cargo absent from PATH, writes an unavailable receipt and exits 2 without a traceback. See `missing-cargo-real-receipt.json`.
- HEAD, source, index, and all 12 original executable hashes match before acceptance, after acceptance, and after the controls. See the three snapshots.

Rust artifacts are cached. No fresh Rust or Go build is claimed.
The strict outer compiler guard refused nothing during acceptance.
The Cargo wrapper retains its documented inability to prevent an existing build script from running.
Unsupported compiler-information flags fail closed rather than broadening the command whitelist.

## Isolation and reproduction

`check.sh` holds the shared build lock and checks at least 8,000,000,000 bytes free on both filesystems after locking.
The admitted runs started with 8.44 GB and 8.43 GB free on `/home/agent`, and 8.08 GB on `/tmp`.
`sandbox.sh` enforces uid 1000, zero capabilities, loopback-only networking, private empty auth directories, and read-only source, index, and protected executables.
`CARGO_INCREMENTAL` stays unset to match the cached profile. The outer wrapper refuses Rust compilation. Normal authorized builds retain the incremental-off policy.
Toolchain and isolation checks are in `safety.json`.

The scripts retain their original Ember scratch paths. Run them only with the same ownership and private cache mounts after inspecting the paths.
The compressed captures contain synthetic loopback observations, not provider traffic.

H1 acceptance requires a new dedicated Sol review. Ledger reconciliation, the frozen-v8.0.35 disabled-persistence audit gap, wire correctness, and combined qualification remain open.
