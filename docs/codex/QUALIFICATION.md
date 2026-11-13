# Codex migration: qualification

This page is the receipt for the Codex migration ([#20](https://github.com/zakstak/cliproxy-rs/issues/20)), written for its final qualification step ([#29](https://github.com/zakstak/cliproxy-rs/issues/29)). It keeps three kinds of evidence apart:

1. Source evidence: what the first-party client and the provider documentation define ([CONTRACT.md](CONTRACT.md)).
2. Local mock results: what the server does at its real ingress and egress, against loopback upstreams, with fake credentials and private `auth-dir`s.
3. Unproven live gates: what no local test can show (section 5).

Qualification covers functional behaviour and security: exact bytes, routing, failure handling and credential isolation. Performance is not part of it (section 3).

No result here comes from a real account, and none was sent to a provider.

Native plugin removal ([#47](https://github.com/zakstak/cliproxy-rs/issues/47)) later deleted the plugin hooks this page checks against, with `codex_native_plugins.rs` and the Go plugin modules. The plugin rows below are a snapshot of the migration; [CONTRACT.md](CONTRACT.md#settings) now refuses a `plugins` section.

## 1. Source evidence

[CONTRACT.md](CONTRACT.md) is the authority. It pins the first-party client (`openai/codex` at `44ccf394573c`) and the OpenAI API documentation, and per endpoint profile and transport it records:

- the [transport matrix](CONTRACT.md#transport-matrix);
- the [permitted transformations](CONTRACT.md#permitted-transformations);
- the [request boundary](CONTRACT.md#request-boundary);
- the native HTTP, native WebSocket and translated routes;
- the [settings outcomes](CONTRACT.md#settings).

[IDENTITY.md](IDENTITY.md) records the identity and transport policy. The contract fixtures (`crates/cpa-exec/tests/fixtures/codex_contract/`) are hand-written from those sources; each names its evidence in `manifest.json`. Go recordings stay historical regression evidence and define nothing here.

## 2. Local mock results

Every test below runs through the real router (`cpa_server::router`) or the executor, against capturing loopback HTTP and WebSocket upstreams, with fake tokens and a private `auth-dir`. Each one compares exact bytes, and applies any permitted edit to the client's bytes explicitly: expected native bytes come from fixtures or the client's own input, never from proxy output.

### The cross-path matrix (this PR)

[`crates/cpa-server/tests/codex_qualify.rs`](../../crates/cpa-server/tests/codex_qualify.rs) runs every pinned native request through every transport and profile a loopback can stand for. That covers `chatgpt`, `compatible` and Sign in with ChatGPT. The `api-key` profile is `compatible` on `api.openai.com`, which a loopback cannot be; its body handling is the same as `compatible`'s.

The requests are the full, Lite, minimal and compaction-v2 requests, the unknown-fields probe and a 1 MB tool-schema body. Between them they carry unknown members at every depth, `null`, `false` and omitted members, raw number spellings and escapes, cache options and encrypted content.

| Test | What it shows |
| --- | --- |
| `http_streamed_and_buffered_across_profiles` | HTTP streamed and buffered, every body, every profile. The upstream gets the client's bytes; a buffered `chatgpt` client's gets `stream: true`, the one permitted change. The client gets the fixture's events, the terminal `response` object or the destination's JSON, unchanged. The upstream sees only the credential's token: the client's key and cookie never cross. |
| `websocket_turns_across_profiles_and_transports` | WebSocket turns on an upstream socket and over HTTP, every body, every profile. The frame reaches the socket as sent. Over HTTP it goes without `type` and with `stream: true`. The client gets each event's payload unchanged, and credentials are isolated as above. |
| `compaction_across_profiles` | `POST /responses/compact` on every profile, body and answer unchanged. |
| `an_alias_routes_only_the_model_on_every_transport` | An operator alias changes only the `model` value, over HTTP and over a WebSocket turn on HTTP. |
| `interrupted_and_truncated_streams_across_profiles` | An interrupted `response.incomplete` passes unchanged. A stream that ends without a terminal event ends for the client after the events that arrived, and nothing is invented. |
| `structured_failures_across_profiles` | A usage-limit 429 keeps its status and body. A context-window `response.failed` arrives as the provider's event. |

### Ownership matrix

Each acceptance item is owned by the PR and tests listed. This PR adds the cross-path matrix above and links the rest instead of duplicating them.

| Acceptance item | Owner | Tests |
| --- | --- | --- |
| Native HTTP streaming, buffered, compaction | #23, #25 | `codex_native_request.rs`: `native_requests_and_events_pass_through_the_route_unchanged`, `buffered_clients_get_the_destination_contract`, `compaction_passes_through_the_route_unchanged`, `compaction_v2_answers_pass_unchanged` (the `sse-compaction-v2` first-party-test answer and the `probe-sse-compaction-v2-opaque` probe, streaming and buffered; #25), `private_and_unknown_events_reach_codex_clients`, `codex_request_boundary_refuses_before_the_upstream_send`; added later for #23: `codex_client_paths_take_the_same_native_routes`, `a_zstd_body_reaches_the_upstream_decoded_and_unchanged`, `responses_lite_keeps_its_body_and_marker`, `an_oauth_alias_and_the_buffered_adaptation_change_only_their_fields`, `unsupported_native_inputs_are_refused_before_the_send`, `issue2_parallelism_and_output_replay_pass_unchanged_for_every_client` (#2 broadened); added later for #25: `a_rate_limit_failure_keeps_the_message_codex_reads_its_delay_from`, `reported_zero_and_missing_usage_pass_unchanged`; the upstream event boundary (#25): `codex_native_request.rs` `undecodable_upstream_events_never_end_or_fail_http_responses`, `decodable_events_keep_their_outcomes_on_http_responses`, `codex_native_tests.rs` `undecodable_events_never_end_fail_or_convert_the_response`, `harmless_repeats_keep_the_decoded_outcome`, `codex_event_tests.rs` (the decision against the pinned `ResponsesStreamEvent` shape; private serde_json tokens, their nesting and the wrapped mapping against serde_json 1.0.149 with the first-party features; the last-wins classification reading); pinned terminals, private tokens and last-wins classification (#25): `codex_native_request.rs` `pinned_non_terminal_events_never_end_http_responses`, `http_failures_are_classified_with_the_last_repeated_key`; processing errors the SSE reader reads past (#25): `codex_native_request.rs` `read_on_events_leave_http_responses_to_the_real_completion` (first event, after `response.created`, after output, bootstrap buffering on and off, streaming and buffered, the real completion's usage), `read_on_events_end_http_responses_only_at_the_upstream_end` (the last such event at a clean end, the 408 without one, and the events both readers end on), `codex_event_tests.rs` `completed_reads_as_the_pinned_struct_does`, `outcome_and_accounted_follow_the_pinned_reader`; `codex_service_tier.rs` compares whole bodies (#8 broadened); matrix above; a read failure ends a buffered answer as a stream without a terminal event, and only a clean end ends it on a processing error (#25): `codex_native_request.rs` `buffered_answers_end_on_a_processing_error_only_at_a_clean_end` (cut-off, clean, later-completion, created-only and completion-first bodies, and the streaming control), `codex_native_tests.rs` `a_processing_error_ends_a_buffered_answer_only_at_a_clean_end` |
| Ordinary WebSocket turns, continuation, prewarm | #24 | `codex_native_ws.rs`: `native_creates_and_events_pass_through_unchanged`, `a_prewarm_reaches_upstream_and_its_continuation_stays_on_the_socket`, `a_lost_socket_never_replays_a_continuation`, `a_home_continuation_stays_on_its_pick_and_socket`, `a_binary_frame_keeps_its_bytes_until_the_boundary`; added later for #24: `a_failed_continuation_never_fails_over` (a retryable failure never moves a continuation to the second account), `keepalive_pings_only_the_client` (pings reach the client only; no inference keepalive upstream); added later for #25: `read_on_events_end_websocket_turns_as_the_websocket_reader_does` (a processing error ends the turn as the pinned WebSocket reader does, on an upstream socket and a turn over HTTP; a steering connection relays both); matrix above |
| Response steering | #24, #28 | `codex_duplex_tests.rs` `steering_lifecycle`, `steer_frames_ignore_the_payload_rules`, `append_is_refused_on_the_open_connection`; `ws_e2e.rs` `steering_forwards_steers_and_the_automatic_successor`, `steering_append_is_refused_on_the_open_connection`; `codex_native_ws.rs` `an_append_is_refused_on_a_codex_connection`; added later for #24: `codex_native_ws.rs` `steering_later_frames_reach_the_socket_as_sent` (every later frame through the duplex queue, with every rewrite control on), `codex_duplex_tests.rs` `outstanding_and_held_creates_stay_bounded`, `a_stalled_upstream_backpressures_client_frames`; the ceiling on steers in flight: `codex_duplex_tests.rs` `unacknowledged_steers_stop_at_the_ceiling`, `accepted_steers_awaiting_their_successor_count_toward_the_ceiling`, `released_steers_free_their_room`, `unsolicited_acceptances_stay_under_the_ceiling`, `pinned_settings_last_only_while_a_steer_names_their_parent`; `codex_native_ws.rs` `steering_stops_forwarding_steers_at_the_ceiling`; response bytes on a steering connection, with steers before and after the first output delta (#25): `codex_native_ws.rs` `steering_responses_pass_unchanged_before_and_after_the_first_delta`; compaction v2 on WebSocket turns (#25): `codex_native_ws.rs` `compaction_v2_answers_pass_unchanged_on_websocket_turns`; the upstream event boundary on every WebSocket transport (#25): `codex_native_ws.rs` `undecodable_events_never_end_or_fail_websocket_turns`, `pinned_readings_decide_websocket_turns` (private tokens, last-wins account classification, the wrapped mapping, pinned terminals); `logical_context_failures_are_preserved_without_an_extra_error` (last duplicate code and private raw-value error, every native WebSocket path; no extra relay error), `non_terminal_events_leave_the_record_to_the_real_completion`; events and wrapped errors written as JSON arrays (#25): `codex_native_ws.rs` `positional_events_classify_as_their_object_form` (account, other, context-window and completion events against their object form on the five native paths), `positional_wrapped_status_errors_classify_as_their_object_form`, `codex_native_request.rs` `positional_events_classify_as_their_object_form_on_http` (streaming and buffered); `codex_native_ws.rs` `positional_handshakes_keep_bootstrap_failover_before_delivery` (object/positional handshakes on HTTP, WebSocket-over-HTTP and upstream sockets, failover before delivery and no replay after a delta), `codex_event_tests.rs` `positional_frames_are_classified_by_their_members`, `positional_context_window_events_are_preserved_as_their_object_form`; terminal provider codes in private values (#25): `codex_native_ws.rs` `private_terminal_provider_codes_classify_as_resolved` (the error object and the code alone as raw values, five native paths); a first response that ends without `response.created` (#25): `codex_native_ws.rs` `first_response_ends_without_created_on_a_steering_connection`; response identity as the pinned `Value` reads it (#25): `codex_native_ws.rs` `decoded_response_ids_settle_as_the_pinned_value_reads_them` (the last of a repeated `id` in the completion and in `response.created`, the reversed order and a private raw value, against the same events with the read ID, steering and ordinary), `codex_event_tests.rs` `response_ids_read_as_the_pinned_value_reads_them`; completed identity by the `ResponseCompleted` reading, positional form included (#25): `codex_native_ws.rs` `positional_completions_settle_their_own_response`, `positional_completion_events_settle_their_own_response`, `codex_event_tests.rs` `completed_identity_follows_the_completed_reading`; failure slots a pinned struct refuses, read by position for account safety only (#25 criterion 7, #6): `codex_native_ws.rs` `refused_failure_slots_never_reach_the_client` (every `refused` frame of `codex_pinned_notice_readings.json` on the five native paths, first and after content: refused nested `Error` arrays keep the generic 502, close, cooldowns and usage of the safe refused array and are withheld; refused wrapped arrays stay skipped, the real completion succeeds and nothing cools; safe and opaque arrays as sent; the upstream received `Bearer at-FAKE`, and no frame carries it raw, escaped or in a private raw value), `later_steering_responses_keep_refused_slots_from_the_client`, `codex_ws_tests.rs` `refused_failure_slots_are_read_for_account_safety`; status notices the wrapped struct does not read (#25): `codex_native_ws.rs` `invalid_status_notices_keep_the_real_completion` (every read-on spelling of the `status` group with `x` and a logical rate limit, plain and private, on the three upstream-socket paths, first and after content; the wrapped `status: 429` and an escaped `status` key as failing controls), `codex_ws_tests.rs` `readings_follow_the_pinned_readers`, `notice_readings_follow_the_pinned_readers` (the pinned decision against `reading` for 467 + 492 frames, with only the documented status-less exception) |
| Cancellation and interruption (#1) | #24 | `codex_native_ws.rs` `interrupt_then_a_new_create_follows_the_first_party_sequence`; `websocket_interrupt_tests.rs` `a_late_interrupt_gets_upstreams_acknowledgement`, `an_http_turn_acknowledges_its_local_interrupt`; `codex_ws_interrupt_tests.rs` `a_zero_token_interrupted_response_is_a_normal_end`; added later for #24: `codex_native_ws.rs` `an_http_turn_answers_an_interrupt_without_inventing_output` (the local answer on a turn over HTTP carries no usage or text of its own, and the upstream request is cancelled) |
| HTTP fallback (a credential without `websockets`) | #28, #40 review | `codex_native_ws.rs` `http_fallback_is_a_separate_explicit_conversion`, `a_turn_over_http_asks_for_the_event_stream_whatever_the_frame_says` (request-sensitive upstream; `stream` omitted and `false`), `a_turn_over_http_refuses_an_answer_without_an_event_stream`, `a_codex_turn_over_http_forwards_what_the_fallback_refused`; matrix above |
| Handshake metadata (#3) | #24 | `codex_native_ws.rs` `handshake_metadata_reaches_the_opening_turn_and_event_metadata_every_turn`; `codex_ws_tests.rs` `handshake_metadata_event_carries_only_the_allowlist` |
| Sanitized transport diagnostics (#4) | #24 | `codex_ws_tests.rs` `a_dial_diagnostic_stays_out_of_the_public_error`, `capture_keeps_why_a_socket_ended`; `websocket_tests.rs` `interrupt_diagnostics_are_sanitized` |
| Context-window recovery (#5) | #25 | `codex_ws_tests.rs` `live_context_window_error_reaches_a_codex_client_as_a_failure`; `ws_e2e.rs` `context_window::*`; `scheduler_attempts.rs` `context_window_failure_reaches_the_client_as_its_own_code`; matrix `structured_failures_across_profiles` |
| Final subscription limits, account failures (#6) | #25 | `ws_limit_errors.rs` `all_accounts_exhausted_deliver_the_structured_limit`, `native_account_failures_after_start_never_reach_the_client_as_sent`, `limit_after_streamed_output` (over HTTP); undecodable frames under every reading of a repeated key (#25): `codex_native_ws.rs` `pinned_readings_decide_websocket_turns`, `ambiguous_account_frames_are_withheld_under_every_duplicate_reading` (three and more occurrences with the account reading only in the middle, on every WebSocket transport), `codex_ambiguous_tests.rs`; capacity messages composed from several members and private values (#25): `codex_native_ws.rs` `composed_capacity_frames_are_withheld_on_every_native_path`, `decoded_private_values_classify_as_the_pinned_value_reads_them` (each wrapped event takes the turn, record and cooldowns of the resolved event on the five native paths), `codex_ambiguous_tests.rs` `capacity_messages_composed_from_several_members_count`, `no_composed_reading_of_a_repeated_member_escapes_the_check`; a repeat is a repeated path, so a safe unique frame reaches the client (#25): `codex_native_ws.rs` `unique_frames_reach_the_client_on_every_native_path`, `codex_ambiguous_tests.rs` `only_a_repeated_path_is_a_repeated_member`; `codex_event_tests.rs` `pinned_view_resolves_private_values_as_the_first_party_value_does`; the failed `Error` struct by position, explicit statuses and withheld delivery (#25, #6): `codex_native_ws.rs` `nested_positional_failed_errors_classify_as_their_object_form`, `explicit_status_account_errors_are_withheld_on_every_native_path`, `a_later_steering_failure_that_reads_as_an_account_failure_is_withheld`, `explicit_zero_status_notices_keep_the_real_completion` (five native paths, first and after content), `codex_ws_tests.rs` `readings_follow_the_pinned_readers` and `failed_errors_read_as_the_pinned_error_struct` (against the pinned readers' outcomes in `tests/fixtures/codex_pinned_failure_readings.json`, built from the first-party code with serde_json 1.0.149 and its features), `nested_positional_errors_are_withheld_and_relayed_as_their_struct`, `explicit_status_account_errors_keep_their_status_and_are_withheld`, `explicit_zero_status_is_a_notice`; the server never renders a withheld failure (`websocket_tests.rs` `a_withheld_failure_is_never_rendered`) |
| Empty `response.incomplete` (#7) | #25 | Every native `response.incomplete` reaches the client as received, at every stage, with no local 502: `codex_native_tests.rs` `every_incomplete_passes_unchanged_at_every_stage`, `upstream_events_skip_the_request_boundary`, `a_stream_without_its_terminal_event_stays_truncated`; `codex_ws_interrupt_tests.rs` `a_zero_token_interrupted_response_is_a_normal_end`, `an_unexplained_zero_output_incomplete_passes_unchanged_at_every_stage`; `websocket_interrupt_tests.rs` `zero_output_ends::an_interrupted_end_keeps_the_socket_the_credential_and_its_usage`, `zero_output_ends::zero_output_ends_pass_unchanged_at_every_stage`, `zero_output_ends::a_turn_over_http_keeps_every_zero_output_end`; `codex_native_request.rs` `every_incomplete_reaches_http_clients_as_sent`; `codex_native_ws.rs` `every_incomplete_reaches_websocket_clients_as_sent` (all on the `probe-sse-incomplete-ends` probe); translated route only (#27): `codex_response.rs` `named_reasons_are_not_silent_aborts` |
| Credential isolation and identity | #26 (#37), #25 criterion 7 | `codex_identity_tests.rs`; `codex_native_request.rs` (`authorization` per profile); matrix above (client key and cookie under any header name); the outbound credential guard (#25, [CONTRACT.md](CONTRACT.md#credential-isolation)): `codex_secret_tests.rs` (values from what the request carried and nothing else, escaped and raw-value spellings, repeated members, arrays, invalid JSON, a failure's classification kept, buffered answers), `codex_native_ws.rs` `the_selected_credential_never_reaches_a_websocket_client` (accepted `Error` arrays of pinned account kinds and of non-account codes, a generic retryable failure, an earlier repeated error, an array `response`, escaped and raw-value spellings, skipped and notice frames, invalid JSON; five native paths, first and after content; each against `at-OTHER`, which keeps its bytes, record, status, cooldowns and attempts), `pinned_account_kinds_are_withheld_without_a_token`, `later_steering_responses_never_carry_the_selected_credential` (a later failure, notice and completion), `each_socket_guards_the_values_its_handshake_carried` (a reused socket after an in-memory key change, then a fresh dial), `codex_native_request.rs` `the_selected_credential_never_reaches_a_streaming_http_client`, `the_selected_credential_never_reaches_an_http_answer` (upstream 400, buffered, compaction, a usage limit and the cooldown's last error) |
| Config, plugin and client-header bypass | #28 | `codex_settings.rs` (every control on, three client header sets, every route, OpenAI-compatible control); `codex_native_plugins.rs` (real Go plugin hooks on HTTP, WebSocket, SIWC and Home; `responses_callbacks_in_either_spelling_take_the_native_route` for all four callback protocol pairs); `validate.rs` `codex_is_compat_is_refused`; `watching.rs` `a_reload_with_a_codex_is_compat_model_keeps_the_running_config`; `management.rs` `management_writes_refuse_a_codex_is_compat_model` |
| Sign in with ChatGPT | #28 | `codex_native_request.rs` `native_siwc_forwards_the_client_body`; `codex_native_plugins.rs` `a_siwc_only_pool_is_native`; matrix above |

### Translations, audited separately

The translated route converts Claude Messages and Chat Completions requests into Responses (the Gemini and Interactions sources were removed with issue #49; the evidence below that names them is historical), and their answers back ([#27](https://github.com/zakstak/cliproxy-rs/issues/27)). It is not byte transparency, so the matrix above does not cover it. It keeps meaning or refuses: [CONTRACT.md](CONTRACT.md#translated-route) lists each retained conversion with its evidence, and each refusal. [`crates/cpa-server/tests/codex_translated.rs`](../../crates/cpa-server/tests/codex_translated.rs) tests it through the router:

- round trips for each source (`claude_round_trips_keep_meaning`, `chat_round_trips_keep_meaning`);
- refusals before the send (`unexpressible_content_is_refused_before_the_send`, `generation_controls_map_or_are_refused`, `failed_tool_results_are_refused`);
- schema and tool-choice fidelity (`tool_schemas_keep_their_constraints`);
- bypass checks (`operator_rules_cannot_rewrite_translated_bodies`, and `codex_native_plugins.rs` `plugin_translators_cannot_rewrite_codex_translated_traffic`).

The meta and xAI executors, which share the registered translators, keep Go's conversion.

## 3. Performance is not a qualification gate

Issue [#45](https://github.com/zakstak/cliproxy-rs/issues/45) removed performance from qualification. An earlier version of this section recorded heap-per-request observations from a counting allocator and pointed to a `bench/budgets.txt` gate. Both the allocation gate and the budgets file are deleted. The raw output stays as a dated historical record in [`bench/results/2026-11-07-codex-alloc.txt`](../../bench/results/2026-11-07-codex-alloc.txt), taken at `894e24c7e2443263ed2bf1a977b52637f3b9e4df`. It is not a current claim or a limit.

## Local verification receipt

**Full workspace, at `894e24c7e2443263ed2bf1a977b52637f3b9e4df`, clean tree.** This follows `.github/workflows/ci.yml`:
- rootless podman as uid 1000, `--network none` (loopback only), the shared heavy-build lock, `CARGO_BUILD_JOBS=2`, `nice -n 19`;
- `CPA_TEST_NO_SKIP=1`;
- PostgreSQL 15 server binaries (`CPA_TEST_PG_BIN`, from a local image that adds only Debian's `postgresql` package);
- the host's Go 1.27.3 with the plugin modules;
- rustc 1.99.1;
- private auth dirs.

`cargo-nextest` is not installed there, so `cargo test` ran the same targets.

| Step | Result |
| --- | --- |
| `cargo test --workspace --locked --offline --no-fail-fast` | 91 test binaries: 2,268 passed, 1 failed, 2 ignored |
| `cargo test --manifest-path vendor/gjson/Cargo.toml` | passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | passed |
| `cargo check --locked --manifest-path harness/rust/Cargo.toml` | passed |
| `cargo fmt --all --check` | passed |

The one failure is `home::tests::home_plugin_finalization_waits_for_the_applied_config` (`crates/cliproxy/src/home.rs`). It fails the same way in this environment at the fork's base before the migration stack (`origin/fork`, `c610d7d8fe99291325573630c4ec99ad8ac86d2e`) and at the stack heads checked (`b555216b`, `7d9d6811`, `1c0a8bce`). It is a pre-existing failure here, and this PR did not change it.

PR #35, a test-only PR that is not merged, diagnoses it. The test counts every RPUSH the fake Home receives. Once another test in the same binary has installed the process-wide log subscriber, this test's own plugin reload adds two `app-log` RPUSHes, which the count reads as early finalization. #35 counts every RPUSH except `app-log`. With #35 merged onto `7fe3f8e3` in a throwaway worktree (not committed), the `cliproxy` unit tests passed 162/162 (uid 1000, `--network none`, `CPA_TEST_NO_SKIP=1`).

These are local runs, not CI. GitHub Actions is disabled on this fork, so no hosted check has run.

## 4. Delivery state and landing order (#20 reconciliation)

Checked on 2026-11-07, at #42 (`5eda74e7`). This section is a snapshot of that head. The optional combined landing candidate, which merges these PRs locally, is recorded in [LANDING-CANDIDATE.md](LANDING-CANDIDATE.md); nothing in it is merged on `fork`.

### Status of the pieces

- **Merged on `fork`:** #9, #10, #11, #13, #14, #16, #17 and #18, including #14 (#1, #7), #16 (#3), #17 (#6), #10 (#5), #13 (#4), and #11 and #18 (#2, #8).
- **Not merged:** every migration PR below, and #30 and #35.
- **Open issues:** every issue in the tracker except #1, #2, #4, #5, #6 and #8.
- **Hosted CI:** has never run on any of them, because GitHub Actions is disabled on this fork. Every check named on this page is a local run.
- **Live acceptance:** unproven (section 5).

A completed review is evidence for one head. It does not merge or close anything.

### Open PRs and their reviewed heads

Each head is the PR head. Each has a completed independent Codex review with no findings at that head, and the parent's source review.

| PR | Issue | Head | Base |
| --- | --- | --- | --- |
| #30 | #19 (fork: Linux only) | `a61a7e8e7f22dcda17185a9e241f7835b15168d6` | `fork` |
| #35 | test isolation, #29 and #20 qualification | `98a7898b8db5370c3b50e7a3c9f7d802871a2651` | `fork` |
| #31 | #21 | `389556556ec55836b79cc6566915790c6d59d087` | `fork` |
| #34 | #22 (part) | `aaa077284c0a0a521d0f0291bf3ee96c03ff6912` | #31 |
| #36 | #23, #25 HTTP half | `7656e7038a9948973190ca02f70dbe45b7b851ca` | #34 |
| #38 | #24, #25 WebSocket half, #3 | `5b2e4382f4c618a290806ea15496763cf874c699` | #36 |
| #39 | #27 | `b555216b40bfeb2c10e7acf6ca67f9a7409ed6fa` | #38 |
| #37 | #26 | `e21db759050c27b9b8f07e83f31c2d248ca5b3a9` | `fork` |
| #40 | #28, and #37's commits through the merge `7d9d6811` | `1c0a8bced211f71fad97bd21f3ee9b88fee780a3` | #39 |
| #41 | #29 | `7fe3f8e3552f4d78ded93c9022a42db84c777d3b` | #40 |

### Landing order

1. **#30.** It is not in the migration stack's history.
   - Trial merges without committing found conflicts with #31 (`crates/cpa-exec/Cargo.toml`, `docs/DIFFERENCES-FROM-GO.md`), with #37 (`crates/cpa-core/src/config/validate.rs`, `crates/cpa-server/src/runtime.rs`) and with #41's head (those four files plus `README.md`, `docs/PARITY.md` and `docs/parity-audit/status.md`).
   - Landing #30 first therefore needs those conflicts resolved in the layers that meet them, as reviewed changes. The resolutions are not part of any PR yet.
2. **#35.** It is independent and merges cleanly onto #41's head.
3. **#31, #34, #36, #38, #39,** in that order. Each is retargeted to `fork` after the one below it lands.
4. **#37, then #40, then #41.**
   - #40 already contains #37's original commits (`030a8ac0`, `f1f8c829`, `a728745c`, `e21db759`) through its merge `7d9d6811`. So #37 must land with those commits intact: by a merge commit, or only through #40.
   - A squash or rebase of #37 would create different commits for the same changes, and #40 would then conflict with them or repeat them.

### Remaining gaps

These criteria are not delivered by the migration or by merged code:

- **#3, handshake-only metadata the client reads only from its own handshake.** This is the presence-based `x-reasoning-included`, and a turn on a reused socket gets no `response.metadata` event.
  - The pinned client reads `x-reasoning-included` only from the upgrade response of a socket it opened (`responses_websocket.rs:617`).
  - The proxy's downstream 101 is sent before the first frame chooses a credential and model, so it cannot carry the upstream handshake ([CONTRACT.md](CONTRACT.md#handshake-metadata)).
  - Delivering it needs a design decision. The source offers no mechanism, so it is not an implementation gap a test can close. The other #3 criteria have tests: `response.metadata` for the opening turn and for a send retried on a fresh socket; handshake-only, event-only and both cases; a model the proxy rewrote; the allowlist. Those tests are on `fork` through #16, or in #38.
  - Later correction (2026-11-08, #24 follow-up): the reused-socket limit was wrong for `openai-model`. The pinned client reads it from a model-only `response.metadata` event, so the proxy now delivers it before each later request on the socket ([CONTRACT.md](CONTRACT.md#handshake-metadata)). The `x-reasoning-included` limit stands. The results recorded above are unchanged, and the test `handshake_metadata_reaches_the_opening_turn_and_event_metadata_every_turn` named in the ownership matrix of section 2 is now `handshake_metadata_reaches_each_turn_and_event_metadata_passes_unchanged`.
- **Merging, and hosted CI on the merged result.** Neither has happened.
- **The live gates in section 5.**

#7's criteria all have tests on `fork` from the merged #14: `named_reasons_are_not_silent_aborts` (interrupted, `content_filter` and `max_output_tokens` are not aborts), `a_zero_token_interrupted_response_is_a_normal_end` (ID, usage, acknowledgement, and the socket serves the next turn through `follow_up_is_clean`), and the unexplained empty `response.incomplete` that stays a 502. The native route keeps that exception (`empty_incomplete_keeps_its_narrow_exception`, in #36). Closing #7 is the owner's call. Later correction (2026-11-08, #25, PR #63): the native routes no longer answer any `response.incomplete` with a 502, because the pinned client classifies the event itself as a terminal, non-retryable `ResponseIncomplete` (`sse/responses.rs:451-506`, `protocol/src/error.rs:428-442`). `empty_incomplete_keeps_its_narrow_exception` is replaced by the tests in the ownership-matrix row for #7. The translated route keeps the 502 until #27. The results recorded above are unchanged.

The other implementation issues (#21 to #29) are mapped criterion by criterion in the ownership matrix above. Each is delivered by its open PR, not by `fork`.

## 5. Unproven live gates

Local mocks cannot show these, and nothing here claims them:

- That the ChatGPT backend accepts each forwarded field and option (`prompt_cache_options`, `flex`, unknown fields), the `stream: true` adaptation of a buffered client, or `response.steer`.
- That the Sign in with ChatGPT and compatible endpoints accept the client's request unchanged. No first-party source covers them, and their own error reaches the client.
- That `chatgpt.com` accepts cliproxy-rs's TLS client and identity ([IDENTITY.md](IDENTITY.md)).
- How accounts are enforced, rate-limited or metered upstream.
- Behaviour under real network failure, latency and concurrency at scale.
- Hosted CI: GitHub Actions is disabled on this fork, so no hosted check has run. The local runs the PR lists are not CI.

### Selected-credential guard follow-up regressions

`codex_native_ws` covers a rejected upgrade whose subscription-limit header carries
the selected credential, a bootstrap error on a reused socket after its configured
key changes, and a middle repeated `body.error` that names the pooled account.
The header has a never-sent-value control; the ambiguity case carries no selected
credential, so its protection depends on the account reading. These are local mock
regressions, not provider or full-client acceptance.

### Served outcome and persisted evidence

`dispatch::tests::an_omitted_frame_commits_the_stream` pins the bootstrap: a served
frame the guard left out commits the stream on the same poll, its end is a clean end and
its error the stream's own, and only a stream that served nothing is `empty_stream`.
`codex_native_ws` `withheld_frames_keep_the_served_outcome_without_a_replay` runs a sole
terminal (plain, escaped, without usage), a withheld prefix and terminal, a safe prefix
with a withheld terminal, and a withheld prefix followed by a failure or a truncation on
all five native WebSocket paths, with a second account available, each against its
`at-OTHER` control: the same executions, records and cooldowns, and no replay of a
served terminal. `codex_native_request`
`a_withheld_stream_keeps_the_served_outcome_without_a_replay` does the same for a
streaming HTTP client, with and without `stream-bootstrap-buffering`. A committed stream
without a first payload is forwarded until its own end (`respond::stream`), as the
WebSocket forwarder already did.

`cpa_core::sent` tests the scrub; `request_logging::tests::sent_values_never_reach_a_section_or_its_spool`
reads the spool file while the request runs and the log written after it.
`codex_native_ws` `logs_and_usage_records_never_keep_the_selected_credential` (five
paths; a failure, an escaped one, one after output, a skipped notice, and a spooled
failure), `logs_follow_the_values_each_socket_carried` (a reused socket after a key
change, then a fresh dial) and `a_lost_socket_logs_no_credential`, and
`codex_native_request` `http_logs_and_usage_records_never_keep_the_selected_credential`
(an error body and its headers, a streamed event, a withheld buffered answer) read the
actual log file and the queued usage record against `at-OTHER` controls. These are
local mock regressions with fake credentials; no collector, real client or provider
traffic is claimed.
