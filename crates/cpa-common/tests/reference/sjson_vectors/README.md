# sjson/gjson vectors

`tests/fixtures/sjson_go_vectors.json` holds the `sjson_*` and `gjson_string` records of
`zz_rsfix_vectors_test.go` (`TestRSFixKimiVectors`), recorded at CLIProxyAPI `6fecc6e`
for the Kimi executor this fork removed (issue #49). Only the provider-neutral JSON
records are kept; the Kimi thinking, schema and message vectors were deleted with the
Kimi executor.

Regenerate from a `6fecc6e` checkout as `crates/cpa-exec/tests/device_fixtures/README.md`
describes (copy the generator into `internal/runtime/executor/`, run
`go test -run TestRSFixKimiVectors` with `RSFIX_OUT` set), then keep only the records
whose `fn` is `sjson_delete`, `sjson_set_str`, `sjson_set_raw` or `gjson_string`.
