# cliproxy-rs

If you are an agent installing cliproxy-rs for a user rather than developing it, stop here and follow [docs/AI-SETUP.md](docs/AI-SETUP.md).

A Rust rewrite of CLIProxyAPI (https://github.com/router-for-me/CLIProxyAPI), compatible with its `config.yaml`, auth JSON files in `auth-dir`, HTTP routes and response shapes and v8 Management API for the providers it keeps. This fork has four built-in upstreams: Claude (`claude`), Codex (`codex`), Grok (`xai`) and Muse (`meta`). Clients use Anthropic Messages, OpenAI Chat Completions or OpenAI Responses. Kimi, Devin, Gemini, Gemini Interactions, Vertex AI, AI Studio, Antigravity and the generic OpenAI-compatible upstreams were removed (issue #49); their settings, routes, logins and imports fail with a migration error (`cpa_core::removed`), and their saved logins stay on disk unread. Do not add them back, migrate their code or keep compatibility layers for them.

## Compatibility

- The product contract is functional: supported clients, operators and providers get correct, secure and reliable behaviour. That covers the client APIs, `config.yaml`, the credential files in `auth-dir`, the HTTP routes and response shapes, and the v8 Management API for the four kept providers. Do not break a client or a saved config that works today just to simplify a rule.
- Provider documentation, first-party clients and verified wire behaviour decide provider behaviour. Go CLIProxyAPI is a source to read, not an oracle. The last reference snapshot is **v8.0.35**, commit `ae3ee3be303fb07fb8173279923413005bf32177` (https://github.com/router-for-me/CLIProxyAPI); treat a clone as read-only. Keep Go-compatible behaviour where a real client, an operator's config or a saved credential depends on it. Go parser quirks and Go's exact wire output are not requirements on their own. Port behaviour, not structure: idiomatic Rust over transliterated Go. Cite Go file paths in comments only where behaviour is non-obvious.
- No change needs a Go build, a Go/Rust differential run or a benchmark to land (#45). Earlier `harness/` and `bench/results/` evidence is available in Git history.
- Go goldens in the test suite are regression fixtures. They keep the Go version they were recorded from. When a deliberate, provider-correct change makes one disagree, change the Rust test's expectation with a comment that says why. Do not re-record it from Go to match. Golden cases of a removed provider are skipped or filtered with a comment naming issue #49.
- `docs/DIFFERENCES-FROM-GO.md` is migration guidance for operators moving between Go and cliproxy-rs. Add an entry when a change alters a config, credential file, route, response shape or Management API answer in a way such an operator would notice. It is not a ledger of every byte that differs from Go. `docs/parity-audit/` is a historical inventory against v8.0.35, not a gate.
- Codex is the exception. For Codex request and response payloads, transports and identity, [docs/codex/CONTRACT.md](docs/codex/CONTRACT.md) is the authority: first-party `openai/codex` at `44ccf394573c` and OpenAI's API documentation, per endpoint profile. Go behaviour, Go goldens and Go parser quirks do not decide the Codex contract, and an unknown in that document stays unknown until a source covers it. Codex goldens stay labelled with their Go version as historical evidence for behaviour not yet migrated. A Codex change that an operator coming from Go would notice gets a `docs/DIFFERENCES-FROM-GO.md` entry, justified by the contract.

## Layout

- `crates/cpa-core`: config and credential file formats. No networking.
- `crates/cpa-common`: provider-neutral, Go-exact helpers shared by translators and executors.
- `crates/cpa-translate`: format translation between the Anthropic Messages, OpenAI Chat Completions and OpenAI Responses (Codex) formats.
- `crates/cpa-exec`: one module group per upstream provider (Claude, Codex, xAI, Meta: wire format, auth headers, HTTP client profile), with shared helpers in provider-neutral modules (`helpers`, `login`, `openai_http`, `openai_payload`, `multipart`, `compaction_capsule`, `claude_input_tokens`).
- `crates/cpa-server`: axum routes, client auth, credential selection, management API.
- `crates/cpa-store`, `crates/cpa-home`: the remote config and auth stores, and Home mode. There is no native plugin host: Go's plugin model is removed (#47, `docs/DIFFERENCES-FROM-GO.md` "Native plugins"); add a capability as a built-in Rust feature, and choose any future extension mechanism separately.
- `crates/cliproxy`: the binary. Go-style single-dash flags are accepted.
- `ui/` is the dashboard. `tools/claude-cache/` holds the manual live prompt-cache check and the offline cache replay. Retired Go comparisons and benchmark results are available in Git history.

## Rules

- The full test suite is slow and memory-heavy (the BoringSSL build and the release link). While iterating, run the affected crate or a filter, for example `cargo test -p cpa-exec xai`. Run fmt, `clippy -D warnings` and the whole suite locally before merging (see `CONTRIBUTING.md`). On a shared or small machine, add `CARGO_BUILD_JOBS=2 nice -n 19`.
- The PostgreSQL store tests run only when `CPA_TEST_PG_BIN` names a PostgreSQL `bin` directory; with `CPA_TEST_NO_SKIP=1`, a missing cluster fails instead of skipping. Run the suite with external network access blocked and loopback available.
- Port Go's `strings.ToLower`, `strings.ToUpper` and `strings.EqualFold` with `cpa_core::gostr::GoStr` (`go_lower`, `go_upper`, `go_eq_fold`). Rust's full case mappings differ on non-ASCII input, so `clippy.toml` disallows `str::to_lowercase` and `str::to_uppercase`; keep `eq_ignore_ascii_case` only where the input is ASCII by definition (header names, protocol tokens) or the comparison is explicitly ASCII. An ASCII literal alone is not enough: Go's `EqualFold` also matches `ſ` to `s` and the Kelvin sign `K` to `k`.
- Request bodies are forwarded byte for byte unless a ported rule rewrites them. Never re-serialize JSON just to pass it through.
- Upstream credentials and client keys must never be logged or forwarded to the wrong side.
- Upstream request and response capture goes through the shared sink in `crates/cpa-server/src/request_logging.rs`. It redacts `Cookie` and `Set-Cookie` values, credentials in URLs and short keys, and spools large sections to disk. Don't format or log captured bytes anywhere else.
- Keep request preparation linear in the body size. Coding agents send bodies from 100 KB to several MB, so walk a JSON body once and edit by path (see `crates/cpa-exec/src/claude/signals.rs`) instead of searching it again from the start for each element.
- Tests check behaviour, not speed or memory use. Do not add wall-clock speed bounds, allocation counts, RSS limits or benchmark scores as test assertions or CI gates. Tests of a configured timeout, cancellation or deadline, and of a resource limit that protects the server (a body, line or decompression cap), are behaviour and stay. A generous timeout that only stops a broken test from hanging is fine.
- Tests use local mock upstreams and never send traffic to real provider accounts. Give every test a private `auth-dir`: without one, credential loading falls back to `~/.cli-proxy-api`, where real logins live. Build test runtimes with `cpa_server::testing::runtime`.
- Mark deliberate simplifications with a `ponytail:` comment naming the ceiling and the upgrade path.
- Prompt-cache changes must keep request semantics and stable prefixes, and show cache-read and cache-write usage for warm, changed-context and idle continuations before merge. Never add inference keepalive pings. `tools/claude-cache/live.sh` runs that check against a live Claude login, by hand.

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
[Claude cache telemetry](docs/CLAUDE-CACHE-TELEMETRY.md) for replay and limitations.
