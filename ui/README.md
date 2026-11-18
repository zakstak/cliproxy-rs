# cliproxy-rs dashboard

Ledger is the primary dashboard. Svelte 5 renders the interface, Effect handles typed API operations, and Tailwind 4 generates layout utilities at build time. The Rust binary embeds the checked-in `dist/` directory and serves `/management.html`. The former dashboard and the separate prototype are removed.

An isolated [Leptos prototype](leptos/README.md) implements Activity and configuration editing with a fixture-only Rust server. Run it separately to evaluate a Rust frontend. It does not replace Ledger or connect to a running proxy.

## Development

Requires Node 22.12 or newer.

```sh
npm ci
CPA_BACKEND=http://127.0.0.1:8347 npm run dev
npm run check
npm test
npm run build
```

Vite binds to loopback on port 5173. It proxies `/v8`, `/v1` and `/healthz` to `CPA_BACKEND` (default `http://127.0.0.1:8317`). Point it at a disposable server when experimenting: the dashboard edits that server's configuration and credentials.

`npm run build` builds Ledger and copies `dist/index.html` to `dist/management.html`. Rebuild the UI before compiling Rust; `crates/cliproxy-internal/build.rs` embeds those assets. Fonts and icons are local. Effect is the only additional runtime dependency; Tailwind and Vite are build tools.

## Safe sandbox verification

Use a test binary, a private auth directory and a loopback-only network namespace. Never use `~/.cli-proxy-api` or real provider credentials for tests. `sandbox-check.mjs` creates a fresh directory under the system temp directory and starts a server with disabled fake OAuth credentials and a fake provider API key. It also starts a local mock upstream for a request that must appear in Activity.

After building the UI and the Rust binary, run from `ui/`:

```sh
CHROME_PATH=/absolute/path/to/chrome scripts/isolated.sh \
  'node scripts/sandbox-check.mjs /absolute/path/to/cliproxy /tmp/ledger-captures'
```

`isolated.sh` denies external networking and leaves loopback available to the server and browser. The harness stops its server after the check. It keeps the disposable config and captures for inspection. It does not install, deploy or replace a running proxy.

For a disposable server you already started:

```sh
CHROME_PATH=/absolute/path/to/chrome node scripts/browser-check-rust.mjs \
  http://127.0.0.1:8347/management.html /tmp/ledger-captures
```

The browser script verifies the private auth directory before writing anything. Its fixture uses management key `orb-dashboard-test-only`, a disabled account named `operator@example.invalid`, a client key, and `routing.retry.request-retry: 3`. It checks all screens in both themes at desktop, tablet and phone widths, real config writes with `If-Match`, YAML parsing, conflict protection, drafts, client-key model checks and keyboard detail focus.

Add `--oauth` to the sandbox command and pass the `login-harness` binary to run the local sign-in checks instead. Build it with `cargo build --manifest-path scripts/login-harness/Cargo.toml` from `ui/`.

`oauth-check.mjs` exercises sign-in against the local provider endpoints in `fake-logins.mjs`, using the non-shipped `login-harness/` Rust binary. Its runtime denies nonlocal provider traffic. These tests prove local protocol flows; they do not prove real provider approval, screen-reader output or physical touch.

## Source layout

| Path | Responsibility |
| --- | --- |
| `src/api-session.ts`, `src/api.ts` | Same-server authentication, response formats, ETags, cancellation, session lifetime and client-key model checks |
| `src/server.ts` | Effect operations, typed failures and immutable reviewed config writes |
| `src/app.svelte.ts`, `src/latest.ts` | Session, navigation, retained data, polling and current-read coordination |
| `src/screens/` | Activity, Usage, Accounts, Tools and Settings |
| `src/parts/` | Details, charts, setup, account sign-in and the configuration editor |
| `src/draft.ts`, `src/diff.ts` | Draft history, multi-hunk review and conflict reconciliation |
| `src/core.ts`, `src/records.ts`, `src/observe.ts`, `src/batch.ts` | Data parsing, account state, watched-time metrics and partial batch results |
| `src/app.css` | Theme tokens and shared components; Tailwind uses these tokens for layout utilities |

## API and state

The management key stays in memory. Reloading or signing out drops it. API calls resolve beside the page, including under a reverse-proxy prefix, and refuse redirects. Sign-out aborts outstanding management calls and rejects late responses from the old session. A rejected key returns to sign-in. Theme choice is kept in browser storage; drafts and watched requests stay in the tab's memory.

Activity starts watching `/observability/usage/live` after sign-in. It reads by cursor without consuming the collector queue, keeps the last hour or 2,000 records, and labels observation gaps. Rates use time actually watched. Config keys are joined by their server-assigned `auth_index` with `/observability/usage/api-keys`; the credential-file endpoint omits them. Their runtime health is labelled unknown. There is no group-disable toggle because the server does not implement it. Accounts refresh every ten seconds while visible; live activity and server logs refresh every three seconds without overlapping reads. Usage shows the server's twenty ten-minute request buckets, with unknown counts distinguished from zero.

Limit checks call `/requests/api-call` only after an explicit user action or account sign-in, one account at a time and at most once per account every two minutes. Failed reads retain the last good limits. Adding an account uses `/oauth/auth-url`, `/oauth/status`, `/oauth/callback` and `/oauth/session`; provider approval happens on the provider's page. No simulated approval or fixture controls ship in the dashboard.

Config writes capture the target, original value and proposed value when reviewed. They reread and compare, then send the server's ETag in `If-Match`. The Rust server checks that version while saving and rejects stale writes with HTTP 412. Only a race involving an unrelated change is rechecked automatically; target changes require reconciliation. An unanswered write is reported as uncertain, and retry checks whether it already landed. Drafts survive navigation, repeated conflicts keep earlier versions, and sign-out or page unload warns about unsaved work.

The primary routes are `#logs`, `#logs/server`, `#usage`, `#accounts`, `#tools` and `#settings`. Settings has `keys`, `models`, `payload`, `config` and `system` sections. Former dashboard bookmarks redirect to the corresponding Ledger screen.
