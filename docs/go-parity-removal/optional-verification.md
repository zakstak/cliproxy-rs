# Optional final verification

All final checks ran in a user/network namespace with only loopback enabled and all capabilities dropped after setup:

```sh
unshare --user --map-current-user --keep-caps --net sh -c '
ip link set lo up &&
setpriv --bounding-set=-all --inh-caps=-all --ambient-caps=-all env ... COMMAND
'
```

- `cargo test --offline -p cpa-server --features media-relay --lib`: **651 passed, 0 failed, 0 ignored** (103.53s). `CPA_TEST_NO_SKIP=1`, `CPA_TEST_PG_BIN=/tmp/cliproxy-parity-migration/pg/usr/bin`, `LD_LIBRARY_PATH=/tmp/cliproxy-parity-migration/pg/usr/lib`, actual CMake 4.5.0 executable, `CARGO_BUILD_JOBS=2 nice -n19`. Log: `media-relay-final.log`.
- `python3 tools/claude-cache/test_replay.py`: **9 passed**. Log: `cache-replay-final.log`.
- `CARGO_NET_OFFLINE=true CARGO_BUILD_JOBS=2 nice -n19 bash .github/scripts/check-unsupported-targets.sh`: **passed**; Windows MSVC and Apple Darwin both stop with the intended Linux-only build error. Log: `unsupported-targets-final.log`.
- PostgreSQL binary preflight: `postgres (PostgreSQL) 18.6` using the extracted library directory above.

The feature build found one test-only STUN generator type mismatch. Changed `provider.as_ref()` to `provider.crypto()` in `crates/cpa-server/src/realtime/tunnel_tests.rs`; the library constructor requires `RTCCrypto`, not the containing `RTCCryptoProvider`. Coordinated with the server owner; formatted the file.

An initial feature test run with capabilities still enabled produced 650 passes and the expected refusal from the permission-specific filesystem watcher test (`CPA_TEST_NO_SKIP=1`): ambient `CAP_DAC_OVERRIDE` made its chmod ineffective. Preserved as `media-relay-with-caps.log`. Dropping all capabilities after enabling loopback restored the intended test conditions; the entire rerun above passed without skips. No remaining optional verification blockers.
