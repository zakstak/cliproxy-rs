# Claude Code 2.1.322 OAuth authorization and refresh

Evidence for [#76](https://github.com/zakstak/cliproxy-rs/issues/76) (C8): the OAuth `state`,
the authorization scopes and the refresh scopes that the first-party Claude Code client uses.
It moves neither the frozen Go v8.0.35 reference nor the
[selective v8.0.67 ledger](selective-v8.0.67.md). It reads the client itself, which the
provider-first rule in `AGENTS.md` puts ahead of Go.

## Provenance

| Item | Value |
| --- | --- |
| Client | `claude` 2.1.322 (Claude Code), native binary installed on the reading host |
| Binary SHA-256 | `bfb3703ad5380242328459bc62ada4bde8ae7ba57c6e1eeadb9c00e08874c478` |
| Date | Read 2026-11-12, re-read 2026-11-13 |
| Method | Static reading of the JavaScript bundled in the binary. The client was not run for this record, and no OAuth request was sent |
| Credentials | None |

## Bundled source

Excerpts, minified names kept:

```js
hv = "user:inference", o5 = "user:profile", vYn = "user:sessions:claude_code", dzt = "user:plugins"
c = "org:create_api_key"; n = [c, o5]; r = [o5, hv, vYn, "user:mcp_servers", "user:file_upload"]
function czt() { let e = [...r]; if (PLUGINS_SCOPE_REGISTERED) e.push(dzt); return e }
function fHo() { return L([...n, ...czt()]) }              // L: unique, in order

// pCo(): the authorize URL
K = oauthClient ? oauthClient.scopes : inferenceOnly ? [hv] : alsoRequestScope ? [...fHo(), alsoRequestScope] : fHo()
searchParams.append("scope", K.join(" ")); ...append("code_challenge_method", "S256"); ...append("state", n)

// refresh: scope is the token's own scopes when given, otherwise czt()
{grant_type: "refresh_token", refresh_token: e, client_id: s ?? CLIENT_ID, scope: M.join(" ")}

function A(e) { return e.toString("base64").replaceAll("+", "-").replaceAll("/", "_").replaceAll("=", "") }
function L() { return A(E(32)) }                           // PKCE verifier
function y() { return A(E(32)) }                           // state
```

## Reading

The production OAuth configuration in the same bundle (client ID
`9d1c250a-e61b-44d9-88ed-5944d1962f5e`) and its local configuration (`-local-oauth`) set
`PLUGINS_SCOPE_REGISTERED: !0` (true). The custom-OAuth configuration (`-custom-oauth`) and a
`CLAUDE_CODE_OAUTH_CLIENT_ID` override set it to false. So, against the production endpoints:

- Authorization asks for `org:create_api_key user:profile user:inference
  user:sessions:claude_code user:mcp_servers user:file_upload user:plugins` (`fHo()`,
  deduplicated).
- A refresh that is not given the token's own scopes asks for `user:profile user:inference
  user:sessions:claude_code user:mcp_servers user:file_upload user:plugins` (`czt()`), without
  `org:create_api_key`. Given the token's own scopes, it sends those.
- The `state` is 32 random bytes as unpadded base64url: 43 characters, 256 bits.

## What cliproxy-rs adopts

- The split: authorization asks for `org:create_api_key` ahead of the refresh scopes, and a
  refresh never asks for it. The code exchange sends no scope.
- The 256-bit base64url `state`, compared byte for byte on the callback.

## What cliproxy-rs does not adopt

- `user:plugins`. cliproxy-rs asks for the five scopes its saved logins were granted with:
  `user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload`
  on refresh, and those after `org:create_api_key` on authorization. That is exactly what Go
  v8.0.67 sends (`ea0b4712`), and Go v8.0.35 refreshes with the same five. A refresh may not
  ask for a scope the grant does not hold (RFC 6749 section 6). Logins saved by Go or by
  earlier cliproxy-rs releases were granted without `user:plugins`, so adding it to the
  refresh scope could get those refreshes refused. Whether Anthropic refuses them is not
  measured. cliproxy-rs makes no request that needs `user:plugins`. Adding the scope to
  authorization alone would make new logins differ from saved ones without a use for it.
- The inference-only, extra-scope and custom-client branches.
- The first-party authorize URL. The client builds it on `platform.claude.com` or
  `claude.com`, in its own parameter order. cliproxy-rs keeps Go's `claude.ai` URL and sorted
  parameters, which #76 does not change.

Go v8.0.35 sends a 128-bit hexadecimal state and uses the refresh scopes for authorization
too.
