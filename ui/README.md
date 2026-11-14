# cliproxy-rs dashboard

The management dashboard for cliproxy-rs, written in Svelte 5 with plain CSS. It talks to the v8 Management API defined by Go CLIProxyAPI, and it has two outputs built from one source tree:

- `dist/` is embedded by the Rust binary (`crates/cpa-server/build.rs`) and served at `/management.html`.
- `dist-panel/management.html` is one self-contained file for existing Go CLIProxyAPI servers. See [PANEL.md](PANEL.md).

Product rules are in [PRODUCT.md](PRODUCT.md) and the visual system in [DESIGN.md](DESIGN.md).

## Commands

Requires Node 22.12 or newer.

```sh
npm ci
npm run dev      # Vite on :5173, proxies /v8 to 127.0.0.1:8317 (override with CPA_BACKEND=...)
npm run check    # svelte-check, 0 errors and 0 warnings expected
npm test         # unit tests for src/core.ts
npm run build    # dist/ and dist-panel/
```

`npm run build` runs `vite build`, then `scripts/panel.mjs` (copies `dist/index.html` to `dist/management.html`, the path the Rust server serves; inlines JS, CSS, font and favicon into `dist-panel/management.html`; writes its SHA-256 to `dist-panel/management.html.sha256` and into PANEL.md). The bundle has no size budget ([#45](https://github.com/zakstak/cliproxy-rs/issues/45)).

Rebuild the UI before compiling Rust when UI sources change; `ui/dist` is checked in and embedded at compile time.

## Structure

| Path | Role |
| --- | --- |
| `src/api.ts` | Fetch wrapper. Holds the management key in memory and reports unimplemented routes (Go-style empty 404, 501, 405). |
| `src/store.svelte.ts` | Shared state: session, server kind from version headers, config, credentials, capabilities, toasts, writes with stale checks. |
| `src/core.ts` | Pure helpers: credential state, traffic buckets, usage records, quota windows, diffs. Tested in `src/core.test.ts`. |
| `src/Load.svelte` | Loading, error, not-available and data states for one read. |
| `src/Missing.svelte` | One line naming actions this server does not implement. |
| `src/Grille.svelte` | The 20-bucket traffic grille. |
| `src/Editor.svelte` | JSON or YAML editor with a reviewed diff and a reread before writing. |
| `src/pages/*.svelte` | One component per screen. |

## Honest states

Every read renders loading, empty, error, not available, or data. The server kind comes from response headers: cliproxy-rs sends `X-CPA-VERSION: cliproxy-rs-<version>`; Go sends its version with `X-CPA-COMMIT` and `X-CPA-BUILD-DATE`. Go implements the whole v8 API, so it is never probed.

Other servers are probed once per session for the write actions a screen offers. A route that does not exist answers with gin's empty 404 (or 501/405), and the UI cannot tell that apart from a real route without asking, because `OPTIONS` returns 204 everywhere. Each probe is therefore a request Go rejects with 400 during input validation, before any side effect: an empty JSON body to `POST /credentials`, `POST /credentials/refresh`, `PATCH /credentials/fields`, `DELETE /credentials`, `POST /routing/cooldown/reset`, `POST /requests/api-call` and `POST /oauth/import`; `GET /oauth/auth-url` without a provider. Each was checked against Go 6fecc6e to return 400 with the auth directory and config unchanged. The live usage feed is a cliproxy-rs route, probed with `GET /observability/usage/live?count=0`, which cliproxy-rs rejects with 400 before reading the feed. Actions whose probe meets an empty 404 render disabled and are named in one line; reads the server lacks show "Not available on this server" with the route and status code. `DELETE /observability/logs` cannot be probed safely, so Clear is disabled when the log read is unavailable and otherwise marks itself off after a first empty 404.

Traffic comes from `recent_requests` in `GET /credentials` and `GET /observability/usage/api-keys` (twenty 10-minute buckets, reported by the server). The live usage view pages through `GET /observability/usage/live` by cursor. That feed copies usage records without removing them, so collectors that read the usage queue and other dashboard tabs still get every record. The view starts on a button, shows only requests that arrive after it starts, and keeps events in the tab only: the last hour, at most 2,000. If the tab falls behind, or is hidden for longer than a minute while no other viewer reads, the server reports a gap and the view says that some requests were missed. Go CLIProxyAPI has no such feed and is never probed, so the dashboard treats the route as missing there: the button is disabled and the page names the live request view as not available.

## Endpoints

All paths are under `/v8/management`. Every screen also uses `GET /config` and `GET /credentials`.

| Screen | Endpoints |
| --- | --- |
| Overview | `GET /credentials` every 10 s while visible |
| Accounts | `GET /credentials` every 10 s while visible, `POST /credentials` (upload), `DELETE /credentials`, `GET /credentials/download`, `GET /credentials/models`, `PATCH /credentials/status`, `PATCH /credentials/fields`, `POST /credentials/refresh`, `POST /routing/cooldown/reset`; quota checks by button only, one at a time and at most once per account every 2 minutes: `POST /requests/api-call` (Claude, Codex usage). Batch actions send one request per account, except one `DELETE /credentials` for all. |
| Connect | `GET /oauth/auth-url`, `GET /oauth/status` every 2 s while waiting, `POST /oauth/callback`, `DELETE /oauth/session` |
| Providers | `GET /observability/usage/api-keys`; `PUT /config/api-keys/<family>` |
| Client keys | `PUT /config/access/api-keys` |
| Models | `GET /routing/model-definitions/<channel>`; `PUT /config/oauth/model-alias/<channel>`, `PUT /config/oauth/excluded-models/<channel>` |
| Payload rules | `PUT /config/requests/payload/<kind>` |
| Usage | `GET /observability/usage/api-keys`; while the live view is on, `GET /observability/usage/live?after=<cursor>` every 3 s while visible |
| Logs | `GET /observability/logs` (cursor, every 3 s), `DELETE /observability/logs`, `GET /observability/logs/errors`, `GET /observability/logs/errors/<name>`, `GET /observability/logs/requests/<id>` |
| Configuration | `GET, PUT /config/<section>`, `GET, PUT /config.yaml` |
| System | `GET /server/latest-version` on request |

Config list writes reread the target first and refuse to write if it changed since it was shown. The API has no revision precondition, so this narrows the race window rather than closing it.

## Verification scripts

Both scripts are for disposable servers with fake credentials only: they edit configuration and credential files. Fixture requirements: management key `orb-dashboard-test-only`, a disabled fake Claude credential with email `operator@example.invalid`, routing strategy `round-robin`, and an empty `requests.payload.default`.

- `node scripts/browser-check-rust.mjs [url] [capture-dir]` drives the dashboard with agent-browser against the Rust binary (or Go): nine screens in both themes, unsupported actions disabled on Rust, a payload rule written and removed, a YAML diff discarded, 390 px layout, and no key in storage.
- `CHROME_PATH=... node scripts/panel-check-go.mjs [url] [capture-dir]` drives the single-file panel with Playwright against Go: all 12 screens in both themes, credential edits, a Codex OAuth start and cancel, config list writes, the section editor, request logs, the live usage view, phone layouts, and asserts that no request leaves the serving origin, Go is never probed, there are no console errors, and the key is not stored. `playwright-core` is a dev dependency for this script only; it downloads no browser.
- `node scripts/fake-logins.mjs [port]` serves fake provider login endpoints (Claude, Codex, Meta and xAI token, profile, device-code and key-mint routes) and a Claude usage endpoint that echoes its `Authorization` header. Fake tokens and `@example.invalid` emails only.
- `scripts/login-harness/` is a test build of cliproxy-rs whose only change is `Options.login_base`, which points every provider login endpoint at the fake. It is never shipped. Build it with `cp ../../../Cargo.lock . && CARGO_TARGET_DIR=../../../target cargo build` inside that directory.
- `CHROME_PATH=... node scripts/oauth-check.mjs <url> --mode fake` drives Claude and Codex sign-in through a pasted callback URL and Meta and xAI through the device code against the harness, and checks each new credential appears. `--mode dead` runs against the shipped binary with a dead `requests.proxy-url` and checks that a pasted callback ends in an honest failure.
- The script's former `--mode plugin`, which drove plugin sign-in with fake management responses, went with native plugin support (docs/DIFFERENCES-FROM-GO.md "Native plugins").
- `node scripts/api-call-check.mjs <base> [<base> ...]` sends the Accounts screen's `POST /requests/api-call` to the fake usage endpoint through each server, checks `$TOKEN$` substitution and that the quota parser reads the windows, and compares the response shape between servers.
- `CHROME_PATH=... node scripts/quota-check.mjs [dashboard-url] [capture-dir]` drives Overview and Accounts with fake management responses (Playwright, like `oauth-check.mjs`): failed checks retain live or passive limits and their timestamps, Claude 403 advice explains setup-token permissions, a successful retry clears errors, a second check within 2 minutes sends nothing, the drawer's Check quota re-enables by itself after its 2 minutes, and a partial batch delete keeps the failed account selected and names it. It makes no provider calls.
- The checks that sign in or send through a real server (`oauth-check.mjs` fake and dead modes, `first-run-check.mjs`, `api-call-check.mjs`) refuse to start unless the server reports an auth-dir whose real path, symlinks resolved, is strictly inside the system temp dir's real path (`scripts/private-auth-dir.mjs`; a path that doesn't resolve is refused too, and `npm test` covers it with disposable symlinks). The key is `oauth.auth-dir` (or the legacy root `auth-dir`). Anything else, such as `management.auth-dir`, falls back to `~/.cli-proxy-api`, where real logins live, and `isolated.sh` isolates the network, not the filesystem. Set `HOME` to a temp dir as well.
- `scripts/verify-backend.mjs` is the API-level verifier for the Go release backend, unchanged from the previous dashboard.

## Security

The management key lives in `src/api.ts` module memory and is cleared on sign-out; reloading signs out. Only the theme is stored. Downloaded credential files and configuration previews contain secrets; treat them accordingly. Do not use the dashboard over plain HTTP across a network.

## Fonts

Host Grotesk (SIL Open Font License, `public/fonts/OFL.txt`), subset to Latin and weights 400–650 with fontTools.
