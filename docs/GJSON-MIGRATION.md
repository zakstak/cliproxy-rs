# Remaining gjson consumers

The third-party Rust `gjson` crate remains a direct dependency of `cpa-exec` and
`cpa-server`. The workspace and `harness/rust` patch it to `vendor/gjson`; both
lockfiles and the vendor test steps in `.github/workflows/ci.yml` still need it.
Remove those together only after the last consumer migrates.

Regenerate the source inventory from the repository root:

```sh
rg -n 'gjson::' crates --glob '*.rs'
rg -n gjson Cargo.toml Cargo.lock crates/*/Cargo.toml harness/rust/Cargo.toml harness/rust/Cargo.lock .github/workflows/ci.yml
```

## Inventory after the provider cutdown

Regenerated after issue #49 kept only the Claude, Codex, Grok (`xai`) and Muse (`meta`)
upstreams. None of the removed providers' modules (Kimi, Devin, Gemini, Vertex, AI
Studio, Antigravity, OpenAI-compatible) used the third-party crate: they read JSON
through `cpa_common::json`, so their deletion removed no consumer and none was
migrated. The command above lists 24 files, all Codex, xAI or shared WebSocket code:
11 source files (7 in `cpa-exec`, 4 in `cpa-server`) and 13 test files (`*_tests.rs` and
`tests/`). Issue #47 removed one more consumer, `cpa-server`'s `management/quota_probe.rs`.
The removal of the external dependency, the vendor copy, the manifest patches, the lockfile entries
and the vendor-only CI steps still waits on those consumers.

## Contract boundaries

Codex requests follow [codex/CONTRACT.md](codex/CONTRACT.md). Use `CodexBody` for
native request routing and its permitted transport edits. Borrow raw values when a
parser needs only a few fields. Forward the original bytes after inspection.
Unknown members, numeric spelling and escapes must survive.

The other retained providers (Claude, xAI, Meta) follow the provider-first policy in
[DIFFERENCES-FROM-GO.md](DIFFERENCES-FROM-GO.md#compatibility-policy), with Go v8.0.35
as the default where no provider source decides. `cpa_common::json` is an independent Rust port of
Go's byte-oriented gjson/sjson behavior, not a dependency on the Rust `gjson` crate.
The translators already use that port. Replacing a Go-compatible reader with
`serde_json::Value` would change duplicate-key lookup, coercions, invalid UTF-8,
lone-surrogate handling and raw numeric spelling. Migrate such callers only with
Go-derived tests for their behavior, or document a deliberate difference.

## Migrated request inspection and edits

- `cpa-exec/src/codex_request.rs` reads Responses Lite metadata, the cache identity
  and the routing-hint tier through a serde struct of borrowed `RawValue` fields.
  Unknown metadata and raw schemas are skipped without building a tree or applying
  the native boundary to a translated body.
- `cpa-exec/src/codex_duplex.rs` reads later client-frame types, models and parent
  IDs through the boundary. A deferred steer retains its decoded parent alongside
  its original payload. Upstream-event bookkeeping still uses gjson.
- `cpa-common/src/codex_body.rs` applies model and stream routing in one output
  copy. An unchanged body stays borrowed. Native HTTP and duplex creates use it.
  The extra `create`/`native_create` forwarding layer is deleted.
- Claude's `rawjson` adapter exposes its own JSON-kind enum. Its readers already
  use `cpa_common::json`; the executor and its Claude modules no longer import
  the third-party crate just to name kinds. Their Go behavior stays the same.

A non-string Codex `prompt_cache_key` no longer becomes header identity through
Go's string coercion. It remains in the forwarded body, and the existing session
fallback supplies the header identity, if available. Non-string duplex frame types
also no longer become names in unsupported-type diagnostics. These differences
are listed in [DIFFERENCES-FROM-GO.md](DIFFERENCES-FROM-GO.md).

## Retained source callers

Paths in this table are relative to `crates/`.

| Files | Remaining use and migration boundary |
| --- | --- |
| `cpa-exec/src/codex_response.rs`, `codex_quota.rs` | Failure classification, events, usage and quota headers. Migrate observation separately from passthrough; do not alter delivered events or erase raw number spellings. |
| `cpa-exec/src/codex_ws.rs`, `codex_duplex.rs` | Upstream event types, response IDs, error status/headers and steering acknowledgements. Socket ownership, failure and interrupt tests must accompany migration. |
| `cpa-exec/src/codex_images.rs` | Images input parsing, tool settings and output aggregation. The adapter performs a cross-API conversion; direct Images bodies must retain passthrough behavior. |
| `cpa-exec/src/codex_json.rs` | Go-compatible object remarshal for Alpha Search, plus byte-edit helpers used by translated requests, Images, token records and response normalization. The conditional model/stream setters remain live. |
| `cpa-exec/src/codex.rs` | Quota model inspection in the shared send helper and Alpha Search response IDs. Alpha Search is outside the Codex Responses contract and retains Go behavior. |
| `cpa-server/src/websocket.rs`, `websocket_requests.rs`, `websocket_tools.rs` | Shared WebSocket transcript, prewarm, request and tool handling for xAI, the only other provider with a Responses WebSocket. Native Codex attempts use the preserved client frame instead. |
| `cpa-server/src/codex_alpha.rs`, `management/quota_probe.rs` | Alpha Search and management quota probes, outside the Codex Responses contract. Preserve their Go behavior. |
| Executor and server unit/integration tests returned by the inventory command | Assertions and helpers still use gjson. They also must migrate before deleting the dependency. Keep historical Go fixtures labelled with their recorded versions. |

The translated and Images tool bodies can retain raw source schemas. Applying the
native request boundary to those generated bodies would introduce new refusals,
including duplicate schema keys. Their editor migration needs its own policy
and tests, so this change leaves their Go-compatible edits in place.

`rawjson` and `codex_json` remain live adapters over `cpa_common::json`. Removing
the external crate will not by itself remove the Go-compatible port or justify
changing other providers' semantics.

## Local validation

Use an isolated checkout, two low-priority build jobs, private test state and
loopback mock upstreams. Build dependencies before entering a network namespace;
run test binaries with only loopback enabled. The local tests do not establish
live-provider acceptance or physical-device behavior. No release, installation,
deployment or live-login request is needed for this migration.
