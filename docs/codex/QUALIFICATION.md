# Codex migration: qualification

This page is the receipt for the Codex migration ([#20](https://github.com/zakstak/cliproxy-rs/issues/20)), written for its final qualification step ([#29](https://github.com/zakstak/cliproxy-rs/issues/29)). It keeps four kinds of evidence apart:

1. Source evidence: what the first-party client and the provider documentation define ([CONTRACT.md](CONTRACT.md)).
2. Local mock results: what the server does at its real ingress and egress, against loopback upstreams, with fake credentials and private `auth-dir`s.
3. Measured performance: heap per request for sampled workloads ([BENCHMARKS.md](../BENCHMARKS.md#codex-request-preparation-heap-per-request)).
4. Unproven live gates: what no local test can show.

No result here comes from a real account, and none was sent to a provider.

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

The one failure is `home::tests::home_plugin_finalization_waits_for_the_applied_config` (`crates/cliproxy/src/home.rs`). It fails the same way in this environment at the fork's base before the migration stack (`origin/fork`, `c610d7d8fe99291325573630c4ec99ad8ac86d2e`) and at the stack heads checked (`b555216b`, `7d9d6811`, `1c0a8bce`). It is a pre-existing failure here; its cause has not been established.

These are local runs, not CI. GitHub Actions is disabled on this fork, so no hosted check has run.

## 4. Unproven live gates

Local mocks cannot show these, and nothing here claims them:

- That the ChatGPT backend accepts each forwarded field and option (`prompt_cache_options`, `flex`, unknown fields), the `stream: true` adaptation of a buffered client, or `response.steer`.
- That the Sign in with ChatGPT and compatible endpoints accept the client's request unchanged. No first-party source covers them, and their own error reaches the client.
- That `chatgpt.com` accepts cliproxy-rs's TLS client and identity ([IDENTITY.md](IDENTITY.md)).
- How accounts are enforced, rate-limited or metered upstream.
- Behaviour under real network failure, latency and concurrency at scale.
- Hosted CI: GitHub Actions is disabled on this fork, so no hosted check has run. The local runs the PR lists are not CI.
