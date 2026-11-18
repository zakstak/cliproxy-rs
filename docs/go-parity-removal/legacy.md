# Legacy management integration migration

Owned target: old `crates/cliproxy-internal/tests/legacy_go.rs` replaced by `legacy_contract.rs`. Runtime behavior is unchanged.

| Original inventory entry | Disposition and independently specified replacement |
| --- | --- |
| `go_v0_routes_replay_byte_for_byte` | Removed Go-output replay and all field-order/prose comparisons, output normalizers, exception-step arrays, removed-provider filters and recorded-count gates. Six independent Rust contract tables cover all 14 supported scalar routes in legacy/v8 layouts, defaults and effective routing strategy, PUT/PATCH/read/full-config-view/persisted reload, typed malformed/invalid refusal with unchanged disk, numeric bounds and strategy aliases; client key replace/patch/append/delete/wrapper/empty and failure atomicity; all four provider lists CRUD, actual auth indexes, v8 group persistence, invalid values/missing targets, duplicated-key endpoint disambiguation; all three OAuth maps normalize/replace/patch/delete/persist and protect other channels; every removed-provider route method and setting write refuses without changing disk, including config uploads; Claude profile/header/model-prefix/exclusion/cloak normalization and identity-sensitive inherited mode. Expectations are authored typed values, never proxy-generated or Go outputs. |
| `v0_aliases_match_v8_and_lists_carry_live_auth_indexes` | Keep independent same-Rust-router alias compatibility and live credential-index contract. |
| `v0_saves_like_go_persist_locked` | Replace with `v0_writes_preserve_comments_and_report_failed_saves`. Keep comment/group preservation, idempotent missing-entry delete and failed-write error propagation. Remove Go-only obligation that a no-op PATCH must save. Replace mode-bit/root skip with deterministic directory-at-config-file-path failure; assert saved bytes and published runtime state are unchanged. |
| `v0_config_yaml_put_writes_like_go` | Replace with `v0_yaml_upload_validates_before_replacing_hashes_keys_and_reloads_access`. Keep atomic invalid YAML refusal, preserved operator comment/strategy, plaintext key hashing/authentication, semantic config.yaml download, and key-removal access reload. Drop Go's exact comment indentation and requirement that a valid block scalar become invalid after unindent. No serialization/key-order expectation is used as a Go oracle. |

## Validation

All nine final contract tests pass with `CPA_TEST_NO_SKIP=1` in a network-denied user namespace with only loopback enabled. Log: `/tmp/cliproxy-parity-migration/legacy-final.log`.

Command:
`unshare --user --map-root-user --net sh -c 'ip link set lo up && CPA_TEST_NO_SKIP=1 CMAKE=/home/zak/.local/share/mise/installs/cmake/4.5.0/cmake-4.5.0-linux-x86_64/bin/cmake CARGO_BUILD_JOBS=2 nice -n19 cargo test --offline -p cliproxy-internal --test legacy_contract'`

Before replay retirement, all 12 tests passed with the original full Go oracle and new independently authored tables active together; log `/tmp/cliproxy-parity-migration/legacy-old-and-new.log`. After retirement, added aggregate config-view/download checks and deterministic failed-save check all pass. Each server uses a private credential directory and fake keys; no provider endpoint is contacted.

Deleted obsolete `tests/fixtures/legacy_go.json` and `tests/reference/legacy/{main.go,shape_fixture_test.go,README.md}`. No production JSON shape/default files were removed, because they remain runtime compatibility data. No Go build was used.
