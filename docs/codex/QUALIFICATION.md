# Codex migration: qualification

This page is the receipt for the Codex migration ([#20](https://github.com/zakstak/cliproxy-rs/issues/20)), written for its final qualification step ([#29](https://github.com/zakstak/cliproxy-rs/issues/29)). It keeps four kinds of evidence apart:

1. Source evidence: what the first-party client and the provider documentation define ([CONTRACT.md](CONTRACT.md)).
2. Local mock results: what the server does at its real ingress and egress, against loopback upstreams, with fake credentials and private `auth-dir`s.
3. Measured performance: heap per request for sampled workloads ([BENCHMARKS.md](../BENCHMARKS.md#codex-request-preparation-heap-per-request)).
4. Unproven live gates: what no local test can show.

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
| Native HTTP streaming, buffered, compaction | #23, #25 | `codex_native_request.rs`: `native_requests_and_events_pass_through_the_route_unchanged`, `buffered_clients_get_the_destination_contract`, `compaction_passes_through_the_route_unchanged`, `private_and_unknown_events_reach_codex_clients`, `codex_request_boundary_refuses_before_the_upstream_send`; matrix above |
| Ordinary WebSocket turns, continuation, prewarm | #24 | `codex_native_ws.rs`: `native_creates_and_events_pass_through_unchanged`, `a_prewarm_reaches_upstream_and_its_continuation_stays_on_the_socket`, `a_lost_socket_never_replays_a_continuation`, `a_home_continuation_stays_on_its_pick_and_socket`, `a_binary_frame_keeps_its_bytes_until_the_boundary`; matrix above |
| Response steering | #24, #28 | `codex_duplex_tests.rs` `steering_lifecycle`, `steer_frames_ignore_the_payload_rules`, `append_is_refused_on_the_open_connection`; `ws_e2e.rs` `steering_forwards_steers_and_the_automatic_successor`, `steering_append_is_refused_on_the_open_connection`; `codex_native_ws.rs` `an_append_is_refused_on_a_codex_connection` |
| Cancellation and interruption (#1) | #24 | `codex_native_ws.rs` `interrupt_then_a_new_create_follows_the_first_party_sequence`; `websocket_interrupt_tests.rs` `a_late_interrupt_gets_upstreams_acknowledgement`, `an_http_turn_acknowledges_its_local_interrupt`; `codex_ws_interrupt_tests.rs` `a_zero_token_interrupted_response_is_a_normal_end` |
| HTTP fallback (a credential without `websockets`) | #28, #40 review | `codex_native_ws.rs` `http_fallback_is_a_separate_explicit_conversion`, `a_turn_over_http_asks_for_the_event_stream_whatever_the_frame_says` (request-sensitive upstream; `stream` omitted and `false`), `a_turn_over_http_refuses_an_answer_without_an_event_stream`, `a_codex_turn_over_http_forwards_what_the_fallback_refused`; matrix above |
| Handshake metadata (#3) | #24 | `codex_native_ws.rs` `handshake_metadata_reaches_the_opening_turn_and_event_metadata_every_turn`; `codex_ws_tests.rs` `handshake_metadata_event_carries_only_the_allowlist` |
| Sanitized transport diagnostics (#4) | #24 | `codex_ws_tests.rs` `a_dial_diagnostic_stays_out_of_the_public_error`, `capture_keeps_why_a_socket_ended`; `websocket_tests.rs` `interrupt_diagnostics_are_sanitized` |
| Context-window recovery (#5) | #25 | `codex_ws_tests.rs` `live_context_window_error_reaches_a_codex_client_as_a_failure`; `ws_e2e.rs` `context_window::*`; `scheduler_attempts.rs` `context_window_failure_reaches_the_client_as_its_own_code`; matrix `structured_failures_across_profiles` |
| Final subscription limits, account failures (#6) | #25 | `ws_limit_errors.rs` `all_accounts_exhausted_deliver_the_structured_limit`, `native_account_failures_after_start_never_reach_the_client_as_sent`, `limit_after_streamed_output` (over HTTP) |
| Empty `response.incomplete` (#7) | #25 | `codex_native_tests.rs` `empty_incomplete_keeps_its_narrow_exception` |
| Credential isolation and identity | #26 (#37) | `codex_identity_tests.rs`; `codex_native_request.rs` (`authorization` per profile); matrix above (client key and cookie under any header name) |
| Config, plugin and client-header bypass | #28 | `codex_settings.rs` (every control on, three client header sets, every route, OpenAI-compatible control); `codex_native_plugins.rs` (real Go plugin hooks on HTTP, WebSocket, SIWC and Home; `responses_callbacks_in_either_spelling_take_the_native_route` for all four callback protocol pairs); `validate.rs` `codex_is_compat_is_refused`; `watching.rs` `a_reload_with_a_codex_is_compat_model_keeps_the_running_config`; `management.rs` `management_writes_refuse_a_codex_is_compat_model` |
| Sign in with ChatGPT | #28 | `codex_native_request.rs` `native_siwc_forwards_the_client_body`; `codex_native_plugins.rs` `a_siwc_only_pool_is_native`; matrix above |

### Translations, audited separately

The translated route converts Claude Messages, Chat Completions, Gemini and Gemini Interactions requests into Responses, and their answers back ([#27](https://github.com/zakstak/cliproxy-rs/issues/27)). It is not byte transparency, so the matrix above does not cover it. It keeps meaning or refuses: [CONTRACT.md](CONTRACT.md#translated-route) lists each retained conversion with its evidence, and each refusal. [`crates/cpa-server/tests/codex_translated.rs`](../../crates/cpa-server/tests/codex_translated.rs) tests it through the router:

- round trips for each source (`claude_round_trips_keep_meaning`, `chat_round_trips_keep_meaning`, `gemini_round_trip_keeps_meaning`, `interactions_conversion_adds_nothing`);
- refusals before the send (`unexpressible_content_is_refused_before_the_send`, `generation_controls_map_or_are_refused`, `failed_tool_results_are_refused`);
- schema and tool-choice fidelity (`tool_schemas_keep_their_constraints`, `gemini_schema_literals_are_not_rewritten`, `gemini_tool_restrictions_are_not_widened`, `wide_gemini_tool_restrictions_keep_their_meaning`);
- bypass checks (`operator_rules_cannot_rewrite_translated_bodies`, and `codex_native_plugins.rs` `plugin_translators_cannot_rewrite_codex_translated_traffic`).

The meta and xAI executors, which share the registered translators, keep Go's conversion.

## 3. Measured performance

[BENCHMARKS.md](../BENCHMARKS.md#codex-request-preparation-heap-per-request) has the setup, commands, compilers and tables. The raw output is in [`bench/results/2026-11-07-codex-alloc.txt`](../../bench/results/2026-11-07-codex-alloc.txt), taken at `894e24c7e2443263ed2bf1a977b52637f3b9e4df`.

These are heap observations from the counting allocator in the test profile, for sampled synthetic workloads. There were six runs, three each on rustc 1.99.1 and on CI's pinned 1.99.0, on one local machine:

- A native request that needs no routing peaked at 84 KB of preparation heap, with 939 allocations, at every sampled size from 122 KB to 7.7 MB. That is down from 2.0x the body, after removing an always-on body copy in the Responses handler.
- An alias added one copy of the body.
- The translated route peaked at between 3.8x and 4.5x the body for the sampled Claude and Gemini requests. In the Gemini requests the 48 tool schemas are the same at every size; only the conversation turns grow.

The allocator measures peak live heap and allocation calls, not time or bytes read. So these results establish nothing about computational scaling, and nothing about growth with schema size.

`bench/budgets.txt` gates the values. They were not measured on GitHub's runners.

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
- **Merging, and hosted CI on the merged result.** Neither has happened.
- **The live gates in section 5.**

#7's criteria all have tests on `fork` from the merged #14: `named_reasons_are_not_silent_aborts` (interrupted, `content_filter` and `max_output_tokens` are not aborts), `a_zero_token_interrupted_response_is_a_normal_end` (ID, usage, acknowledgement, and the socket serves the next turn through `follow_up_is_clean`), and the unexplained empty `response.incomplete` that stays a 502. The native route keeps that exception (`empty_incomplete_keeps_its_narrow_exception`, in #36). Closing #7 is the owner's call.

The other implementation issues (#21 to #29) are mapped criterion by criterion in the ownership matrix above. Each is delivered by its open PR, not by `fork`.

## 5. Unproven live gates

Local mocks cannot show these, and nothing here claims them:

- That the ChatGPT backend accepts each forwarded field and option (`prompt_cache_options`, `flex`, unknown fields), the `stream: true` adaptation of a buffered client, or `response.steer`.
- That the Sign in with ChatGPT and compatible endpoints accept the client's request unchanged. No first-party source covers them, and their own error reaches the client.
- That `chatgpt.com` accepts cliproxy-rs's TLS client and identity ([IDENTITY.md](IDENTITY.md)).
- How accounts are enforced, rate-limited or metered upstream.
- Behaviour under real network failure, latency and concurrency at scale.
- Hosted CI: GitHub Actions is disabled on this fork, so no hosted check has run. The local runs the PR lists are not CI.
