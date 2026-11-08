# Removal of the Rust gjson crate

The third-party Rust `gjson` crate (0.8.1, vendored with a SIMD patch in `vendor/gjson`)
is no longer a dependency. Every caller now reads JSON through `cpa_common::json`, the
independent Rust port of Go's tidwall/gjson v1.18.0 and tidwall/sjson v1.2.5, either
directly or through its `&str` facade `cpa_common::rawjson`. The vendor copy, both
`[patch.crates-io]` entries, both lockfile entries, the vendor-only CI steps and the
Docker `COPY vendor` step are deleted.

Run these commands from the repository root. Both print nothing:

```sh
rg -n '\bgjson::|^\s*use gjson\b|extern crate gjson' crates harness --glob '*.rs'
rg -nw gjson Cargo.toml Cargo.lock crates/*/Cargo.toml harness/rust/Cargo.toml harness/rust/Cargo.lock .github Dockerfile
```

Other matches for `gjson` in the tree name Go's tidwall/gjson: Go reference programs,
recorded Go fixtures, and comments that cite a Go function's behaviour.

## Readers by surface

Paths in this table are relative to `crates/`. No caller writes what it reads back to
the wire. Native Codex routes deliver the bytes they received, and observation reads a
copy.

| Surface | Reader | Contract |
| --- | --- | --- |
| `cpa-exec/src/codex_response.rs`, `codex_quota.rs` | `rawjson` | Failure classification, usage-limit and retry hints, quota headers. Quota and frame headers copy numbers raw, so `12.50` stays `12.50`. |
| `cpa-exec/src/codex_ws.rs`, `codex_duplex.rs` | `rawjson` for upstream events; `CodexBody` for client frames, as before | Event types, response IDs, interrupt acknowledgements and wrapped error frames, for bookkeeping. The client receives each frame as it arrived. |
| `cpa-exec/src/codex_images.rs` | `CodexBody` for the client's JSON; `rawjson` for upstream events | The client's Images JSON goes through the Codex request boundary with typed member reads ([codex/CONTRACT.md](codex/CONTRACT.md#request-boundary)). The upstream events and the aggregated Images response read through `rawjson`. The tool path's generated Responses body does not go through the native request boundary. |
| `cpa-exec/src/codex_json.rs` | `cpa_common::json` and `rawjson` | The Go-compatible remarshal for Alpha Search, and the conditional `model` and `stream` setters for translated and Images bodies. |
| `cpa-exec/src/codex.rs` | `rawjson` | The model in the shared send helper's quota bookkeeping, and the Alpha Search `id`. |
| `cpa-server/src/websocket.rs`, `websocket_requests.rs`, `websocket_tools.rs` | `rawjson` | The shared Responses WebSocket transcript, prewarm, tool repair and error frames for xAI. Native Codex attempts still take the preserved client frame from `NativeRequest`. |
| `cpa-server/src/codex_alpha.rs` | `rawjson` | Alpha Search routing fields, with Go's last-duplicate-wins unmarshal. |
| Executor and server tests | `rawjson` | Assertions only. Go fixtures and their recorded versions are unchanged. |

`rawjson` moved from `cpa-exec` to `cpa-common` so that the server can share it. The
Claude executor reaches it through a `crate::rawjson` re-export, unchanged.

## Reader semantics

`rawjson` reads as Go's gjson does, because it is a facade over the Go port. On every
recorded fixture document, the crate and the facade agree on every path these callers
read. The differences are all on inputs no conforming upstream or client sends. In each
case the new reading is the one Go's gjson gives:

- A path that is missing under the first of duplicate keys continues to a later
  member. For example, `error.message` in `{"error":{},"error":{"message":"m"}}` reads
  `m`. The crate stopped at the first `error`. A direct key still reads the first
  duplicate.
- A lookup into text that does not start with JSON finds the first `{` or `[`. This
  includes a leading byte-order mark. The crate found nothing.
- `str()` of a number is Go's `Result.String()`, so `1.0` reads `1` and `1e2` reads
  `100`. The crate returned the raw spelling. Callers that copy a number into output
  (quota headers, frame headers, raw `set_raw` copies) use the raw `json()` and keep
  the spelling.
- `i64()` is Go's `Int()`. A string such as `"429.5"` or `"+429"` reads 0, where the
  crate read 429. An integer beyond the `i64` range converts as Go's `int64(float64)`
  does, where the crate saturated.
- An invalid `\u` escape decodes to NUL as in Go, where the crate produced U+FFFD.
  Such a document is not valid JSON.

`.items()` replaces the crate's `.array()` at every migrated call site: the elements of
an array, and nothing for any other value. `rawjson`'s own `array()` is Go's `Array()`,
which returns a one-element list for a scalar. `crates/cpa-common/src/rawjson.rs` tests
these semantics, and the Codex, xAI WebSocket and native passthrough tests check the
callers.

The differential check that compared the crate with the facade before the removal is
not part of the tree. It needed both readers in one build. Its report is in the pull
request that removed the crate.

## Contract boundaries

Codex requests follow [codex/CONTRACT.md](codex/CONTRACT.md). `CodexBody` remains the
one parser for native request routing and its permitted transport edits. The translated
and Images tool bodies can carry raw source schemas with duplicate keys. Those bodies do
not go through the native request boundary, which would refuse them.

The other retained providers (Claude, xAI and Meta) follow the provider-first policy in
[DIFFERENCES-FROM-GO.md](DIFFERENCES-FROM-GO.md#compatibility-policy), with Go v8.0.35
as the default where no provider source decides. Do not replace a Go-compatible reader
with `serde_json::Value`. That would change duplicate-key lookup, coercions, invalid
UTF-8, lone-surrogate handling and raw number spelling.

A non-string Codex `prompt_cache_key` does not become header identity, and a non-string
duplex frame type does not become a name in a diagnostic.
[DIFFERENCES-FROM-GO.md](DIFFERENCES-FROM-GO.md) lists both. They came from the
earlier request-inspection migration (`codex_request.rs` and `codex_duplex.rs` client
frames) and are unchanged here.

## Local validation

Run the tests in an isolated checkout, with two low-priority build jobs, private test
state and loopback mock upstreams. The tests do not prove live-provider behaviour. No
release, installation, deployment or live-login request is part of this migration.
