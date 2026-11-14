# Contributing

Thanks for helping. Bug reports, fixes and new tests are all welcome.

## Before you start

- cliproxy-rs reads CLIProxyAPI's `config.yaml` and credential files and serves the same client routes and Management API for the providers it keeps. Working behaviour for real clients and providers is the goal: provider documentation, first-party clients and verified wire behaviour decide what is correct. CLIProxyAPI's Go code is a useful source, not a reference that must be matched byte for byte. Codex follows its own [wire contract](docs/codex/CONTRACT.md).
- If a change alters something an operator moving from CLIProxyAPI would notice (a setting, a credential file, a route, a response shape or a Management API answer), add a note to [docs/DIFFERENCES-FROM-GO.md](docs/DIFFERENCES-FROM-GO.md).
- For a larger change, open an issue first so we can agree on the approach.

## Building and testing

You need Rust (stable), `cmake`, `clang` and `perl` for BoringSSL. PostgreSQL lets the storage tests run instead of skipping; with `CPA_TEST_NO_SKIP=1`, as in CI, those tests fail instead.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

CI runs the same tests with [cargo-nextest](https://nexte.st), which runs test binaries in parallel (`cargo nextest run --workspace --locked`; its settings are in `.config/nextest.toml`), then the doc tests, the prompt-cache replay tests (`python3 tools/claude-cache/test_replay.py`), and cpa-server's unit tests with the optional relay: `cargo test -p cpa-server --features media-relay --lib`.

Tests use local mock upstreams only. Never point a test at a real provider or use a real account or key, and run the tests with external network access blocked if you can; CI runs them in a network namespace with only a loopback interface. The BoringSSL build and the release link use a lot of memory, so on a small machine set `CARGO_BUILD_JOBS=2`.

Tests check behaviour. Do not add benchmark scores, wall-clock speed bounds, allocation counts or memory limits as test assertions or CI gates ([#45](https://github.com/zakstak/cliproxy-rs/issues/45)). Tests of a configured timeout, a cancellation or a resource limit that protects the server are behaviour and are welcome.

The dashboard lives in `ui/` (Svelte 5). `npm ci`, `npm run check`, `npm test` and `npm run build` there. See [ui/README.md](ui/README.md).

## Pull requests

- Keep each pull request to one change, with a test that fails without it.
- Request bodies are forwarded byte for byte unless a ported rule rewrites them; do not re-serialize JSON just to pass it through.
- Never log or forward credentials or client keys to the wrong side.
- Mark a deliberate simplification with a `ponytail:` comment that names its limit and how to lift it.
- CI must pass: format, clippy, tests, and the check that native Windows and macOS builds stop with the Linux-only error (`.github/scripts/check-unsupported-targets.sh`). This fork builds for Linux only.

By contributing you agree that your work is released under the [MIT license](LICENSE).
