# FP1: first-party source refresh and qualification matrix

Tracking issue: [#115](https://github.com/zakstak/cliproxy-rs/issues/115), under tracker
[#66](https://github.com/zakstak/cliproxy-rs/issues/66). Research: the official Codex
audit, `docs/research/codex-official-2026-11-11.md#fp1`, an orchestrator document not yet
on `fork`. Depends on
[N1](N1-NATIVE-PROTECTIONS.md) (#111) and H1 (#67).

This document compares first-party Codex at the contract pin with researched main, one
endpoint at a time. It records which fixtures the delta adds, and qualifies each
behavior per endpoint profile. **The contract pin does not move.**
[CONTRACT.md](CONTRACT.md) stays the authority at `44ccf394573c`. Fixtures from main are
labelled source-refresh evidence. No proxy behavior changes in this slice. Failure
classification belongs to FP2 (#116), frame fragmentation to FP3 (#117), message budgets
and HTTP fallback to FP4 (#118), and documented SIWC limits to FP5 (#119).

## Pins and ancestry

| Source | Revision | Status |
| --- | --- | --- |
| Contract pin | `44ccf394573cabb95e1f751a59f9d76f6f79faf1` (2026-11-07) | Authority. Unchanged. `cpa_exec::codex_contract::FIRST_PARTY_COMMIT`. |
| Researched main | `5c3ff1dabf644169f4bd74b215719f381e4fdb3f` (2026-11-12 01:42:53 UTC) | Source-refresh evidence. `RESEARCHED_MAIN_COMMIT` and the manifest's `research` list. |
| Latest release `rust-v0.167.2` | `ce21072c90d32c40af7a55b60b2fb35c84a43a6a` | Divergent maintenance branch. Not qualified here. |

GitHub compare, read on 2026-11-12: main is 164 commits ahead of the pin and 0 behind,
and the local count of non-merge commits is also 164. The release is 7 ahead and 333
behind, with merge base `cb1bc0ae4ed996c67a6e118bf79a7435f0d3f0d2`. Its 7 commits are
Windows launcher and exec-policy changes, opt-in incremental tools for OpenAI Responses,
Bedrock and GCP incremental-tool policy, and release notes. None of the main findings
below is a claim about the release, and the release's incremental-tool opt-in belongs to
T1/T2 (#79, #80). The local research clone is shallow, so the release ancestry comes from
GitHub compare and was not recomputed locally.

## Method

The diff covers wire-relevant paths between the two pins in the read-only clone:

```sh
git diff --stat 44ccf394573cabb95e1f751a59f9d76f6f79faf1 5c3ff1dabf644169f4bd74b215719f381e4fdb3f -- \
  codex-rs/codex-api codex-rs/core/src/client.rs codex-rs/protocol/src/models.rs \
  codex-rs/protocol/src/turn_input.rs codex-rs/tools codex-rs/model-provider-info \
  codex-rs/model-provider codex-rs/core/src/compact_remote_v2.rs \
  codex-rs/core/src/compact_remote_v2_attempt.rs codex-rs/core/src/responses_metadata.rs \
  codex-rs/login codex-rs/protocol/src/context_window_id.rs codex-rs/protocol/src/response_item_id.rs
```

The commit routing table in that research audit ("Source changes reviewed for the wire
audit") still applies. This document adds the per-endpoint reading of the diff. It is a source
audit. It is not runtime proof that a provider accepts any shape.

## Endpoint-by-endpoint delta

| Surface | First-party source | Delta pin to main | Fixture action |
| --- | --- | --- | --- |
| HTTP `POST /responses` request body | `codex-api/src/common.rs` `ResponsesApiRequest`; `core/src/client.rs` `build_responses_request` | None in `common.rs` or `endpoint/responses.rs`. The `client.rs` changes are runtime authentication, budget checks and prewarm fallback, with no body field change. | Keep `http-request-full`, `-lite`, `-minimal` and `-compaction` at the pin. |
| Request items (`ResponseItem`) | `protocol/src/models.rs` | Item `id` fields use `OptionalWireItemId` (`response_item_id.rs`), which serializes the same plain string. The allocation is kept only in client history (`340a18ed7cd0`). `FunctionCallOutputContentItem::InputAudio` now flattens `AudioReference`, so a tool output can carry `{"type":"input_audio","file_id":…}` (`9018f3a3b021`). At the pin it carried only `audio_url`. | **Add** `http-request-tool-output-media` (main). Model-facing IDs need no new fixture: the wire is unchanged, and existing fixtures carry IDs byte for byte. |
| `additional_tools` (Responses Lite) | `core/src/client.rs:1198-1213` at the pin | No structural change. `c3936b4b8a66` adds two optional catalog strings, `model_messages.tools.incremental_tools.exec_tools_update_hint` and `exec_tools_removed_definitions_header`, which change only the text of developer messages. | Keep `http-request-lite`. The new catalog fields are a K1/R5 (#68, #98) qualification item: the research audit's "no model catalog delta" reading missed these two optional members. |
| Compaction v2 | `core/src/compact_remote_v2*.rs` | None. | Keep `http-request-compaction`, `sse-compaction-v2` and the opaque probe. |
| Turn metadata (`client_metadata`, `x-codex-turn-metadata`) | `core/src/responses_metadata.rs` | `context_window_id` is now rendered through `Display` into a `String`. Both pins emit a string. | None. The value is opaque to the proxy. |
| SSE event decoding | `codex-api/src/sse/responses.rs`, `sse/responses_error.rs` | Five more terminal errors propagate (context window, invalid request, inference hook, access program, content policy). `parse_failed_response` no longer maps `context_length_exceeded` by code alone. | None in FP1. FP2 owns the classification and its fixtures. |
| Error classification | `codex-api/src/error.rs`, `api_bridge.rs` | New terminal codes: `inference_hook_denied`, `content_policy_violation`, `invalid_access_program`, `unsupported_access_program`, `access_program_not_enabled`. A 403 with `invalid_workspace_selected` or `codex_workspace_access_denied` is a workspace denial. `context_length_exceeded` is a context failure only without a status or with 400 (`1885417cd45a`). | **Add** `ws-error-context-length-status` (main) for FP2. FP1 asserts no behavior on it. |
| WebSocket transport | `codex-api/src/endpoint/responses_websocket.rs` | Text larger than 1 MiB goes as fragments (`d15bf1deb77e`, FP3). `x-codex-websocket-max-message-bytes` from the upgrade, and close 1009 before any text, mean the message is too large (`ed7ca22bb60a`, `bc3ccba6e1cb`, FP4). Interrupt, steer drain, prewarm and frame shapes are unchanged. | None in FP1. Fragmentation is a transport property, not a payload fixture. |
| Client session and authentication | `core/src/client.rs`, `model-provider/*`, `login/*` | Runtime-authenticated sessions reconnect before each request, and budget checks recheck auth ownership (`d4a341bd52cc`, `6bd17ba6ae6c`, `c4f0e09912fd`, `efdacaf46ff5`). A prewarm stream switches to HTTP on a fallback error. | Exclusion. N1 records the proxy-side fences. |

## Qualification matrix

Profiles follow [CONTRACT.md "Endpoint profiles"](CONTRACT.md#endpoint-profiles):
`chatgpt`, `api-key`, `siwc`, and `compatible`, the operator-set base URL. A loopback mock
can stand for `chatgpt`, `compatible` and `siwc`. The `api-key` profile is `compatible` on
`api.openai.com`, which a loopback cannot be. Its body handling is the same code path.

Source status values: **FP** means the pinned first-party client does it. **FP main**
means only researched main does it. **Docs** means OpenAI documentation covers it.
**Unknown** means no source covers it. Proxy status values: **pass-through pinned**,
**refused** (an actionable local error, nothing sent), and **owned elsewhere**.

| Behavior | `chatgpt` | `api-key` | `siwc` | `compatible` | Proxy evidence (local mocks) |
| --- | --- | --- | --- | --- | --- |
| HTTP SSE stream | FP, no delta | FP, Docs, no delta | Unknown, FP5 owns the documented limits | Unknown | `codex_qualify::http_streamed_and_buffered_across_profiles`, `codex_native_request::native_requests_and_events_pass_through_the_route_unchanged` |
| Buffered client | Not used. The proxy sends `stream: true` and builds the answer. | Docs, passed unchanged | Unknown | Unknown | `codex_native_request::buffered_clients_get_the_destination_contract`, `codex_qualify::http_streamed_and_buffered_across_profiles` |
| WebSocket create | FP, no delta | FP, Docs | Unknown | Unknown | `codex_qualify::websocket_turns_across_profiles_and_transports`, `codex_native_ws::native_creates_and_events_pass_through_unchanged` |
| WebSocket continuation and prewarm | FP, no delta | FP, Docs | Unknown | Unknown | `codex_native_ws::a_prewarm_reaches_upstream_and_its_continuation_stays_on_the_socket`, `a_lost_socket_never_replays_a_continuation` |
| Steering (interrupt drain, then create) | FP, no delta | FP | Unknown | Unknown | `codex_native_ws::interrupt_then_a_new_create_follows_the_first_party_sequence` |
| `response.steer` | Not used | Docs, GPT-6 family | Unknown | Unknown | Pass-through, no claim it works: `codex_native_ws::operator_rules_cannot_rewrite_native_frames`, `steering_later_frames_reach_the_socket_as_sent` |
| Interruption | FP, no delta | FP | Unknown | Unknown | `codex_ws::tests::interrupt::*`, `codex_qualify::interrupted_and_truncated_streams_across_profiles`, `websocket_interrupt_tests::*` |
| Compaction v2 and `/responses/compact` | FP, no delta / not used | FP / Docs | Unknown | Unknown | `codex_qualify::compaction_across_profiles`, `codex_native_request::compaction_passes_through_the_route_unchanged` |
| Account replacement (failover before delivery, a disabled account, socket replacement) | Proxy policy, no first-party source | same | same | same | `codex_native_ws::an_early_overload_fails_over_and_the_continuation_stays_on_the_serving_socket`, `an_account_disabled_between_turns_serves_no_later_turn`, `a_replacement_socket_owns_its_loss_after_failover`. N1 witness W1 recorded uncertain replay at the audit baseline (`0e8463bd`). Merged PR [#135](https://github.com/zakstak/cliproxy-rs/pull/135) fixes that gap ([N1 disposition](N1-NATIVE-PROTECTIONS.md#disposition-in-the-safety-fix-branch)): `an_uncertain_loss_is_never_replayed_on_another_account` and `an_uncertain_http_loss_is_never_replayed_on_another_account` assert that an attempt the upstream may have read is request-scoped and never replayed on another account. Only a provably unsent attempt may fail over. |
| Large WebSocket messages | FP main: 1 MiB fragments | FP main, Docs | Unknown | Unknown | Owned elsewhere: FP3 (#117). The proxy sends one frame today. |
| Peer message budget and 1009 before text | FP main | FP main | Unknown | Unknown | Owned elsewhere: FP4 (#118). |
| New terminal error codes and status-aware context length | FP main | FP main | FP5 docs add SIWC codes | Unknown | Owned elsewhere: FP2 (#116). Fixture `ws-error-context-length-status`. |
| Profile-specific unsupported features | Responses Lite: FP; `stream: false` acceptance unknown | Responses Lite: Unknown | All Responses behavior: Unknown at the pin; preview limits documented, FP5 | All: Unknown | `response.append` refused (`codex_native_ws::an_append_is_refused_on_a_codex_connection`), HTTP-fallback prewarm refused (`http_fallback_is_a_separate_explicit_conversion`), unsupported native inputs refused (`codex_native_request::unsupported_native_inputs_are_refused_before_the_send`). No payload is stripped to fit a profile. |
| Exact bytes: `additional_tools` | FP | Unknown | Unknown | Unknown | `codex_native_request::responses_lite_keeps_its_body_and_marker`, `codex_qualify` (`http-request-lite` on every profile) |
| Exact bytes: model-facing IDs | FP, wire unchanged on main | FP | Unknown | Unknown | Every fixture comparison carries item IDs byte for byte (`http-request-full` on every profile and transport) |
| Exact bytes: opaque reasoning and compaction | FP | FP | Unknown | Unknown | `codex_qualify` (`http-request-full`, `-compaction`), `codex_native_ws::compaction_v2_answers_pass_unchanged_on_websocket_turns` |
| Exact bytes: multimodal tool results | FP main: file-backed audio; FP: inline audio, images | FP main | Unknown | Unknown | **New:** `codex_qualify` (`http-request-tool-output-media` on every profile and transport), `codex_native_request::tool_output_media_reaches_the_upstream_byte_for_byte`, `codex_native_ws::tool_output_media_reaches_the_upstream_socket_byte_for_byte` |

## Decision on the pin

Keep `44ccf394573c` as the contract pin. Main is adopted only partially, as labelled
fixtures that preserve bytes. A pin bump would need FP2, FP3 and FP4 implemented and
reviewed, plus contract text for each changed decoder rule. Line references across
CONTRACT.md would also have to move to main. None of that is done here, so moving the pin
would claim compatibility the proxy does not have. A fixture that cites main sets
`revision` in the manifest and says in its notes that it does not move the contract pin.
`codex_contract::tests::research_revisions_never_replace_the_contract_pin` enforces this.

## Exclusions

These main changes are client application behavior. The proxy keeps their valid wire
bytes and copies none of their state machines:

- History and persistence: model-facing ID allocations (`340a18ed7cd0`), typed context
  window IDs (`4f8205c80841`), thread-local model ID state (`77cbcdb4a6dd`), and deferred
  MCP output in thread listings (`d2b4d63e95aa`).
- UI and app-server display: display instructions (`202b590e84d0`, `8a4b226ad5d6`), the
  Code Mode MCP content helper (`733a1cca06f0`), and sandbox labels and templates
  (`0e6f153732cc`, `c93732887e2e`).
- Host budget callbacks and auth ownership rechecks (`c4f0e09912fd`, `6bd17ba6ae6c`).
- Runtime authentication: reconnect, terminal denials, analytics auth, workload identity
  (`d4a341bd52cc`, `efdacaf46ff5`, `cce9d3228b5d`, `3ff5d6223b4f`), and gateway OAuth
  catalog discovery (`6222faa3c463`, a K1/R5 qualification item).
- Realtime attachment deadlines (`9f04d886f366`).

## Unresolved gates

- Live acceptance of every row on every profile. Local mocks cannot show it
  ([QUALIFICATION.md §5](QUALIFICATION.md#5-unproven-live-gates)).
- Whether any endpoint accepts a file-backed `input_audio` in a tool output.
- The FP2, FP3, FP4 and FP5 behaviors listed above.
- The two new incremental-tool catalog members, for K1/R5.
- The release branch `rust-v0.167.2` and its incremental-tool opt-in, for T1/T2.

## Execution evidence

| Item | Value |
| --- | --- |
| Toolchain | `rustc 1.98.0`, `cargo 1.98.0` (local). Ember 1.99 final gate not run. |
| Builds | `cargo test --offline -p cpa-exec --lib`, and `-p cpa-server --test codex_native_ws --test codex_native_request --test codex_qualify`, with `--no-run`, `CARGO_BUILD_JOBS=2`, `nice -n 19`, debug 0, under the shared heavy-build lock |
| Lint | `cargo fmt --all -- --check`. `cargo clippy -p cpa-exec --lib --tests --features test-support -- -D warnings`. `cargo clippy -p cpa-server --test codex_native_ws --test codex_native_request --test codex_qualify -- -D warnings`. All clean. |
| Results | `cpa-exec` `codex_contract` 6 passed, `boundary` 16 passed (the new request fixture passes the request boundary), `codex_native_request` 34 passed, `codex_native_ws` 87 passed, `codex_qualify` 6 passed. All ran in the network-denied namespace described in N1. |
| Not run | Full workspace build and `clippy --all-targets`, the Go differential harness, any live provider traffic |

The rows above are the FP1 author's runs at the time of PR #130. They are kept as
recorded and are not independent evidence.

A later integration run covers the current tree. Merge-train session J ran on
`a2eea19c`, whose tree `37f2dab9` is the tree of `fork` merge commit `005118f0`
(PR [#141](https://github.com/zakstak/cliproxy-rs/pull/141)), and includes the N1 fixes
from PR #135. Its per-target verdict is PASS for 75 of 75 expected targets, with no
problems. The Codex targets in this document report `codex_native_ws` 98 passed,
`codex_native_request` 35 passed and `codex_qualify` 6 passed, including both uncertain-loss
regressions. The run used local mocks only. It did not run Rust 1.99.0, PostgreSQL, the Go
differential harness or any live provider traffic. This evidence leaves the contract pin,
the unknowns above, and the FP2, FP3, FP4 and FP5 ownership unchanged.
