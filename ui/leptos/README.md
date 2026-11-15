# Try the Leptos dashboard

This standalone prototype implements Activity and configuration editing in Rust.
Leptos renders in the browser through WebAssembly. A small Axum server embeds the
assets and serves synthetic activity and an in-memory YAML document.

The existing Svelte dashboard remains the production dashboard. This workspace
does not link the proxy, load config or credentials from disk, or include an
upstream HTTP client. Its routes are sandbox fixtures, not the Management API.
Reloading the page drops local drafts. Restarting the server resets all data.

## Build and run

You need stable Rust, the `wasm32-unknown-unknown` target, and
`wasm-bindgen-cli` version `0.2.130`. Node is not required for this build.

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.130 --locked
cd ui/leptos
scripts/build.sh
target/release/ledger-sandbox 8847
```

Open <http://127.0.0.1:8847>. The server binds only to IPv4 loopback. Its optional
argument selects a port. Stop it with Ctrl+C. The binary contains the generated
assets, so it runs without `dist/` beside it. Generated files and build outputs
are ignored by Git.

Set `WASM_BINDGEN` to an existing binary if it is not on your path. The build
defaults to two Cargo jobs. The workspace has its own lockfile and does not add
dependencies to the production proxy.

## Try the interactions

- **Activity:** search by client, provider or model, filter requests that need
  attention, pause and resume, and open a request with the keyboard. Escape closes
  details and returns focus to the request. The tab retains up to 80 records.
- Expand **Try a failure scenario** and fail the next read. The error retains the
  last good records. **Retry** resumes reading from the last successful cursor.
- **Configuration:** edit the YAML, switch to Activity and back, then review the
  retained draft. The comparison shows the original and the exact proposed text.
- Expand **Try a conflicting write** after reviewing. Simulate another operator
  saving, then apply your reviewed changes. The server refuses the stale write
  with HTTP 412 and returns its latest snapshot.
- Keep your draft, edit it against the new revision, and review again. Or use the
  server version, then restore your previous draft. Repeated conflicts retain
  earlier drafts. A dirty draft requests the browser's normal unload warning.
- Switch between light and dark themes and try a narrow viewport.

## Verify changes

Build the assets first, because the sandbox embeds them at compile time. Fetch
dependencies before entering a network namespace.

```sh
cargo fetch --locked
../scripts/isolated.sh 'cargo test --workspace --offline --locked'
../scripts/isolated.sh 'cargo clippy --workspace --all-targets --offline --locked -- -D warnings'
../scripts/isolated.sh 'cargo clippy -p ledger-leptos --target wasm32-unknown-unknown --offline --locked -- -D warnings'
cargo fmt --all --check
```

The Rust tests use pure editor transitions and an in-process Axum router. No
test loads an auth directory or calls a provider. Run the browser and server in
the same namespace when using the repository's browser harness. For a repeatable
browser check, install the existing UI's development dependencies with `npm ci`
from `ui/`, then run from `ui/leptos/`:

```sh
CHROME_PATH=/absolute/path/to/chromium ../scripts/isolated.sh \
  'node scripts/browser-check.mjs /tmp/ledger-leptos-captures'
```

The check starts and stops its own sandbox on a random loopback port, uses a
temporary browser profile, and refuses external page requests. It verifies WASM
startup, activity recovery, draft retention, conflict protection, exact saved
bytes, keyboard focus, three viewport widths and both themes.

## Scope of the experiment

The shared `model` crate owns fixture DTOs, YAML syntax validation and the editor's
review, save and conflict states. Both the browser and server use it. The server
checks `If-Match` and updates the in-memory document under one lock. Reviewed YAML
bytes, including comments and whitespace, are preserved.

`app` owns Leptos components, browser requests, polling and focus. `sandbox` owns
the fixture routes and embedded assets. The only handwritten JavaScript is the
WASM loader. Styling uses plain CSS and the existing Ledger tokens and font.

This experiment validates YAML syntax, not the proxy's complete configuration
schema. It shows full versions side by side rather than a line diff, and it does
not implement authentication, accounts, usage charts or a production API adapter.
Those remain migration work if the prototype is accepted. The migration must
retain the existing Management API, session cancellation, uncertain-write
recovery, reverse-proxy prefixes and configuration validation behavior.
