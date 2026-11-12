# N1: native Codex protection audit

Tracking issue: [#111](https://github.com/zakstak/cliproxy-rs/issues/111), under tracker
[#66](https://github.com/zakstak/cliproxy-rs/issues/66). The selective research plan and
execution record (`docs/SELECTIVE-UPSTREAM-PLAN.md#n1`,
`docs/SELECTIVE-UPSTREAM-EXECUTION.md`) are orchestrator documents not yet on `fork`.

This audit checks the native Codex WebSocket protections that the selective plan assumed
the fork already had. It adds assertions only where a protection exists and no test pinned
it. It changes no production code. Where a protection does not exist, it records a bounded
witness for the orchestrator to route; it does not port the behavior.

## Pins and authority

| Source | Revision | Role |
| --- | --- | --- |
| Fork baseline | `5565231e2f935e2cf93903a61cfabd872ca0cf3a` (branch base `df328be7`, docs only since) | Code under audit |
| First-party Codex contract | `openai/codex@44ccf394573cabb95e1f751a59f9d76f6f79faf1` | Decides native payload, transport and identity ([CONTRACT.md](CONTRACT.md)) |
| Selective Go target | CLIProxyAPI v8.0.67, `8bba7a77c6bcbfd993516d26606bfecaa4ff49d0` | Source of the N1 anchors; never decides a native Codex payload |
| Frozen Go reference | CLIProxyAPI v8.0.35, `ae3ee3be303fb07fb8173279923413005bf32177` | Unchanged |

Go anchors are reference patches, not port instructions. Where Go and the first-party
contract disagree, the contract wins, and the existing difference entry stays.

## Results

Status values:

- **Present, pinned**: the protection exists and a named test asserts it.
- **Present, newly pinned**: the protection exists; this audit adds the assertion.
- **Deliberate difference**: Rust differs from the Go anchor on purpose, with a contract or
  difference-ledger entry.
- **Missing, witness**: the protection does not exist. A witness test fails at the audited
  head. Not fixed here.
- **Not adopted**: a selective Go change the fork does not have, with no first-party
  source. Routed, not fixed.

| Protection | Go anchor | Status | Rust implementation | Assertions |
| --- | --- | --- | --- | --- |
| GPT Image 2.6 built-ins | `7a7d37ec` | Present, pinned for `-flare` and `-sunburst`. The bare `gpt-image-2.6` that merged Go adds is **not adopted**. | `cpa-common/src/codex_catalog.rs`, `cpa-exec/src/codex_images.rs` | `codex_image_models.rs` (8 tests), `codex_catalog_tests::gpt_image_built_ins_are_hidden_image_models`. `DIFFERENCES-FROM-GO.md` records "no bare `gpt-image-2.6`" from open PR #7261 at `4d5658e4`. |
| Terminal 429 on the Responses WebSocket | `b5e6027c` | Present, pinned; deliberate difference in delivery | `cpa-server/src/websocket_limit.rs`, `codex_ws::account_sensitive` | `ws_limit_errors.rs` (11 tests: `all_accounts_exhausted_deliver_the_structured_limit`, `first_frame_rejection_delivers_the_structured_limit`, `pinned_continuation_at_its_limit`, `limit_after_streamed_output*`, `native_account_failure*`). Account failures are withheld; an official Codex client gets the allowlisted limit event (#6, CONTRACT "Responses and failures"). |
| Early 429 or overload fails over without closing the client | `86a1aabf` | Present, pinned for 429; **newly pinned** for overload, with and without bootstrap buffering, and for continuation ownership afterward | `codex_ws::bootstrap`, scheduler failover on retryable status | `ws_limit_errors::one_exhausted_account_fails_over_without_an_error`, `codex_ws::tests::bootstrap_overload_fails_over_without_disconnect_notice`, `codex_native_ws::positional_handshakes_keep_bootstrap_failover_before_delivery`, new `an_early_overload_fails_over_and_the_continuation_stays_on_the_serving_socket` |
| Hold `response.metadata` during bootstrap buffering | `9e79552f` | **Missing, witness W3** | `codex_response::bufferable` holds `codex.response.metadata` but not `response.metadata` | Witness `n1_witness_moderation_metadata_keeps_bootstrap_failover` fails: the overload after a moderation `response.metadata` reaches the client with one dial. |
| `turn_rate_limit_exceeded` as transient capacity | `9a35fa12` | **Not adopted** | None. No first-party Codex source at either pin names the code. | Classification policy. Route with FP2 (#116) or R1 (#94). |
| Upstream loss: no replay after uncertain execution | `f73e66ac` | Present, **newly pinned** after delivery. **Missing, witness W1** before delivery. | Transport-scoped loss (`codex_ws::transport`, `FailureScope::Transport`) may fail over | New `a_loss_after_delivery_never_replays_the_create_on_another_account`. Witness `n1_witness_uncertain_loss_before_delivery_is_not_replayed` fails: a create the upstream read, followed by a close 1000, close 1011 or dropped connection before any client event, is sent again on the second account. With bootstrap buffering, this also happens after the upstream sent `response.created`. |
| Upstream loss keeps the client open | `f73e66ac`, `fa42f390` | Deliberate difference | The client connection closes and the client recovers (CONTRACT "Socket and account ownership"; DIFFERENCES "Codex upstream-socket loss during a turn's failover") | `ws_e2e::upstream_loss_between_turns_closes_the_client`, `codex_native_ws::a_lost_socket_never_replays_a_continuation`, `a_replacement_socket_owns_its_loss_after_failover`, `a_retained_socket_loss_does_not_end_a_turn_served_elsewhere` |
| Late interrupts after the terminal event | `9717e325` | Present, pinned; deliberate difference (Rust forwards a late interrupt when no later turn reads, and waits for the acknowledgement) | `codex_ws::Pool::interrupt`, `websocket::Control::interrupt` | `codex_ws::tests::interrupt::*` (13), `late_interrupts_for_finished_responses`, `interrupt_needs_a_turn_reading_on_the_socket`, `websocket_interrupt_tests::*`, `ws_e2e::late_interrupt_does_not_poison_the_next_turn` |
| Interrupt and steer ownership by the serving credential | `b9bb031d` (credential half) | Present, pinned | `enabled(credential)` checks in `codex_ws::Pool::interrupt` and `codex_duplex` | `codex_ws::tests::interrupt_needs_a_turn_reading_on_the_socket` (disabled credential), `ws_e2e::steering_disabled_credential_cannot_send_another_frame` |
| Per-turn credential ownership on an ordinary connection | `b9bb031d`, `3e7a21e2` | Present, **newly pinned** | Per-turn selection (`websocket::Connection::usable`) and a continuation that needs the socket holding its previous response (`codex_ws::stream_ws_turn`), with at least one more layer that this audit did not isolate (see the mutation check) | New `an_account_disabled_between_turns_serves_no_later_turn` |
| Per-turn client principal reauthorization | `b9bb031d`, `3e7a21e2` | **Missing, witness W2** | Client keys are checked only at the upgrade (`access::require_client_key`). The connection keeps its `Caller`. | Witness `n1_witness_a_revoked_client_key_admits_no_later_turn` fails: after `api-keys` drops the connection's key, the next `response.create` still reaches upstream, while a new connection gets 401. |
| Duplex turns bound to the client model after interceptor rewrites | `6340fe43` | Not applicable | The fork has no interceptors (#47). Only the operator alias routes `model`. | `codex_native_ws::each_request_on_a_reused_socket_judges_the_alias_rule_itself`, `ws_e2e::steering_model_switch_needs_replay` |
| Terminal teardown releases the upstream socket | `9717e325`, `f73e66ac` | Present, **newly pinned** | Session release on connection end; failure invalidation | New `a_terminal_end_releases_the_upstream_socket`; existing `websocket_tests::upstream_loss_while_the_client_stalls_releases_the_session`, `a_client_close_cancels_a_stalled_steering_turn`, `removing_a_credential_closes_only_its_upstream_sockets` |
| Go test race fixes | `84e83441`, `23e31fe2` | Not applicable | Go test harness only | None |

### Official source notes since the contract pin

Official Codex main at `5c3ff1dabf644169f4bd74b215719f381e4fdb3f` revalidates auth ownership
after asynchronous budget checks
([`6bd17ba6ae6c`](https://github.com/openai/codex/commit/6bd17ba6ae6ca2788026bd789f28f38b754d7379))
and reconnects runtime-authenticated requests before each request
([`d4a341bd52cc`](https://github.com/openai/codex/commit/d4a341bd52cc47ff635761334bde8a972d6de943)).
Both are client host behavior. The proxy equivalent is the per-turn credential fence
(present, pinned above) and the per-turn client principal (missing, W2). A
runtime-authenticated client that reconnects for each request sends a full create on a
new connection. The proxy already serves that as a first turn and refuses an old
`previous_response_id` locally (`codex_native_ws::a_lost_socket_never_replays_a_continuation`).
No client host extension, budget callback or reconnect policy is copied.

## Defect witnesses for routing

The witnesses are not committed. They compile against this branch and fail on their
intended assertion at the audited head. Each one names its evidence source. None of them
cites a first-party Codex source as a requirement. The orchestrator decides routing.

| ID | Witness | Evidence | Gap | Design questions |
| --- | --- | --- | --- | --- |
| W1 | `n1_witness_uncertain_loss_before_delivery_is_not_replayed` | Go v8.0.67 `sdk/cliproxy/executor/websocket.go` `UpstreamWebsocketTurnError` ("The turn was not replayed and its outcome may be unknown") | A create the upstream read can run twice on two accounts. | The contract closes the client on loss, while Go keeps it open with an `upstream_websocket_lost` error. Write failures before the frame left still retry once (`shouldRetryCodexWebsocketSend`). A fix should separate "not sent" from "sent, outcome unknown" and update CONTRACT "Socket and account ownership". |
| W2 | `n1_witness_a_revoked_client_key_admits_no_later_turn` | Go v8.0.67 `b9bb031d`, `3e7a21e2` | A revoked client key keeps its open connection. | Reauthorize each turn against the bound principal, and decide the refusal frame and close behavior. Client keys are not a Codex payload concern. A difference entry is needed either way. |
| W3 | `n1_witness_moderation_metadata_keeps_bootstrap_failover` | Go v8.0.67 `9e79552f`, upstream capture from 2026-10-21 (Go issue #6856) | Under `stream-bootstrap-buffering`, an overload after `response.metadata` leaks to the client. | Holding a frame changes no bytes. The pinned client reads `response.metadata` headers (`sse/responses.rs:193-222`), so held metadata must still be delivered in order. |

### Disposition in the safety-fix branch

The table and the witnesses above are the audit as reviewed at `0e8463bd` (PR #130). The
dependent branch `opus/n1-safety-fixes` implements the three missing protections. The
audit evidence above keeps its original limits: author runs, local mocks, no live
provider proof.

| ID | Fix | Regression tests (local mocks, author runs) |
| --- | --- | --- |
| W1 | Writes report whether the frame provably never left (`codex_ws::WriteFailure`). A read loss after a successful write, and a write that may be partial, are request-scoped (`codex_ws::uncertain`): no failover, no cooldown. The client closes as on any lost socket. Only a provably unsent frame is retried once on a fresh socket. | `codex_native_ws::an_uncertain_loss_is_never_replayed_on_another_account`; `codex_ws::tests::only_refusals_before_queueing_prove_a_write_unsent`, `a_socket_whose_loss_was_recorded_is_not_written`, `an_uncertain_loss_is_request_scoped_and_otherwise_unchanged`; reworked `a_write_the_upstream_never_reads_times_out`, `a_send_retried_on_a_fresh_socket_returns_that_sockets_handshake`, `a_replacement_socket_never_inherits_the_old_sockets_model` |
| W2 | `websocket::ClientAuthority` re-runs the upgrade's client-key check for every turn, interrupt and duplex frame. The executor session holds it (`codex_ws::Authority`), so it is checked again right before each write. The bound principal is never replaced. | `codex_native_ws::a_revoked_client_key_admits_no_later_send_on_a_reused_socket`, `a_create_queued_before_revocation_is_refused_when_it_would_be_sent`, `a_revoked_client_key_cannot_interrupt`, `another_accepted_key_never_replaces_the_bound_principal`; `codex_duplex::tests::a_revoked_client_cannot_send_a_later_create_and_stays_connected`, `a_create_admitted_before_revocation_is_not_written_after_it` |
| W3 | `codex_response::bufferable` holds `response.metadata`. | `codex_native_ws::bootstrap_buffering_holds_response_metadata_and_releases_it_in_order`, `codex_native_request::bootstrap_buffering_holds_response_metadata_on_http`, `codex_response::bufferable_tests::response_metadata_is_held_and_output_is_not` |

An independent review of the first fix head (`d6426b32`) found four more defects, fixed in
the same branch:

- Authority was checked before the write waited for the socket. It is now decided at
  write admission (`codex_ws::write_phases`), after the sink lock, readiness and earlier
  flushes, for creates, duplex frames and interrupts.
- The WebSocket HTTP fallback never checked the authority. It now checks before each
  attempt dials and again when the HTTP client takes the body (`proxy::GatedBody`).
  Revoked during the dial, no request byte left.
- The native HTTP route and the fallback replayed requests the upstream may have read.
  They now track send certainty (`proxy::send_routed_certain`): only a failed connection
  or an unsent deadline fails over.
- The bootstrap deadline was checked only when the next frame arrived. It is now a timer
  (`Turn::frame_until`).

Regression tests: `codex_ws::tests::each_write_phase_reports_whether_the_frame_left`,
`a_create_waiting_for_the_socket_is_refused_after_revocation`,
`an_interrupt_waiting_for_the_socket_is_refused_after_revocation`,
`a_key_revoked_while_an_http_turn_dials_sends_no_request`;
`codex_duplex::tests::a_create_waiting_for_the_socket_is_refused_after_revocation`;
`codex_native_ws::an_uncertain_http_loss_is_never_replayed_on_another_account`,
`provably_unsent_and_rejected_http_requests_still_fail_over`,
`a_revocation_during_an_http_attempt_stops_the_failover_attempt`,
`held_metadata_is_released_at_the_bootstrap_deadline`,
`metadata_beyond_the_bootstrap_bound_passes_in_order`. The translated route keeps its
transport retry policy.

## Execution evidence

| Item | Value |
| --- | --- |
| Toolchain | `rustc 1.98.0 (88d9e12ae 2026-08-18)`, `cargo 1.98.0` (local). Ember 1.99 final gate not run. |
| Build | `CARGO_BUILD_JOBS=2 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 nice -n 19 flock /tmp/cliproxy-selective-heavy-build.lock cargo test --offline -p cpa-server --lib --test codex_native_ws --test ws_e2e --test ws_limit_errors --test duplex_dispatch --test codex_image_models --test codex_routed_images --test codex_models --no-run`, and `-p cpa-exec --lib --no-run` |
| Runtime isolation | Each test binary ran in an unprivileged user and network namespace with only loopback up, UID preserved, no capabilities, and a private temporary `HOME`. The `auth-dir` of each test is a private temporary directory. Upstreams are loopback mocks with fake tokens. |
| Results at the working tree | `codex_native_ws` 86 passed. `ws_e2e` 69. `ws_limit_errors` 11. `duplex_dispatch` 4. `codex_image_models` 8. `codex_routed_images` 1. `codex_models` 5. `cpa-server` lib `websocket` filter 76. `cpa-exec` lib 547 passed, 1 ignored (pre-existing Claude `unverified_go_cancellation_cases`). |
| New tests, repeated | Three consecutive runs of the four new tests passed. |
| Regression control vs witness | The four new tests are regression controls. They pin existing behavior and pass at the base. The three witnesses fail at the base. |
| Mutation check | Classifying `server_is_overloaded` as a 400 request fault fails `an_early_overload_fails_over_and_the_continuation_stays_on_the_serving_socket`. Two single-point mutations of the disabled-credential path did not fail `an_account_disabled_between_turns_serves_no_later_turn`: dropping `!credential.disabled` from `Connection::usable`, and letting a continuation use any session socket, separately and together. Another layer still refuses the turn. It was not isolated. `CredentialStore::upsert` does not close sessions, so session closing is not that layer. No mutation was run for the loss and teardown tests. |
| Not run | Full workspace build, `clippy` on all targets, Go differential harness, live provider traffic |

Evidence files from the run are outside the repository and summarized here: the probe patch
and its output, the witness patches and their failing output, the mutation log and the
suite log.
