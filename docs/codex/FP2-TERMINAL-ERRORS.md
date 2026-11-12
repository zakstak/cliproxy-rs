# FP2: first-party terminal request rejections

Tracking issue: [#116](https://github.com/zakstak/cliproxy-rs/issues/116), under tracker
[#66](https://github.com/zakstak/cliproxy-rs/issues/66). It depends on FP1
([#115](https://github.com/zakstak/cliproxy-rs/issues/115),
[FP1-SOURCE-REFRESH.md](FP1-SOURCE-REFRESH.md)) and R1
([#94](https://github.com/zakstak/cliproxy-rs/issues/94)).

This slice changes failure classification only. The proxy does not rewrite, add or drop
any request or response bytes. The contract pin stays `44ccf394573c`. The rules come
from researched main and are labelled as source-refresh evidence, as FP1 decided.
Message budgets, HTTP fallback and lane policy belong to FP4
([#118](https://github.com/zakstak/cliproxy-rs/issues/118)), and are out of scope here.

## Sources

All references are to `openai/codex` at research main
`5c3ff1dabf644169f4bd74b215719f381e4fdb3f`, read from a read-only clone.

| Source | What it decides |
| --- | --- |
| `codex-rs/codex-api/src/error.rs:92-162` `parse_terminal_provider_error` | The terminal code and status pairs: `inference_hook_denied` (none or 403), `context_length_exceeded` (none or 400), `invalid_access_program` and `unsupported_access_program` (none), `content_policy_violation` (none or 400), `access_program_not_enabled` (none). |
| `codex-rs/codex-api/src/api_bridge.rs:130-312` | HTTP and wrapped WebSocket statuses: the same parser with the status, then `access_program_not_enabled` on 403 (`/detail/code` or `/error/code`), and the workspace 403 (`invalid_workspace_selected`, `codex_workspace_access_denied`). |
| `codex-rs/codex-api/src/sse/responses.rs:375-450`, `sse/responses_error.rs:26-34` | In-stream `error` and `response.failed` events use the parser with no status. |
| `codex-rs/codex-api/src/endpoint/responses_websocket.rs:747-806`, `1416-1439` | A wrapped status frame is an HTTP error with its status. The context-length 400 is a context failure; the 503 can be retried. |
| `codex-rs/protocol/src/error.rs:470-534` `retry_policy` | Each of these rejections, and the workspace denial, is `RetryPolicy::Never`. A 5xx `UnexpectedStatus` gets `Backoff`. |
| Commits `1885417cd45a`, `65f27090521b`, `0811df397684`, `d9446b4b01aa`, `5b98eaafeb34`, `4b345a8de9b3`, `bb360e5db88c` | The changes since the pin that #116 lists. |

## Endpoint profiles

The rules apply on the `chatgpt` and `api-key` profiles, where the first-party source
reads these codes. `siwc` and `compatible` endpoints are not covered by any source for
these codes. The proxy uses the same classification there, because a request fault that
cannot fail over is the conservative reading. Loopback mocks stand for `chatgpt` and
`compatible`.

## Decisions

| Failure | Proxy classification | Why |
| --- | --- | --- |
| A terminal rejection with its first-party status, on HTTP, a wrapped status frame, or an in-stream event | Request fault: no failover, no cooldown, no token refresh | First-party never retries it. It concerns the request, or the workspace policy for that request. It does not show that the account is unhealthy. |
| The same codes with any other status | Unchanged, as Go v8.0.35 classifies them | First-party also reads them as ordinary statuses then. |
| An in-stream event with one of these codes | It takes the code's first-party status: 403 for `inference_hook_denied` and `access_program_not_enabled`, 400 for the others | The event has no status. A 403 keeps the native WebSocket account guard (#6), because the frame describes the account's workspace. |
| `context_length_exceeded` with a 5xx (HTTP or wrapped) | Transient. Go's 503 classification applies: retry round, failover, cooldown. The body is not rewritten to `context_too_large`. | `error.rs:110-112`. This prevents a context-window failure that the provider did not report. |
| `context_length_exceeded` in-stream, or with 400 | Unchanged context failure | First-party Codex reads no status in-stream. |
| Workspace 403 | Unchanged credential failure: failover and cooldown, no refresh | The proxy selects the workspace from the account's own credential, so the denial concerns that account. |

`cpa_exec::codex_terminal` holds the rules. The executor (`codex_response`) and the
server's request-fault check (`cpa_server::classify::is_request_fault`) both read them.
The server check applies to every retained provider, because these codes are OpenAI's
(docs/DIFFERENCES-FROM-GO.md).

## Mock evidence

All tests use loopback upstreams, fake tokens and private `auth-dir`s.

| Test | Asserts |
| --- | --- |
| `cpa-exec` `codex_terminal::tests::*` | Code and status pairs, exact codes, the 5xx context rule, and that a workspace denial is left to the credential rules. |
| `cpa-exec` `codex_response::first_party_terminal_tests::*` | HTTP status errors keep their bodies and get the right scope. In-stream events take the first-party status whatever inner status they name, and get no retry delay. The `ws-error-context-length-status` fixture: the 400 is a request fault and the 503 is a credential failure. A wrapped 403 rejection is withheld and a wrapped 400 one is not. |
| `cpa-server` `classify::tests::first_party_codex_terminal_rejections_decide_before_go` | Request-invalid, cooldown and retry-round decisions, the 5xx context rule beside Anthropic's type, and no refresh for a workspace 403. |
| `codex_native_request::first_party_terminal_rejections_stop_on_the_selected_account` | With a second account available: HTTP 403 and 400 rejections, and `error` and `response.failed` events as the first event and after output. Each has one upstream attempt with the selected token, no cooldown, one failed usage record, and the provider's status and bytes. |
| `codex_native_request::transient_and_workspace_failures_move_to_another_account` | A context-labelled 503 (with and without a type) and both workspace 403 forms each move once to the second account, which serves the answer. The first account is cooled. |
| `codex_native_ws::first_party_terminal_rejections_never_fail_over_on_websocket_turns` | On a socket and on a turn served over HTTP: one upstream frame or request, no cooldown, one failed usage record. 403 frames are withheld. Other frames, including the 400 fixture frame, arrive as sent. |
| `codex_native_ws::a_context_labelled_503_fails_over_on_websocket_turns` | The 503 fixture frame and a typed 503 move once to the second account. The client receives that account's answer and never the 503 frame, and only the first account is cooled. |

## Unresolved gates

- Live acceptance on any profile. Local mocks cannot show which status the provider sends
  with each code, or whether `siwc` or `compatible` endpoints send these codes at all.
- A withheld 403 rejection gives a native WebSocket client a silent close. The pinned
  client then reconnects and resends until its own retry budget ends. Delivering the frame
  needs evidence that a workspace-hook or access-program message never names the
  account.
- The `x-error-json` header code, which first-party reads for 403 workspace and
  access-program denials, is not read. A 403 access-program denial that only the header
  names stays a credential failure.
- A wrapped notice (a 2xx or non-HTTP status), or a decoded `error` frame with a status
  the wrapped reader refuses, is still checked only against the pinned terminal codes
  (`codex_ws::terminal_provider_error`). Main raises the new codes there too.
- No pin bump. CONTRACT.md line references stay at `44ccf394573c`.
