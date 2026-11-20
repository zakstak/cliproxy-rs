# Server LCP migration

Owned files: `crates/cliproxy-internal/src/gateway/lcp_tests.rs` and new `lcp_model_tests.rs`. Production `lcp.rs` is unchanged. No provider-wire claim is made: conversation matching, leases and deterministic identity are local proxy policy.

| Original inventory entry | Disposition and independently specified replacement |
| --- | --- |
| `canonical_turns_and_fingerprints_match_go` | Removed fixture oracle. Independent tables compare supported Claude Messages, Chat Completions and Responses conversations; role aliases, reasoning omission, tools/media/opaque JSON, normalization and volatile system data, explicit versioned SHA256 field framing, user versus system dates, sampled/unsampled content identity, actual tails, all system-context digest fields, turns/parts/nesting ceilings and parallel tool permutations. No removed-provider extraction obligation remains. |
| `matcher_replays_go_call_by_call` | Removed recorded-call oracle after replacements and old oracle passed together. New linear-scan/vector-LRU reference model runs 18,000 generated public-API operations across namespace isolation, capped prefixes, growth/truncation/forks, longest trajectory and recency, TTL/equality/backward clocks, binding/failover/delayed-success protection, find/peek/touch/remove/invalidate/retain/lookup, group/prefix eviction caps, clear/reset and generation guards. Independent deterministic session framing preserves operator-visible identity. Separate compaction scenarios cover contiguous overlap and summary boundaries, preserved prefixes, single-turn/noncontiguous rejection, system environment isolation including middle system turns and touch changes, repeated compaction/ancestor leaf choice, unrelated ambiguity, bucket cap/removal/expiration recovery, real tail of long histories and >1024-turn candidate, divergent heads outside the probe. |
| `limits_default_and_cover_max_turns` | Kept independent cap/default assertion. |
| `tool_parts_sort_by_value_then_digest` | Kept independent normalization invariant; expanded with all six three-tool permutations and stable text locations. |
| `lookup_reports_every_credential_of_a_session` | Kept independent public-API sorted multi-auth lookup behavior. |
| `lookup_drops_expired_groups_without_refreshing` | Kept independent expiration/no-refresh lease behavior. |
| `deeply_nested_turns_skip_the_matcher_without_overflowing` | Kept worker-stack and depth ceiling check, using a supported Chat Completions payload rather than removed Gemini input. |

## Evidence

All 22 final LCP tests passed in a user/network namespace with only loopback enabled:
`unshare --user --map-root-user --net sh -c 'ip link set lo up && CMAKE=/home/zak/.local/share/mise/installs/cmake/4.5.0/cmake-4.5.0-linux-x86_64/bin/cmake CARGO_BUILD_JOBS=2 nice -n19 cargo test --offline -p cliproxy-internal lcp::tests --lib'`
Log: `/tmp/cliproxy-parity-migration/lcp-final.log`.

Before oracle retirement, the replacements and both original replay suites passed together (23 tests at that point). Added final tool permutations/long-candidate coverage passed after retirement.

Sensitivity control: temporarily reversed production longest-trajectory preference (`>` to `<` in `newest_matching`). The reference model failed at seed 1/step 41 with a concrete fork/session/parent mismatch, log `/tmp/cliproxy-parity-migration/lcp-mutant.log`. Exact original `lcp.rs` content was restored and subsequent checks passed. This is an assertion failure after compilation, not a build-failure control.

Obsolete fixture/recorder eligible for parent removal: `crates/cliproxy-internal/tests/gateway/fixtures/lcp_go.jsonl.zst`, `crates/cliproxy-internal/tests/gateway/reference/lcp/`. No Go build was used.
