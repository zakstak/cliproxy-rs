# Claude cache telemetry

Claude Messages attempts add `claude_cache` to the existing usage queue and RESP
`usage` channel only when usage statistics and the separate, default-off
`observability.usage.claude-cache-telemetry` setting are enabled. For example:

```yaml
observability:
  usage:
    usage-statistics-enabled: true
    claude-cache-telemetry: true
```

Management must also be enabled for the usage queue to accept records.
This is observation only: it
does not change request bytes, cache markers, TTLs, routing, or responses. It never
sends maintenance requests or token-count requests.

## Record fields

`claude_cache.schema_version` is `2`. Each record covers one upstream attempt,
including failed attempts that reached request preparation.
An additional queue observation marked `usage_observed: false` is emitted on
`Ending::Done` (completion or client drop) only when all these conditions hold:

- The attempt was not discarded and has not already been published.
- The executor has not published an outcome.
- The executor marked the attempt as usage-required, but no usage was observed.
- Collected Claude telemetry is present, requiring the telemetry opt-in and usage
  collection to be enabled.
- The usage queue accepts records (management and usage statistics are enabled).

Every other record without reported token usage carries the same marker
([#99](https://github.com/zakstak/cliproxy-rs/issues/99)), with or without telemetry:
a failed attempt before usage arrived, or a native non-streaming Claude JSON response
without usage. Absence of the marker therefore means the provider reported usage.
Zero tokens on a marked record mean unknown usage, and `failed: false` on an
additional record does not establish successful completion.

- `breakpoints` lists the final upstream cache markers in tools, system, and
  messages order. Each entry has `path`, `ttl`, a cumulative `prefix`, and
  `lookback` fingerprints within Anthropic's 20 lookup positions, including the
  breakpoint itself. Consecutive tool-use blocks and consecutive tool-result
  blocks share a position. An omitted TTL is recorded as Anthropic's default
  `5m`. Top-level automatic caching appears as `cache_control:<last-eligible-path>`:
  thinking and empty text suffixes are skipped. A same-TTL explicit marker at
  that target is a no-op, not a duplicate breakpoint. `automatic_cache_ttl`
  retains the top-level TTL even when no target exists or explicit TTLs conflict.
- `tools`, `system`, and `history_prefix` contain `hash` and `bytes`, the byte size
  of the serialized fingerprint input described below, not the raw section size.
  No online tokenizer runs. History ends at the last message cache breakpoint. No cached
  history leaves an empty fingerprint. Cumulative breakpoint fingerprints include
  the preceding tools, system, and message roles.
- `media_present` flags image, document, audio, or video blocks, including nested
  tool results. Their full contents still participate in hashes and byte lengths.
- `model`, `thinking_type`, `thinking_budget`, `thinking_hash`, `effort`, `betas`,
  `speed`, and `service_tier` describe the final body and beta headers. The full
  thinking configuration is hashed, not stored. Prefix hashes also bind the
  model, thinking, output configuration, tool choice, context management, speed,
  tier, and betas.
- `session_hash` hashes the final wire session, falling back to the dispatch
  session. `account_hash` hashes the selected credential's account UUID, falling
  back to its credential ID. The latter is stable only while that credential
  ID remains unchanged. OAuth token refresh does not change an account UUID.
  `scope_hash` also binds the wire account UUID, upstream base URL, caller isolation, and final
  organization, project, and workspace headers. No identifiers or header values
  are stored in these additions. Without an explicit client session, a derived
  session can change when its instructions change. All hashes are keyed with a
  private process-local secret; they are stable only until the proxy restarts.
- `request_start_unix_ms`, `first_response_byte_unix_ms`, and `end_unix_ms` are UTC
  Unix milliseconds. Start is the upstream send, not client arrival. End is the
  attempt's completion or cancellation, not the downstream socket flush. Missing
  stages are `null`, not a synthetic zero timestamp.
- `request_type` is `main`, `helper`, `subagent`, `compaction`, or `retry`.
  `base_request_type` retains the original kind for retries. `attempt` starts at
  zero and increments across proxy failover and credential-refresh retries within
  one client request. Existing `request_id` joins those attempts. A positive
  client `x-stainless-retry-count` also classifies a request as `retry`.

Known Claude helper and subagent signals and the server's compaction metadata
determine the kind. A producer can supply the logging-only
`x-cliproxy-request-type` header with one of those five values when its purpose is
not visible to the proxy. The header does not select a traffic policy. Without
such a signal, an ordinary Messages request is `main`; the proxy cannot reliably
infer an Amp compaction or scheduled wake-up from prompt text.

The existing `tokens` object adds `cache_creation_5m_tokens` and
`cache_creation_1h_tokens` from Anthropic's
`cache_creation.ephemeral_5m_input_tokens` and
`cache_creation.ephemeral_1h_input_tokens`. SSE output-only updates retain earlier
cache buckets. A returned zero is stored. An omitted bucket stays absent: it is
not inferred from the requested TTL. Aggregate reads and writes remain unchanged.
Returned TTL buckets survive an executor-published failure even when Go's
aggregate failure accounting reports zero tokens.

## Fingerprint and length limits

Exported hashes are domain-separated HMAC-SHA256, keyed with a random 256-bit
process-local secret. The secret never leaves memory, is not persisted or logged,
and is not derived from credentials. Collection is skipped if secure randomness
is unavailable. This prevents exported identities, key-derived credential IDs,
tool aliases, or wire metadata from becoming offline credential verifiers.
Content hashes wrap SHA-256 over the final prepared JSON blocks, paths, and roles,
with length framing.
Only the blocks' `cache_control` objects are omitted, so moving a marker or choosing
another TTL does not change the matching content fingerprint. JSON string escapes
are normalized by decoding and serializing the observation copy. Field order,
text, tool names, signatures, and other content remain significant. These are
proxy fingerprints, not Anthropic's private cache keys.

Each `bytes` value sums the sizes of JSON-serialized `(path, role, block)` tuples,
with each block's `cache_control` removed. This includes path and role strings
and tuple JSON framing. It excludes the eight-byte length frame added to the
hash for each tuple, model/configuration hash inputs, and provider framing.
These are fingerprint-input byte sizes, not section byte lengths or token counts.
Anthropic returns exact aggregate usage, not per-section lengths. These byte
sizes do not establish section token counts, especially for media. Exact section
counts would require additional provider calls, which this feature does not make.

Work is linear in body size, with a bounded lookback ring. Tokenization and the
tokenizer's resident allocation are absent. The observation copy and hashing are
skipped unless both usage collection and the separate telemetry opt-in are enabled.
Collection still parses and hashes the final payload synchronously before sending;
large bodies can delay async workers.

Restarts rotate every fingerprint and identity. Do not interpret a hash change
across a restart as a prompt change or cold-cache proof. Restart boundaries cannot
join sessions or detect double writes. Any schedule export used to correlate
wake-ups must use hashes from the same process epoch.

The new fields contain no prompt content or credentials. Existing Go-compatible
usage fields are unchanged and can contain client keys and account metadata. A
collector must protect the full record or export an allowlist containing
`execution_id`, `request_id`, `failed`, `usage_observed`, `tokens`, and `claude_cache`.

## Collection and interpretation limits

The existing usage queue retains records for 60 seconds by default, at most one
hour. It is not a week-long archive. A collector must continuously drain the queue
or consume the RESP channel into a protected JSONL file. This change adds no
collector or live service. Missing collection intervals cannot be reconstructed.

Unobserved cache eviction or routing and incomplete exports limit conclusions.
Future compaction behavior, quota pressure, logout, and unlogged errors cannot be
inferred from these records. A schedule export with matching `session_hash` values
is required to correlate scheduled wake-ups because the proxy cannot observe the
Amp scheduler. Matching hashes and nearby cache writes alone do not establish a
double write.

## Deployment boundary

Mock-upstream tests verify both response modes, privacy of the additions, final
TTLs, and byte-identical traffic with telemetry on and off. A week of production
telemetry is needed before choosing a TTL or evaluating a maintenance policy.
Publishing a release or replacing Core's running proxy requires Vayun's explicit
approval. No live Claude login is exercised by these tests.
