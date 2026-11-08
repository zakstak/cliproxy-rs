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

Failed attempts do not receive the marker. A native non-streaming Claude JSON response without usage
still calls `UsageSink::publish()` without `usage_required()`: its pre-set outcome
produces an ordinary record without this marker. Absence of the marker therefore
does not prove that usage was observed. The additional records' zero tokens mean
unknown usage, and `failed: false` does not establish successful completion.
Ordinary records retain Go's accounting.

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
Anthropic returns exact aggregate usage, not per-section lengths. Offline replay
scales these byte positions to that returned aggregate; relative section token
lengths remain estimates. Media lengths cannot be scaled this way, so replay
leaves media requests unmodelled. Exact section counts would require additional
provider calls, which this feature does not make.

Work is linear in body size, with a bounded lookback ring. Tokenization and the
tokenizer's resident allocation are absent. The observation copy and hashing are
skipped unless both usage collection and the separate telemetry opt-in are enabled.
Collection still parses and hashes the final payload synchronously before sending;
large bodies can delay async workers. Use the mock load check below before enabling
collection on a constrained host.

Restarts rotate every fingerprint and identity. Do not interpret a hash change
across a restart as a prompt change or cold-cache proof. Restart boundaries cannot
join sessions or detect double writes; use wake-up exports from the same process
epoch. The replay does not model reuse across epochs.

The new fields contain no prompt content or credentials. Existing Go-compatible
usage fields are unchanged and can contain client keys and account metadata. A
collector must protect the full record or export an allowlist containing
`execution_id`, `request_id`, `failed`, `usage_observed`, `tokens`, and `claude_cache`.

## Offline replay

`bench/claude-cache-replay.py` accepts JSONL, one usage record per line, and reports
the last seven days ending at the latest attempt end. Earlier supplied records
warm the simulation without contributing to that week's costs. Duplicate
`execution_id` values are skipped. A short export reports its coverage rather
than claiming to contain a week.

```sh
python3 bench/claude-cache-replay.py week.jsonl --wakeups wakeups.jsonl > comparison.json
python3 bench/test-claude-cache-replay.py
```

The existing usage queue retains records for 60 seconds by default, at most one
hour. It is not a week-long archive. A collector must continuously drain the queue
or consume the RESP channel into a protected JSONL file. This change adds no
collector or live service. Missing collection intervals cannot be reconstructed.

The report compares these policies:

- `observed`: replay the TTLs actually sent, to check the model against real usage.
- `5m`: every existing breakpoint uses five minutes.
- `1h`: every existing breakpoint uses one hour.
- `hybrid`: tools and system breakpoints use one hour; message breakpoints use five
  minutes. Breakpoint positions remain unchanged.
- `1h+bounded-pings`: simulate a matching-prefix refresh after 57 minutes without
  matching activity, at most three pings and four hours after real activity.
  Pings are permitted only after successful `main` attempts. Real session activity
  cancels the old timer. Errors, compaction, helper requests, and scope or prefix
  changes prevent that old timer from firing. An expired prefix fails the local
  preflight and stops the chain.

The simulation anchors sliding expiry at request start and makes a cold entry
available at the first response byte. Concurrent cold starts can both write. It
uses the longest matching live prefix within the recorded lookback, accounts for
new tail writes, and separates account and workspace scopes. Prefix positions are
scaled from serialized fingerprint-input bytes to each attempt's exact returned
cache-read plus cache-write total. Media and `usage_observed: false` records remain
unmodelled, cancel session maintenance timers, and never schedule simulated pings.
The relative positions of mixed-TTL segments remain estimates.
`replay_validation` compares the observed-TTL simulation with actual reads and
writes. Large residuals indicate missing cache state, collection gaps, or model
error; do not treat policy cost differences as measured savings in that case.

Default prices are Opus 5.5 API-equivalent dollars per million tokens: $4 input,
$5 five-minute writes, $8 one-hour writes, $0.20 cache reads, and $20 output.
Other models and fast requests are excluded from priced totals, but retained as
activity that cancels session timers and can flag wake-up writes. `--model`, `--input-usd`,
`--output-usd`, and `--read-multiplier` select another pricing scenario. Pings add
128 assumed uncached input tokens and zero output by default. Adjust
`--ping-input-tokens` to test overhead. This is an optimistic simulation of
successful refreshes, not evidence that OAuth prewarming works or is permitted.
There is no subscription-quota conversion.

Real input and output charges, including observed compaction and retries, stay in
each policy. Requests without usable cache measurements are not invented: their
observed usage remains charged, and `unmodelled_requests` identifies the gap.
Unknown write TTLs contribute a cost range, `api_equivalent_usd_low` through
`api_equivalent_usd_high`. Known and unknown writes are computed per attempt before
summing, so zeroed failure aggregates cannot erase another attempt's unknown-TTL
writes. `observed_tokens.cache_creation_tokens` retains the Go aggregate;
`effective_cache_creation_tokens` also includes exact buckets preserved on
failures. Replay validation compares against that effective total.
Left-edge cold state, unobserved eviction or routing,
incomplete exports, and the unknown return after the right edge limit conclusions.
Pings before the right edge cost money even if the user never returns. Future
compaction behavior, additional maintenance output, quota pressure, logout, and
unlogged errors cannot be inferred from these records.

## Scheduled wake-up flags

An optional wake-up export supplies one JSON object per line:

```json
{"timestamp":"2026-10-15T08:00:00Z","session_hash":"<matching claude_cache.session_hash>"}
```

The timestamp can also be Unix milliseconds. Use the hash observed for that
scheduled thread, not its raw thread ID or credentials. The report flags cold
writes within two minutes after each wake-up. It distinguishes identical-prefix
rewrites, overlap before the earlier first byte, and changed scope, model,
thinking, effort, beta, tools, system, or history fingerprints.

These are candidate events, not a double-write diagnosis. A schedule export is
required because the proxy cannot observe the Amp scheduler. No export produces
`wakeups_provided: false`, not an assertion that scheduled double writes did not
happen.

## Deployment boundary

Mock-upstream tests verify both response modes, privacy of the additions, final
TTLs, and byte-identical traffic with telemetry on and off. CI also runs the
network-free replay fixtures. A week of new production telemetry is still needed
before choosing a TTL or evaluating a maintenance policy. Publishing a release
or replacing Core's running proxy requires Vayun's explicit approval. No live
Claude login is exercised by these tests.

### Mock load check

The existing release latency harness supports telemetry on/off, a synthetic
base64 image, two proxy workers, and an unrelated 10 ms SSE stream. Run alternating
off/on pairs at least five times, for both workloads and both transport modes:

```sh
cargo build --release -p cpa-server --example claude_latency --locked
for workload in text media; do
  size=1000000; media=0
  if [ "$workload" = media ]; then size=30000000; media=1; fi
  for repeat in 1 2 3 4 5; do
    for telemetry in off on; do
      CACHE_TELEMETRY="$telemetry" MEDIA="$media" UNRELATED_STREAM=1 \
        WORKERS=2 N=2 WARMUP=2 CONC=2 SIZES="$size" \
        bench/latency.sh /tmp/cache-load.jsonl target/release/examples/claude_latency
    done
  done
done
```

`bench/latency.sh` requires a loopback-only network namespace. Set `CPUS` to the
host's permitted CPU list. Every timed request must reach the mock, return HTTP
200 and `message_stop`; the harness asserts that the telemetry toggle takes effect.
Compare `proxy.up_first_ms`, `proxy.process_cpu_ms`, `process_vmhwm_kb`, and
`proxy.unrelated_stream_max_gap_ms`. CPU and peak RSS cover the entire harness
(proxy, mock, and client), not an isolated proxy process. The media source is
synthetic and is accepted only by the mock; it is not an Anthropic media validation
test. These measurements do not substitute for host-specific deployment evidence.
