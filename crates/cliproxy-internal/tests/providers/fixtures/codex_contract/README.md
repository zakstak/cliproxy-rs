# Codex contract fixtures

Small, hand-authored Codex wire payloads. `manifest.json` records the provenance of each
file: its transport, direction, endpoint profiles, kind of evidence, exact sources and
the contract it shows. A fixture that cites a researched first-party revision instead of the
contract pin names it in `revision`. The pin is `44ccf394573cabb95e1f751a59f9d76f6f79faf1`;
researched main is `5c3ff1dabf644169f4bd74b215719f381e4fdb3f`. Research fixtures do not
move the contract pin. [docs/codex/CONTRACT.md](../../../../../../docs/codex/CONTRACT.md)
defines the contract and the rules for adding a fixture.

Never generate a file here by running cliproxy-rs or from a Go golden. Use placeholders
only: no credentials, captured traffic or account IDs. `providers::codex_contract` reads
these files, and its tests fail when a file is missing from the manifest.
