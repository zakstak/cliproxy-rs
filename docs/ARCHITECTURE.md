# Backend architecture

The backend workspace has two packages: the `cliproxy` executable and the
`cliproxy-internal` library. Internal modules share one compilation boundary so
related code can live together and implement traits without another crate or adapter.
Neither package is published to Cargo. The dashboard has its own build targets in
`ui/`.

## Follow a request

Start at [gateway/mod.rs](../crates/cliproxy-internal/src/gateway/mod.rs), which
registers the HTTP routes. A Messages request enters `gateway/claude.rs`; Chat
Completions and Responses enter `gateway/openai.rs`.

The handler builds a `gateway::dispatch::Call`. Dispatch coordinates attempts with
`runtime::Runtime`, which selects a credential, prepares it when necessary and owns
the attempt's lease. `runtime/scheduler.rs` owns selection and cooldown policy.

`providers::Executors` dispatches the attempt to Claude, Codex, xAI or Meta. Each
provider owns its authentication, transport and provider-specific request preparation.
`translation/client.rs` applies client-specific rewrites before format conversion.
The converters and streaming translators live together under `translation/`.
The native Codex route keeps its own byte-preserving request boundary, as required
by [the Codex contract](codex/CONTRACT.md).

Providers return the execution response or stream. Dispatch and the handler render
the client response and settle the lease. Upstream capture goes through
`gateway/request_logging.rs`; provider code must not log captured bytes itself.

## Find an owner

| Concern | Module |
| --- | --- |
| HTTP routes, client auth and management | `gateway` |
| Credential lifecycle, scheduling and registration | `runtime` |
| Upstream authentication and I/O | `providers` |
| Client/provider format conversion | `translation` |
| JSON span editing, payload rules, thinking and signatures | `protocol` |
| Config documents, credentials and execution envelopes | `config`, `credential`, `execution`, `format` |
| Static and dynamic model catalogs | `catalog` |
| Persistence contracts and cooldown records | `persistence` |
| PostgreSQL, object storage and Git adapters | `storage` |
| Home enrollment, dispatch, leases and protocol | `home` |
| Shared text compatibility and private filesystem operations | `text`, `private_fs` |

Put a trait beside the code that consumes it, or beside its shared data contract.
Implement it directly on the owning type. For example, `runtime::quota::QuotaSource`
lets scheduling read passive quota evidence from Claude and Codex without accessing
their concrete executor fields. `persistence::StorePersister` describes persistence
without belonging to the HTTP gateway. `PostgresStore` and `ObjectStore` implement
that trait directly. `GitPersister` moves blocking Git work onto the blocking pool.
Use concrete functions for fixed operations that need no interchangeable implementation.

`protocol/json_output.rs` builds generated JSON objects for HTTP envelopes and Home
messages. String fields use the shared JSON encoder; raw fields retain their JSON
spelling and whitespace. Both consumers use the same object builder.

Prefer `pub(crate)` for interfaces used only inside the library. Public items support
the executable, integration tests and the local login harness; they are not a
published library API. Add another crate only for a demonstrated independent build
target or external consumer.

Source builds use `cliproxy-internal/media-relay` and `cliproxy-internal/pprof` for
the optional backend features. Module-scoped `RUST_LOG` filters also follow the new
paths, for example `cliproxy_internal::providers=debug` replaces `cpa_exec=debug`.
Level-only filters such as `RUST_LOG=debug` keep the same behavior.

## Run the relevant tests

Unit tests live beside their modules. Integration targets use concern prefixes such
as `gateway_`, `translation_`, `protocol_` and `storage_`. Each target remains a
separate process so tests that replace process-wide model catalogs stay isolated.
Fixtures live under `tests/<concern>/fixtures`; their original provenance is retained.

```sh
cargo test -p cliproxy-internal providers::xai
cargo test -p cliproxy-internal --test gateway_soonest_reset
cargo test -p cliproxy-internal --test translation_request_pair_contract
```

Use private auth directories, local peers and an environment that blocks external
network access while retaining loopback. The full required checks and optional
PostgreSQL setup are in [Contributing](../CONTRIBUTING.md#building-and-testing).
The `test-support` feature exposes local peers and protocol fixtures for integration
tests. `media-relay` and `pprof` retain their existing opt-in behavior.
