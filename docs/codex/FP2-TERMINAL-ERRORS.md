# FP2: first-party terminal request rejections

Tracking issue: [#116](https://github.com/zakstak/cliproxy-rs/issues/116), under tracker
[#66](https://github.com/zakstak/cliproxy-rs/issues/66). It depends on FP1
([#115](https://github.com/zakstak/cliproxy-rs/issues/115),
[FP1-SOURCE-REFRESH.md](FP1-SOURCE-REFRESH.md)) and R1
([#94](https://github.com/zakstak/cliproxy-rs/issues/94)).

This slice changes failure classification only, and only for Codex credentials. The
proxy does not rewrite, add or drop any request or response bytes. The contract pin
stays `44ccf394573c`. The rules come from researched main and are labelled as
source-refresh evidence, as FP1 decided. Message budgets, HTTP fallback and lane policy
belong to FP4 ([#118](https://github.com/zakstak/cliproxy-rs/issues/118)) and are out of
scope.

## Sources

All references are to `openai/codex` at research main
`5c3ff1dabf644169f4bd74b215719f381e4fdb3f`, read from a read-only clone.

| Source | What it decides |
| --- | --- |
| `codex-api/src/error.rs:92-162` `parse_terminal_provider_error` | Code and status pairs: `inference_hook_denied` (none or 403), `context_length_exceeded` (none or 400), `invalid_access_program` and `unsupported_access_program` (none), `content_policy_violation` (none or 400), `access_program_not_enabled` (none). |
| `codex-api/src/api_bridge.rs:130-312`, `342-354` | HTTP and wrapped WebSocket statuses: the parser with the status, then for a 403 the code from `/detail/code`, else `/error/code`, else the base64 `x-error-json` header. `invalid_workspace_selected`, `codex_workspace_access_denied` and `access_program_not_enabled` are terminal there. |
| `codex-api/src/sse/responses.rs:375-450`, `sse/responses_error.rs:26-34` | In-stream `error` and `response.failed` events use the parser with no status. |
| `codex-api/src/endpoint/responses_websocket.rs:747-806`, `906-925` | A WebSocket `error` frame is first offered to the wrapped mapping. Only an error status (100 to 999, not 2xx) makes it an HTTP error. Every other frame (no status, a 2xx, a status past 999, or one the struct cannot read) goes to `process_responses_event`, which reads no status. |
| `protocol/src/error.rs:470-534` `retry_policy` | Each rejection above, and `WorkspaceAccessDenied`, is `RetryPolicy::Never`. A `context_length_exceeded` with another status is `UnexpectedStatus`, which is retried with backoff. |
| `protocol/src/error.rs:703-732` `is_runtime_auth_denial` | Runtime-agent authentication denials (`runtime_assertion_invalid`, `runtime_instance_revoked`, ...). They are authentication failures, not FP2 rejections. |
| `core/tests/suite/websocket_fallback.rs:133-176` | A 403 with `{"detail":{"code":…}}` on the WebSocket upgrade and on HTTP is not retried and does not fall back. |
| Commit `4b345a8de9b3` | "A `403` with `codex_workspace_access_denied` indicates that the account cannot use Codex in the selected workspace. Retrying or falling back to HTTP does not resolve that denial." |

## Endpoint profiles

The fixtures cover `chatgpt`, and `api-key` where the first-party test is
transport-neutral. No source covers these codes on `siwc` or `compatible` endpoints; the
proxy applies the same reading to every Codex credential, which is a decision, not
evidence. Loopback mocks stand for `chatgpt`.

## Decisions

| Failure | Reading | Proxy classification |
| --- | --- | --- |
| An HTTP error, or a wrapped WebSocket status frame or upgrade refusal, with a terminal code and its first-party status | `Surface::Status` | `SourcedFault::RequestTerminal`: request fault. No failover, no cooldown, no token refresh. |
| A 403 workspace or access-program denial, in `/detail/code`, `/error/code` or `x-error-json` | `Surface::Status` | `RequestTerminal`. First-party does not retry it, and nothing in the source shows that the account is unhealthy for other requests. A 503 with the same body is an ordinary 503. |
| An in-stream event, or an unmapped WebSocket `error` frame, with a new code | `Surface::Stream` (no status) | `RequestTerminal`, classified with the code's first-party status: 403 for `inference_hook_denied` and `access_program_not_enabled`, 400 for the others. The pinned reader read on past such a notice; main fails it. |
| `context_length_exceeded` with any HTTP status except 400 | `Surface::Status` | `SourcedFault::Retryable`: no request fault, and no `context_too_large` rewrite. Its status decides (a 503 fails over and cools). A 403 whose body or `x-error-json` names a workspace or access-program denial is that denial first. |
| A first-party code (the stream codes and the two workspace codes) on any other status but 400 | `Surface::Status` | `SourcedFault::Retryable`: first-party retries it (`api_bridge.rs:223-312`), so Go's `invalid_request_error` type, 413 or 422 rule does not stop it. On 400 it is `RequestTerminal`: first-party reads it as `InvalidRequest` (`api_bridge.rs:188-222`), before anything its message says. |
| `context_length_exceeded` without a status, or with 400 | either | Unchanged context failure. An unmapped notice with this code, which the pinned reader read on, now fails as main does. |
| Workspace codes in a stream event | `Surface::Stream` | Unchanged. First-party has no stream reading for them. |
| 401, a bare 403, another 403 code, a runtime authentication denial | none | Unchanged credential failure. |

The reading comes first. A terminal rejection keeps its first-party status and its body as
sent, whatever its message says: Go's usage-limit, model-capacity and context rewrites do
not apply to it, and an in-stream one is never a bootstrap overload. Bodies, wrapped
frames and the `x-error-json` JSON are read as the first-party build's `serde_json::Value`
reads them: the last of a repeated key, numbers of any size, private tokens resolved. A 426 upgrade refusal keeps Go's plain rendering
and carries the reading too.

`cpa_exec::codex_terminal` reads the rules. The Codex executor carries the result on
`ExecError::sourced`, and the server's `classify` functions read it before Go's
provider-neutral request-fault rules: `is_request_invalid`, `is_unauthorized`,
`is_compact_fault`, and the WebSocket `exposed` check. Claude, xAI and Meta never set
`sourced`, so their failures keep Go's rules exactly.

## Delivery and account confidentiality

- The native HTTP route delivers the provider's status, body and IDs, as it does for
  every upstream error (CONTRACT.md "Native HTTP route"). In-stream events reach an HTTP
  client as sent.
- A native WebSocket client never receives a rejection classified as 403. That covers a
  workspace or access-program denial, an inference-hook denial, and an upgrade refusal.
  The #6 guard withholds the frame, and `exposed` keeps account statuses unexposed
  even when they are request faults. The client gets a silent close.
- Known limitation, unchanged by FP2: after a silent close, a first-party client treats
  the loss as a stream error. It reconnects and resends until its own retry budget ends.
  The proxy does not cool the account and does not fail over, so each resend is a new
  request, and the scheduler may pick another account for it. The client never sees a
  terminal frame for these rejections.

## Fixtures

| Fixture | First-party test |
| --- | --- |
| `sse-terminal-rejections` | `sse/responses_error_tests.rs:112-140`, with the FP2 codes only |
| `ws-error-terminal-rejections` | `api_bridge_tests.rs:364-409`, the wrapped form with the first message |
| `http-error-workspace-detail` | `api_bridge_tests.rs:831`, `core/tests/suite/websocket_fallback.rs:133-152` |
| `http-error-workspace-provider` | `api_bridge_tests.rs:830-836`, also the `x-error-json` payload |
| `ws-error-context-length-status` | `responses_websocket.rs:1416-1439` (FP1) |

The pinned-reader oracle fixtures (`codex_pinned_failure_readings.json` and
`codex_pinned_notice_readings.json`) stay as recorded at the pin. Their 143 decoded
`context_length_exceeded` frames that the pinned reader read on (62 and 81) are asserted
as failures, labelled as the main departure (`codex_ws_tests` `main_context_failure`).

## Mock evidence

All tests use loopback upstreams, fake tokens and private `auth-dir`s.

| Test | Asserts |
| --- | --- |
| `cpa-exec` `codex_terminal::tests::*` | Stream and status readings, the 403 match case for case with `only_an_explicit_workspace_403_stops_retries`, `x-error-json`, exact codes, the context rule, and runtime-auth controls. |
| `cpa-exec` `codex_response::first_party_terminal_tests::*` | HTTP bodies kept as sent, with scope and `sourced`. The context code at 403, 404, 413 and 5xx. Every `sse-terminal-rejections` event. Wrapped frames, `x-error-json` in frame headers, and notices at `200`, `1000`, `"503"`, `429.0` and `"403"`, with withholding. |
| `cpa-exec` `codex_ws_tests` pinned-reading tests | The recorded pinned decisions, with the 143 main departures counted. |
| `cpa-server` `classify::tests::sourced_codex_readings_decide_before_go`, `unsourced_failures_keep_go_rules` | Sourced readings decide, including no refresh for a body naming 401. Unsourced (non-Codex) failures keep Go's rules for the same bodies. |
| `codex_native_request::first_party_terminal_rejections_stop_on_the_selected_account` | With a second account available: 8 HTTP cases, plus 24 in-stream cases (as the first event and after output). Each has one attempt with the selected token, no cooldown, exactly one failed usage record, and the provider's status and bytes. |
| `codex_native_request::retryable_and_account_failures_move_to_another_account` | Context 503 (with and without a type), context 403, workspace 503, bare 403, another 403 code beside a workspace header, and a runtime denial each move once to the second account. |
| `codex_native_request::a_context_labelled_503_is_retried_once_per_account` | With `request-retry: 0`, one attempt per account, both accounts cooled, and the last body delivered. |
| `codex_native_ws::first_party_terminal_rejections_never_fail_over_on_websocket_turns` | 18 frames, on a socket and on a turn served over HTTP: one upstream frame or request, no cooldown, one failed record. 403 frames are withheld; the others arrive as sent. |
| `codex_native_ws::a_workspace_upgrade_refusal_stops_on_the_selected_account` | A 403 workspace upgrade refusal: one upgrade, no HTTP request, no cooldown, one failed record, silent close. The bare 403 control fails over. |
| `codex_native_ws::a_context_labelled_503_fails_over_on_websocket_turns` | The 503 fixture frame and a typed 503 move once to the second account. Only the first account is cooled. |

## Review corrections

A source review at `c4f968fb` found five defects, fixed together with unit and route
tests:

1. A model-capacity or usage-limit message turned a terminal rejection into an unsourced
   429, and an in-stream one into a bootstrap overload that failed over.
2. A first-party code on a status first-party retries became a request fault through Go's
   `invalid_request_error` type rule.
3. A context code made a 403 workspace denial retryable before the 403 match ran.
4. A 426 upgrade refusal carried no reading.
5. The status reading took the first of a repeated key; first-party's `Value` takes the
   last.

A second review at `b503dce5` found four more, fixed in the next commit:

1. FP2 classified an in-stream rejection with its paired status (for example 400), so a
   frame whose own error named an account status (an inner 403) or a usage limit was no
   longer withheld from a native client. The account-safety reading now also keeps Go's
   status for the frame (`codex_response::go_failure_status`), so such a frame stays
   withheld (#6), whatever FP2 classifies it with.
2. On 400, a first-party code and the context code could still become a 429 through Go's
   capacity or usage reading of the message, or an in-stream context failure a bootstrap
   overload. They are now read from the code first; a first-party code on 400 is
   `RequestTerminal`, and the context code keeps the proxy's context classification.
3. The translated routes classified failure events as written, so a repeated
   `response.error` read as its first occurrence. They now read the pinned view, as the
   native routes do.
4. A wrapped frame's `x-error-json` was trimmed before the reading. The reading now takes
   the frame's headers as first-party does, unchanged.

A third review at `9978805f` found two more, fixed in the next commit:

1. (#6) A context-window failure a Codex client receives ahead of its error (#5) was
   emitted before the account-safety reading, on a socket and on a turn served over HTTP.
   A frame that safety withholds is now never preserved either.
2. FP2 read its codes from the pinned view, which projects an accepted positional
   `response.error` array into the object the `Error` struct decodes. First-party checks
   terminal codes before that decode, so such an array names no code there and reaches the
   struct's default `Retryable` branch. The routes now classify with
   `codex_response::received_failure`, which reads FP2's codes from the logical value
   (`codex_event::logical_view`) and keeps Go's classification of the view; a positional
   array naming an FP2 code is `Retryable`.

A fourth review at `cadfe89a` found three more, fixed in the next commit:

1. A bootstrap guard passed an owned error where a reference was required, which would
   not compile.
2. A positional wrapped status frame was read for FP2 from its projected view. First-party
   maps it to the HTTP error of its status with the frame as received as the body, which
   names no `/error/code`, and the decoded struct's headers. FP2 now reads exactly that;
   Go's classification still reads the view.
3. FP2 took codes from Go's fallback error locations (a `response.failed`'s top-level
   `error`, an `error` event's top-level `code`). First-party reads only an `error` event's
   `error` and a `response.failed`'s `response.error`
   (`codex_terminal::event_rejection_status`); a code only Go's fallbacks name is
   `Retryable`, with Go's status.

A fifth review at `836cde5d` found three more, fixed in the next commit:

1. FP2's readers parsed with this build's `serde_json::Value`, which refuses a number past
   `f64` (for example `1e400`). The first-party build enables `arbitrary_precision`, so it
   reads such a body and its terminal code; the proxy fell back to an unsourced failure,
   and its account-safety readings missed the 403 too. Every FP2 reader now reads the
   pinned views (`codex_event::value_view` for a body read as a `Value`,
   `codex_event::logical_view` for an event read as its struct), which accept numbers of
   any size and resolve serde_json's private tokens as the first-party build does.
2. `Surface::Stream` was constructed only in tests, a `dead_code` warning that
   `clippy -D warnings` rejects. The in-stream reading is now its own function, and the
   status reading takes the status directly.
3. Reparsing a whole event as a `Value` refused a leading private-token member that the
   first-party event struct ignores as an unknown field. Events are now read with their
   own keys plain, as the struct reads them.

A sixth review at `9adc5188` found three more, fixed in the next commit:

1. Wrapped headers were read from the de-duplicated view, which keeps a repeated key in
   its last position. First-party decodes them into an insertion-ordered map, where a
   repeated key keeps its first position and takes its last value, so a different header
   could win in the `HeaderMap`.
2. That view also resolved a private token as the `headers` object's own key. First-party
   decodes `headers` as a `Map<String, Value>`, whose keys are plain. The headers are now
   read from the frame as received (`codex_event::wrapped_headers`), with those map rules
   and each value read as a first-party `Value`.
3. `value_view` trimmed Unicode whitespace before validating. It now trims only JSON
   whitespace, so a body led by U+00A0 is no JSON, as for first-party.

The S4 session at `9adc5188` ran out of disk space on `/home` after 25 exact targets and is
an incomplete run, not acceptance evidence.

A seventh review at `201fd7ca` found three more, fixed in the next commit:

1. Wrapped-header de-duplication scanned every earlier member for each header, quadratic
   in the frame. It now keeps a key-to-position index beside the ordered list.
2. FP2's status reading ran on a wrapped frame whose code is
   `websocket_connection_limit_reached` or `previous_response_not_found`; first-party
   returns `Retryable` for those before any status mapping. FP2 now reads nothing from such
   a frame, and Go's classification stays.
3. Translated routes applied FP2 to events the pinned client cannot decode, which
   first-party skips. Only a decoded event now gets a first-party reading.

The S5 session at `201fd7ca` passed every target, control and witness, but it predates
these fixes and is not acceptance evidence for them.

The S3 session at `c4f968fb` passed, but it predates these fixes and is not acceptance
evidence for them.

## Unresolved gates

- Live acceptance on any profile. Local mocks cannot show which status the provider
  sends with each code, or whether `siwc` or `compatible` endpoints send these codes.
- The silent-close reconnect limitation described above. Delivering a terminal frame
  needs evidence that these messages never name the account.
- An unmapped notice with `context_length_exceeded` is delivered as sent, or as #5's
  `response.failed` when its integer status reads as zero. The pinned client still reads
  on past the `error` frame it receives.
- No pin bump. CONTRACT.md line references stay at `44ccf394573c`.
