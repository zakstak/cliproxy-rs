# cpa-translate migration

All original inventory tests in this crate have a disposition below. No active test reads recorded Go output, variants, dynamic-value normalization or Go error strings. `tests/golden.rs`, the patch replay and all output fixture files are removed. Historical inputs remain labelled as inputs; they do not establish endpoint acceptance or expected output. The only runtime behavior change is the independently justified tool-choice correction described below.

## Original named tests

| Original test | Disposition and independent replacement |
| --- | --- |
| `colliding_sanitized_names_get_go_hash_suffixes` | Replaced by `sanitized_names_are_valid_unique_and_reversible`: valid bounded unique names, order independence and reversible mapping. No required Go hash bytes. End-to-end long namespace collisions also have authored request/choice/history/streamed/buffered identity expectations. |
| `unwrap_input_follows_encoding_json` | Replaced by semantic patch envelope/string preservation and ambiguity refusal tests. Go invalid UTF-8/lone-surrogate repair and HTML escaping expectations retired. |
| `over_length_call_ids_match_go` | Replaced by two properties for bounds, collision disambiguation, legal-ID preservation, Unicode, missing/duplicate IDs and multiturm call/result references. Strict Codex call-ID pairing also has an authored reasoning/history test. |
| `url_citations_match_go` | Replaced by three independent buffered/streamed citation tests for URL/title/index fields, character offsets, deduplication and rejected negative/reversed ranges. |
| `translated_clients_keep_gos_service_tier_rule` | Direct input/output service-tier table retained under `translated_service_tiers_follow_the_route_policy`, grounded in translated versus native route policy; no output fixture. |
| `is_http_url_follows_go_url_parse` | Replaced with normal HTTP authority/path media boundaries. Go acceptance of odd authorities is no longer a test obligation. |
| `trim_space_follows_go_unicode_rules` | Replaced by boundary whitespace examples plus preservation of non-whitespace invalid/opaque bytes. |
| `go_helper_calls_match` | Tagged Go-call interpreter removed. Seven helper contracts cover patch wrappers/schema, explicit/stable user identity, cache-control attachment boundaries, schema normalization/constraints, structured-output labels/schema, call/result adjacency and ambiguity, and model/reminder/regex/attribution behavior. |
| `responses_framer_matches_recorded_go_frames` | Replaced by independently specified payload/event order/terminal boundary assertions. |
| `responses_framer_matches_go_joining` | Replaced by partial-event buffering, final flush and grouped input invariance against an authored SSE expectation. |
| `apply_patch_responses_match_go` | Removed after 20 authored patch contracts passed. All 139 original operation sequences retained input-only in `patch_inputs.json`; supplemental replay checks typed custom input fields and emitted failure finality. There is no recorded output count/chunk/error oracle. |
| `reference_goldens` | Removed after the request/response/state/corpus contracts below passed. All 4,715 pair inputs retained: 4,457 in `translation_inputs/` and 258 same-format Chat cases in `chat_passthrough_inputs.json`. Go output variants, time/ID normalization, pending/selective failures and schema-layout patching are gone. |
| `tool_search_goldens` | Removed after 12 authored contracts cover its 22 unique families: schema/query, hosted exclusion, on/off isolation, discovery/deduplication/references, direct precedence, omission/empty result, stable prefix/cache hints, namespace history, complete/truncated response, client anchor ownership, same-named ordinary function, sanitized-name identity. |
| `registered_pairs_match_go` | Replaced by an explicit supported-pair/token-adapter matrix in `sdk_translator.rs`. |
| `fallbacks_match_go` | Replaced by model-only rewrites, unknown-field byte preservation, no-op bytes and explicit token-count shape. |
| `registration_matches_go` | Retained its direct behavior assertions under `registry_adapters_and_unregistered_model_rewrites_are_explicit` in `translation_contracts.rs`; no Go fixture consumer. |
| `golden_rfc3339_parser_matches_unix_seconds` | Removed with the obsolete dynamic-value normalizer it served. |

`responses_controls.rs` also lost all incidental JSON member-position/prefix/suffix assertions; semantic generation-limit and parallel-control assertions remain.

## Broad replay replacement

The old pair cases comprised 845 Claude/Codex, 732 Claude/Chat, 561 Chat/Claude, 536 Chat/Codex, 971 Responses/Claude, 217 Responses/Codex, 595 Responses/Chat, and 258 Chat/Chat scenarios. No fixture count is a test gate.

- `request_pair_contract.rs`: 12 authored tests across the seven translated/native Responses pairs. They assert complete conversation text and tool call/result fields, reverse-order parallel result identity, schema property/required/additionalProperties constraints, URL/base64 images, PDFs/file IDs/file data/audio/video metadata, every emptied-turn refusal versus retained text neighbors, tool-choice intent and allowed subsets, structured output constraints, namespace choice/history identity, custom string tool input/result replay and return, three colliding long namespace/local identities in both response modes, opaque Codex encrypted-reasoning order and strict foreign-thinking refusal, and top-level versus additional ordinary-tool precedence.
- `response_pair_contract.rs`: authored buffered and streaming transcripts for all seven pairs. They assert text, reasoning, exact call identity/name/parsed arguments, split argument assembly, usage, model identity and finish state.
- `stream_state_contract.rs`: five authored matrices for late tool name/ID, independent choice slots, truncated EOF/DONE, length/filter incomplete terminals, late terminal hydration for Claude, and upstream error envelopes without synthesized success.
- `translation_corpus_contract.rs`: all original valid object requests must succeed, except an explicit reviewed attachment-refusal list and input-type-invalid generation controls, which must refuse. For each accepted context the test adds a fresh user turn and a distinct schema declaration, then requires preservation of the text, schema constraints and routed model. It never lets implementation failure choose which accepted cases remain covered. Every historical response/stream/token-count input is replayed; recognized native JSON response text and completed incremental text streams have independent source-to-target text conservation assertions.
- Existing independent suites remain active: `parallel_tool_calls`, `claude_stop_reasons`, `claude_chat_usage`, `claude_responses_usage`, `responses_claude_reasoning`, `claude_property_keys`, `claude_system_turns`, `developer_roles`, `claude_fallback`, `registry_overlay`, `responses_controls`, `web_search_history`, and depth/resource-limit tests.
- `translation_contracts.rs` retains the old direct fragmentation, request-local tool state, opaque passthrough, registry and deeply nested input assertions.
- `patch_contract.rs`: 20 authored tests cover exact patch string content/Unicode splits, declaration/history/choice normalization, invalid-envelope refusal, passthrough bytes, buffered restoration/usage/unknown fields, single safe failure, final snapshot requirement, transport/source terminal, late identity, conflicting identity and snapshots, all three matching keys, folded dispatcher provenance and ordinary-child exclusion, interleaved calls, sparse terminal snapshots, winning declarations and sealed response/transport lifecycle.

## Retired obligations and limits

Malformed JSON repair, invalid Unicode repair, numeric/library coercion, exact JSON key ordering/HTML escaping, Go hash suffixes, recorder population, private Go error prose, exact generated IDs/timestamps and Go chunk layouts are not expected-output obligations. Input-only robustness coverage remains where useful. Historical bare/malformed event framing and Chat delta chunks paired with a non-streaming client request are not reinterpreted as valid provider messages; typed framing, truncation and native-byte contracts cover the supported paths.

The new suites specify useful contract families rather than claiming equality with all old outputs. They do not prove provider endpoint acceptance of synthetic test payloads. Codex reasoning samples are explicitly synthetic opaque transport carriers, grounded in `docs/codex/CONTRACT.md`; tests use no live provider credentials or accounts.

## Runtime correction

`src/claude_responses.rs` adds one `tool_choice: "none"` arm that produces Anthropic Messages `{"type":"none"}`. Previously the converter kept tools but discarded the choice, allowing provider auto-selection. The mapping follows Anthropic's Messages API and the existing Chat-to-Claude mapping. `docs/DIFFERENCES-FROM-GO.md` now has an operator-facing entry. No other runtime logic changed.

## Verification

Network-denied `cargo test --offline -p cliproxy-internal` passed after retirement (49 library tests and every integration suite). The final added ordinary additional-tool precedence test passed separately, bringing request-pair tests to 12. `cargo fmt -p cliproxy-internal` passes. Final network-denied `cargo clippy --offline -p cliproxy-internal --all-targets -- -D warnings` passed. Parent owns the final workspace-wide fmt/clippy/suite gates.
