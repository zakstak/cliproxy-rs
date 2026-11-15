# Historical Unicode table provenance

`unicode/main.go` records the source of the compatibility tables in
`cpa-core/src/gostr_tables.rs`. Those tables still serve saved identity and
configuration compatibility. This source is provenance, not an executable test
or a requirement for contributors. New casing behavior follows the input contract.
All Go test-output recorders were retired in issue #198.
