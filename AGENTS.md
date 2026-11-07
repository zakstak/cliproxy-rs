# cliproxy-rs

If you are an agent installing cliproxy-rs for a user rather than developing it, stop here and follow [docs/AI-SETUP.md](docs/AI-SETUP.md).

A Rust rewrite of CLIProxyAPI (https://github.com/router-for-me/CLIProxyAPI) targeting full parity and drop-in compatibility: same `config.yaml`, same auth JSON files in `auth-dir`, same HTTP routes and response shapes, same v8 Management API.

## Reference

- The reference is frozen at CLIProxyAPI **v8.0.35**, commit `ae3ee3be303fb07fb8173279923413005bf32177`: clone https://github.com/router-for-me/CLIProxyAPI, check out that commit and treat the clone as read-only. Cite Go file paths in comments only where behaviour is non-obvious. Existing goldens keep their recorded versions; they do not prove later Go changes. `docs/parity-audit/checklist.md` tracks the target contracts; `docs/parity-audit/upstream-v8.0.35.md` lists the commits since the previous reference, `6fecc6e`.
- When Go behaviour and documentation disagree, the Go code wins. Port behaviour, not structure: idiomatic Rust over transliterated Go.
- A deliberate difference from Go is listed in `docs/DIFFERENCES-FROM-GO.md`, and tests that compare against Go goldens encode it explicitly.
- Codex is the exception. For Codex request and response payloads, transports and identity, [docs/codex/CONTRACT.md](docs/codex/CONTRACT.md) is the authority: first-party `openai/codex` at `44ccf394573c` and OpenAI's API documentation, per endpoint profile. Go behaviour, Go goldens and Go parser quirks do not decide the Codex contract, and an unknown in that document stays unknown until a source covers it. Codex goldens stay labelled with their Go version as historical evidence for behaviour not yet migrated. A Codex PR that changes behaviour visible to an operator coming from Go still lists it in `docs/DIFFERENCES-FROM-GO.md`, justified by the contract. Every other provider keeps the Go v8.0.35 rule above.

## Layout

- `crates/cpa-core`: config and credential file formats. No networking.
- `crates/cpa-common`: provider-neutral, Go-exact helpers shared by translators and executors.
- `crates/cpa-translate`: format translation between the Anthropic, OpenAI and Gemini APIs.
- `crates/cpa-exec`: one module per upstream provider (wire format, auth headers, HTTP client profile).
- `crates/cpa-server`: axum routes, client auth, credential selection, management API.
- `crates/cpa-plugin`, `crates/cpa-store`, `crates/cpa-home`: the plugin host, the remote config and auth stores, and Home mode.
- `crates/cliproxy`: the binary. Go-style single-dash flags are accepted.
- `ui/` is the dashboard, `harness/` sends the same requests to Go and Rust and compares them.

## Rules

- The full test suite is slow and memory-heavy (the BoringSSL build and the release link). While iterating, run the affected crate or a filter, for example `cargo test -p cpa-exec kimi`; CI runs fmt, `clippy -D warnings` and the whole suite. On a shared or small machine, add `CARGO_BUILD_JOBS=2 nice -n 19`.
- The Go and PostgreSQL comparison tests in `cargo test` run fully only in CI, inside a network-denied namespace. For a local Go/Rust differential run on Linux, use `./harness/run` (see `harness/README.md`).
- Port Go's `strings.ToLower`, `strings.ToUpper` and `strings.EqualFold` with `cpa_core::gostr::GoStr` (`go_lower`, `go_upper`, `go_eq_fold`). Rust's full case mappings differ on non-ASCII input, so `clippy.toml` disallows `str::to_lowercase` and `str::to_uppercase`; keep `eq_ignore_ascii_case` only where the input is ASCII by definition (header names, protocol tokens) or the comparison is explicitly ASCII. An ASCII literal alone is not enough: Go's `EqualFold` also matches `ſ` to `s` and the Kelvin sign `K` to `k`.
- Request bodies are forwarded byte for byte unless a ported rule rewrites them. Never re-serialize JSON just to pass it through.
- Upstream credentials and client keys must never be logged or forwarded to the wrong side.
- Upstream request and response capture goes through the shared sink in `crates/cpa-server/src/request_logging.rs`. It redacts `Cookie` and `Set-Cookie` values, credentials in URLs and short keys, and spools large sections to disk. Don't format or log captured bytes anywhere else.
- Keep request preparation linear in the body size. Coding agents send bodies from 100 KB to several MB, so walk a JSON body once and edit by path (see `crates/cpa-exec/src/claude/signals.rs`) instead of searching it again from the start for each element.
- Performance and memory claims come from `docs/BENCHMARKS.md`. `bench/run.sh`, `bench/messages.sh` and `bench/latency.sh` reproduce its scripted workloads; field observations there are labelled as such and are not reproducible on demand.
- Tests use local mock upstreams and never send traffic to real provider accounts. Give every test a private `auth-dir`: without one, credential loading falls back to `~/.cli-proxy-api`, where real logins live. Build test runtimes with `cpa_server::testing::runtime`.
- Mark deliberate simplifications with a `ponytail:` comment naming the ceiling and the upgrade path.
- Prompt-cache changes must keep request semantics and stable prefixes, and show cache-read and cache-write usage for warm, changed-context and idle continuations before merge. Never add inference keepalive pings. `bench/claude-cache-live.sh` runs that check against a live Claude login, by hand.

## Review guidelines

Flag, in order: behaviour that differs from Go v8.0.35 (`ae3ee3be`) without an entry in `docs/DIFFERENCES-FROM-GO.md` or an explicit partial/missing audit item, or, on Codex routes, behaviour that differs from `docs/codex/CONTRACT.md`, a payload change outside its permitted transformations, a setting, credential attribute, plugin hook or client header that rewrites a Codex request or is accepted and silently ignored instead of refused (the contract's "Settings" table lists each outcome), a request path that bypasses the native or translated route, or a Codex fixture without first-party or documented provenance; request or response bytes changed on passthrough; credentials or client keys reaching logs or the wrong side; tests that can touch the network or a real `auth-dir`; work on a request body that grows faster than linearly with its size. Skip style nits that `cargo fmt` and clippy already cover.

## Claude cache telemetry deployment check

Publishing a release or replacing Core's running proxy requires Vayun's explicit
approval. Mock tests do not prove that Core's collector retains the new fields.
After an approved deployment, explicitly enable `observability.usage.claude-cache-telemetry`
alongside usage statistics. Inspect the collector's protected JSONL file without
printing legacy credential fields (hashes remain stable only until proxy restart):

```sh
jq -c 'select(.claude_cache.schema_version == 2) | {execution_id, usage_observed, tokens: {cache_read_tokens: .tokens.cache_read_tokens, cache_creation_5m_tokens: .tokens.cache_creation_5m_tokens, cache_creation_1h_tokens: .tokens.cache_creation_1h_tokens}, claude_cache}' "$USAGE_JSONL"
```

Confirm that normal existing Amp traffic produces ordered start, first-byte, and
end timestamps, final TTLs, stable hashes, and returned TTL buckets. Check both
buffered and streaming requests if both occur naturally. Do not send synthetic
requests to a live login for telemetry-only deployment checks. This rule does not
replace the required live cache-read/cache-write check above for changes to cache
semantics. The usage queue holds at most one hour, so a week-long
study needs a continuous collector and a separate hashed schedule export. See
[Claude cache telemetry](docs/CLAUDE-CACHE-TELEMETRY.md) for replay and limitations.
