# cliproxy-rs

If you are an agent installing cliproxy-rs for a user rather than developing it, stop here and follow [docs/AI-SETUP.md](docs/AI-SETUP.md).

A Rust rewrite of CLIProxyAPI (https://github.com/router-for-me/CLIProxyAPI), compatible with its `config.yaml`, auth JSON files in `auth-dir`, HTTP routes and response shapes and v8 Management API for the providers it keeps. This fork has four built-in upstreams: Claude (`claude`), Codex (`codex`), Grok (`xai`) and Muse (`meta`). Clients use Anthropic Messages, OpenAI Chat Completions or OpenAI Responses. Kimi, Devin, Gemini, Gemini Interactions, Vertex AI, AI Studio, Antigravity and the generic OpenAI-compatible upstreams were removed (issue #49); their settings, routes, logins and imports fail with a migration error (`cliproxy_internal::removed`), and their saved logins stay on disk unread. Do not add them back, migrate their code or keep compatibility layers for them.

Go's remote config and credential stores (`PGSTORE_*`, `OBJECTSTORE_*`, `GITSTORE_*`) were removed too (issue #222): setting one stops the server with a migration error, and `config.yaml` and `auth-dir` stay files. Storage is an abstraction: callers depend on `persistence::history::HistoryStore`, and every backend passes its conformance suite. Do not bring back a file-mirroring store.

## Compatibility

- The product contract is functional: supported clients, operators and providers get correct, secure and reliable behaviour. That covers the client APIs, `config.yaml`, the credential files in `auth-dir`, the HTTP routes and response shapes, and the v8 Management API for the four kept providers. Do not break a client or a saved config that works today just to simplify a rule.
- Provider documentation, first-party clients and verified wire behaviour decide provider behaviour. Go CLIProxyAPI is a source to read, not an oracle. The last reference snapshot is **v8.0.35**, commit `ae3ee3be303fb07fb8173279923413005bf32177` (https://github.com/router-for-me/CLIProxyAPI); treat a clone as read-only. Keep Go-compatible behaviour where a real client, an operator's config or a saved credential depends on it. Go parser quirks and Go's exact wire output are not requirements on their own. Port behaviour, not structure: idiomatic Rust over transliterated Go. Cite Go file paths in comments only where behaviour is non-obvious.
- No change needs a Go build, a Go/Rust differential run or a benchmark to land (#45). Earlier `harness/` and `bench/results/` evidence is available in Git history.
- Tests assert supported client, operator, and provider behavior using explicit Rust tables, protocol examples, local peers, and properties. Historical Go recordings retain their original provenance. Reusing their inputs is useful, but their recorded outputs are not an authority: justify expected behavior independently and preserve useful coverage when replacing a replay. Do not regenerate expectations by running Go or this proxy. Cases of a removed provider are skipped or filtered with a comment naming issue #49.
- `docs/DIFFERENCES-FROM-GO.md` is migration guidance for operators moving between Go and cliproxy-rs. Add an entry when a change alters a config, credential file, route, response shape or Management API answer in a way such an operator would notice. It is not a ledger of every byte that differs from Go.
- For Codex request and response payloads, transports and identity, [docs/codex/CONTRACT.md](docs/codex/CONTRACT.md) is the authority: first-party `openai/codex` at `44ccf394573c` and OpenAI's API documentation, per endpoint profile. Go behaviour, Go recordings and Go parser quirks do not decide the Codex contract, and an unknown in that document stays unknown until a source covers it. Historical input corpora retain their original provenance. A Codex change that an operator coming from Go would notice gets a `docs/DIFFERENCES-FROM-GO.md` entry, justified by the contract.

## Layout

The backend has one internal library, `crates/cliproxy-internal`, and one executable,
`crates/cliproxy`. Use modules for internal boundaries. Add a crate only when an
independent build target or an actual external consumer requires one.

- `crates/cliproxy-internal/src/gateway`: axum routes, client auth, management API and HTTP response handling.
- `crates/cliproxy-internal/src/runtime`: credential selection, scheduling, refresh, model registration and request lifecycle.
- `crates/cliproxy-internal/src/providers`: Claude, Codex, xAI and Meta upstream I/O, authentication and provider profiles.
- `crates/cliproxy-internal/src/translation`: format conversion and the client-rewrite entry point in `client.rs`.
- `crates/cliproxy-internal/src/protocol`: byte-preserving JSON, payload rules, thinking, signatures and session identity.
- `crates/cliproxy-internal/src/persistence`: the history store contract (`history`, with its conformance suite) and cooldown records. `storage` implements the history store in SQLite and PostgreSQL; `home` implements the Home control-plane protocol.
- `crates/cliproxy-internal/src/{config,credential,execution,format,catalog}`: shared data contracts and model catalogs. These modules perform no provider networking.
- `crates/cliproxy`: flags, startup, integration assembly and TUI. Go-style single-dash flags are accepted.
- `ui/`: the dashboard, with separate browser build targets.

Keep traits beside their consumers or in the relevant shared-contract module. Implement
them on the owning types directly. Do not add facade modules or compatibility crates
for the retired `cpa-*` packages. There is no native plugin host (#47).
See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the request path and test locations.

## Rules

- The full test suite is slow and memory-heavy (the BoringSSL build and the release link). While iterating, run the affected crate or a filter, for example `cargo test -p cliproxy-internal providers::xai`. Run fmt, `clippy -D warnings` and the whole suite locally before merging (see `CONTRIBUTING.md`). On a shared or small machine, add `CARGO_BUILD_JOBS=2 nice -n 19`.
- The PostgreSQL store tests run only when `CPA_TEST_PG_BIN` names a PostgreSQL `bin` directory; with `CPA_TEST_NO_SKIP=1`, a missing cluster fails instead of skipping. Run the suite with external network access blocked and loopback available.
- Choose case handling from the input's contract. Use Rust's Unicode case conversion for Unicode text and ASCII methods for protocol tokens or an explicitly ASCII comparison. Existing simple-case adapters remain where configuration, saved credentials, or shared identities depend on their behavior; Go's case tables are not a reason to require them for new code. Do not change an existing identity or trust comparison without checking its callers and compatibility requirements.
- Request bodies are forwarded byte for byte unless a ported rule rewrites them. Never re-serialize JSON just to pass it through.
- Upstream credentials and client keys must never be logged or forwarded to the wrong side.
- Upstream request and response capture goes through the shared sink in `crates/cliproxy-internal/src/gateway/request_logging.rs`. It redacts `Cookie` and `Set-Cookie` values, credentials in URLs and short keys, and spools large sections to disk. Don't format or log captured bytes anywhere else.
- Keep request preparation linear in the body size. Coding agents send bodies from 100 KB to several MB, so walk a JSON body once and edit by path (see `crates/cliproxy-internal/src/providers/claude/signals.rs`) instead of searching it again from the start for each element.
- Tests check behaviour, not speed or memory use. Do not add wall-clock speed bounds, allocation counts, RSS limits or benchmark scores as test assertions or CI gates. Tests of a configured timeout, cancellation or deadline, and of a resource limit that protects the server (a body, line or decompression cap), are behaviour and stay. A generous timeout that only stops a broken test from hanging is fine.
- Tests use local mock upstreams and never send traffic to real provider accounts. Give every test a private `auth-dir`: without one, credential loading falls back to `~/.cli-proxy-api`, where real logins live. Build test runtimes with `cliproxy_internal::gateway::testing::runtime`.
- Mark deliberate simplifications with a `ponytail:` comment naming the ceiling and the upgrade path.
- Prompt-cache changes must keep request semantics and stable prefixes, and show cache-read and cache-write usage for warm, changed-context and idle continuations before merge. Run that check manually against a live Claude login. Never add inference keepalive pings.

## Fork release versioning

- Build this fork locally from source or with the Dockerfile. Keep Cargo publishing disabled. Do not add release publishing, registry uploads, installers or distribution packages unless explicitly requested. Preserve this policy when integrating upstream changes.
- Use the upstream Rust project's version as the base, with a `-zak.N` suffix for fork releases. This is separate from the Go reference version above.
- Start at `N = 1` and increment for each fork release on the same upstream base: `v0.4.1-zak.1`, `v0.4.1-zak.2`, and so on. When adopting upstream `v0.4.2`, start at `v0.4.2-zak.1`.
- The Cargo workspace version and `cliproxy --version` must equal the release tag without its leading `v`. Keep workspace dependency versions and lockfiles consistent when bumping the version.
- Never move or reuse a published release tag.

## Review guidelines

Flag, in order: a reintroduced native plugin loader, plugin hook or `plugins` setting that is accepted instead of refused; a change that breaks a supported client, config file, credential file, route, response shape or Management API answer that worked before; an operator-visible change from Go without an entry in `docs/DIFFERENCES-FROM-GO.md`; on Codex routes, behaviour that differs from `docs/codex/CONTRACT.md`, a payload change outside its permitted transformations, a setting, credential attribute or client header that rewrites a Codex request or is accepted and silently ignored instead of refused (the contract's "Settings" table lists each outcome), a request path that bypasses the native or translated route, or a Codex fixture without first-party or documented provenance; request or response bytes changed on passthrough; credentials or client keys reaching logs or the wrong side; tests that can touch the network or a real `auth-dir`; work on a request body that grows faster than linearly with its size; a new Go differential, benchmark, wall-clock speed bound or allocation bound added as a required check. Skip style nits that `cargo fmt` and clippy already cover.

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
[Claude cache telemetry](docs/CLAUDE-CACHE-TELEMETRY.md) for collection and limitations.
