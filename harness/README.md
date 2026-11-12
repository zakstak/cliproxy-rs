# Local differential harness

From the repository root, run:

```sh
./harness/accept                      # the acceptance command: every step below, one verdict
./harness/run                         # capture only: historical target
./harness/run --target selective      # capture only: selective target
```

## Revisions

[`manifest.json`](manifest.json) is the one revision manifest. `manifest.py check` refuses it
when a pin disagrees with the source it mirrors: the CI Rust toolchain, the Codex fixture
manifest and `FIRST_PARTY_COMMIT`, or the fixture counts.

| Target | Go revision | Role | Suite |
| --- | --- | --- | --- |
| `historical` | v8.0.10, `6fecc6e5567912661654a4eaf9b8f5436facd1c2` | The revision the 50 Claude cases were recorded against | 50 Claude cases (`cases()`) |
| `policy` | v8.0.35, `ae3ee3be303fb07fb8173279923413005bf32177` | AGENTS.md's frozen default reference. Not a comparison target; it only records goldens for `golden_cases` | none |
| `selective` | v8.0.67, `8bba7a77c6bcbfd993516d26606bfecaa4ff49d0` | Separate selective comparison ([ledger](../docs/parity-audit/selective-v8.0.67.md)) | the 50 cases plus 12 Grok and Muse cases (`selective_cases()`) |

No target is a relabelled copy of another, and no golden fixture changes provenance. Codex
has no Go target. Its contract is first-party `openai/codex` at `44ccf394573c`, checked by
the `codex-contract` cargo group.

For each executed target, `run` clones and builds the **unmodified** Go revision. It also
builds the Rust test driver, runs the collector's regression tests, and starts both proxies
against one scripted local TLS upstream. The selective Grok and Muse cases add a plain-HTTP
loopback upstream.
No production source or workspace manifest is changed. The Rust driver uses the
production config loader, credential loader, runtime, routes and Claude executor;
`ClaudeExecutor::with_hooks` injects test trust and dial routing only. It is not a
test of the production binary's command-line or listener setup.

## Prerequisites and safety

Linux, Go **1.26+**, Rust with Cargo, CMake, Clang, Perl, Python 3, OpenSSL, util-linux
(`unshare`, `mount`), and iproute2 (`ip`) are required. Rust builds BoringSSL.
Unprivileged user, network, mount and PID namespaces must be enabled. The command
does **not** install packages, change the machine's global hosts/trust files, or
fall back to an unrestricted run if isolation fails. A failed prerequisite or
namespace setup stops the run before either proxy starts.

Downloads from GitHub and language dependency registries happen during the build,
before isolation. Runtime has only loopback interfaces and routes; an explicit
egress check requires `ENETUNREACH`. The private `/etc/hosts` maps
`api.anthropic.com` and `platform.claude.com` to `127.0.0.2`; the Rust client's dial
override maps those logical hosts to `127.0.0.3`. One server listens on port 443
and labels captures by the local destination address. URL, Host, SNI and port all
remain first-party, including OAuth refresh. TLS verification stays enabled using
an ephemeral test CA, not an insecure TLS setting. No real credentials are read.
Each proxy gets a whitelist-only environment and its own generated working/auth
directory. The input credentials, caller key, device/account identity and request
bytes are identical. Config differs only in bind port and writable auth path, and, for the selective Grok and Muse cases, in the loopback address of the configured base URL (`127.0.0.2` for Go, `127.0.0.3` for Rust).

The Go binary's unrelated background update attempts can appear as denied network
errors in its log. They cannot leave the network namespace. `-local-model` and
disabled management panel updates suppress catalog/panel downloads. All test
credentials contain `FAKE` or use `example.invalid`. Successful runs delete their
generated private test keys; the public CA/certificate and captures remain.

An existing clean reference checkout can avoid cloning:

```sh
CPA_GO_REFERENCE=/absolute/path/to/CLIProxyAPI ./harness/run
CPA_GO_REFERENCE_SELECTIVE=/absolute/path/to/CLIProxyAPI-v8.0.67 ./harness/run --target selective
```

Without them, the historical checkout is `harness/.cache/CLIProxyAPI` and the selective one
`harness/.cache/CLIProxyAPI-8bba7a77`. The wrapper refuses a different revision, modifications
or untracked files, including any the Go build itself would leave. It never
changes an existing reference checkout. Build output goes in `harness/.cache/`
and `harness/rust/target/`, both ignored by Git. The driver's separate workspace
and lockfile keep Go/network requirements out of `cargo test --workspace`.

## Acceptance

`./harness/accept` runs both differential targets, evaluates each one, runs the mutation
control on each capture, runs every cargo group and combines exactly those receipts. The
collector only records; `acceptance.py` and [`judge.py`](judge.py) decide. Nothing is
accepted on a capture without these checks.

**Per-side invariants** apply to every case on both targets, and no divergence can
relax them:

- credential ownership: each upstream request carries only its provider's credential
  (Claude OAuth token, Grok key or Muse key), never a client key or another provider's
  secret, and no upstream secret reaches the client;
- capture consistency: raw heads match the parsed request line, status and headers;
  chunked or plain wire bodies decode to the compared body; Content-Length matches;
- generated values (`generated_headers`) match their documented pattern;
- upstream writes: every captured upstream request finished writing its scripted reply
  (`write_result` is `complete`). The exception is `invariants.upstream_write_failures`
  (`client-disconnect`, whose client abandons the stream). There a write may fail, but only
  with an outcome the collector records, and Go and Rust must agree;
- native bytes: `native-signals` sends exactly the request recorded from Go v8.0.10
  ([golden](expectations/native-signals.v8.0.10.json)), and `count-tokens` forwards the
  client's body byte for byte;
- a refused client or unusable credential reaches no provider.

**Dimensions** are compared Go against Rust: status and reason, upstream request lines,
ordered upstream headers, upstream bodies byte for byte, downstream field values
(case-insensitive names), downstream bodies byte for byte, TLS handshakes, and the
persisted credential. Structured values (status, write results, TLS, the credential and
decoded JSON) are compared with exact JSON types: `0` is not `false` and `13` is not `13.0`.

**Named divergences** state the exact delta each side must show. Each predicate names one
of these:

- a header value per side (`null` for absent);
- a body edit with an exact match count;
- a JSON element only one side lists, or a set compared without order, with evidence of
  nondeterminism;
- a generated JSON value, named by a shared mask: its exact JSON type and narrow format are
  checked on each side before it is masked;
- an extra Go TLS handshake that must equal a reference handshake;
- a credential field value;
- for a case where one reference cannot answer, a replaced comparison. Each side must
  match a complete golden in every dimension the replacement covers.

A predicate whose delta is missing or different fails, and everything a predicate does
not name is still compared. There are no path or assertion exemptions, and the manifest
validator refuses them. Kinds: `pending-feature` and `deferred-feature` (owning issue),
`unowned` (Go commit found, no owner), `unattributed` (states what is not known),
`documented-difference` (ledger entry), `later-reference` (historical only, Go evidence),
`nondeterministic` and `harness-boundary`. A pending or unattributed entry grants no
adoption.

**Raw differences** must fall in a dimension whose check passed. Any other path fails,
including a TLS change, an extra request or a read error.

**Case universe**: the expected cases come from the current fixtures and the manifest,
not from the run's summary. Missing, extra or duplicate cases, or a summary whose target,
tag, commit, role or suite disagrees, fail. A `--case` run is labelled partial (`partial-pass`,
exit 3) and can never satisfy combined acceptance.

**Provenance**: before each run, `provenance.py` records HEAD, its tree, worktree
cleanliness, the manifest hash, the driver executable's hash, the Go binary's hash and its
embedded VCS stamp. After the run it checks that all of them are unchanged, that cargo
still reports the same driver as fresh, and that the Go stamp names the clean pin. The
cargo groups do the same for every test executable they run.

**Cargo groups**: `claude-router`, `codex-contract`, `xai`, `meta`, `refusals` and `controls`
list exact test names. Each test executable is built and hashed, then listed, then run
directly with `--exact` inside `ui/scripts/isolated.sh` (loopback only, caller uid,
`CPA_TEST_NO_SKIP=1`). A run passes only if every exit is 0, there is exactly one libtest
summary line, and it counts every requested test as passed with none failed or ignored.
A timeout, a signal or a failed listing is unavailable. A nonzero exit is never a pass,
whatever lines were printed.

Each execution record keeps what `combine` re-derives its outcome from:

- the artifact identity cargo reported before and after the run: package and its
  directory, target kind and name, crate types, source file, test profile, executable
  and SHA-256;
- the requested test names;
- the listing command, its exit and the complete list of test names;
- the run command, its working directory and isolation, exit and timeout;
- every individual result line and the parsed summary.

This checks structural consistency only. It is not a cryptographic proof of a build and
does not detect a coherently fabricated receipt.

**Goldens** ([`golden.py`](golden.py), [`expectations/`](expectations/)) are complete per-side
expectations recorded from the Go side of a pinned, provenance-verified capture, never from
cliproxy-rs. A golden holds:

- every upstream request: line, ordered headers, body and the collector's write result;
- every client reply: status, reason and body;
- every TLS handshake.

Values are literal except `{{mask:NAME}}` tokens. Each mask is named in the manifest's
`masks` with a narrow pattern: the minted request id, the date, `cch`, and the compaction
ids, time and capsule. A golden declares which body masks it uses, and the validator
refuses any other mask. Generated JSON values (`downstream_json.generated`) name the same
masks, so there is one format definition, not a looser parallel one. A mask may also
declare its exact JSON type (`json_type`) and a structural check (`decode`). The compaction
capsule must be `cpa-ag-compact-v1:` and canonical unpadded URL-safe base64 of at least a
12-byte nonce and a 16-byte tag, as in `crates/cpa-exec/src/compaction_capsule.rs`. The
random ciphertext is never compared, and a valid structure says nothing about provider
correctness. Recorded goldens:

| Golden | Source | Use |
| --- | --- | --- |
| `native-signals.v8.0.10.json` | historical capture | cliproxy-rs native bytes, always (invariant) and as the Rust side of C2 |
| `native-signals.v8.0.67.json` | selective capture | Go side of C2: observed v8.0.67 behaviour, no adoption |
| `compact-claude.v8.0.10.json` | historical capture | Go side of historical compaction: the 501, no provider traffic |
| `compact-claude.v8.0.35.json` | `harness/run --target policy --golden` | Rust side of historical compaction: what the frozen v8.0.35 reference sends and answers |

```sh
CPA_GO_REFERENCE_POLICY=/path/to/clean/v8.0.35 ./harness/run --target policy --golden
python3 harness/golden.py record RUN_DIR compact-claude --covers upstream,status,downstream-body,tls \
  --masks date,cch,compact-response-id,compact-item-id,compact-capsule,unix-seconds --about "..." --out FILE
```

A policy run captures only the manifest's `golden_cases`, is labelled
`purpose: golden-recording`, and can never be evaluated as acceptance. Recording checks
that the golden matches the capture it came from.

**Mutation control** edits real captured cases, on the Rust side unless a control names Go,
and requires each edit to fail: request and reply bytes, status, credential swaps, tool
arguments, the TLS ClientHello, persisted `disabled` (`true`, and `0` on either side), drift of
a named delta, deltas outside it, caller text, `max_tokens` and failed or unfinished upstream
writes inside a replaced comparison on either side, generated values of the wrong JSON type,
malformed capsules, and an integer turned into a float (`MUTATIONS` in `acceptance.py`).

**Combine** requires each of the five receipts exactly once and validates every record in
them. Totals are recomputed from the records, never trusted:

- the differential cases are exactly the fixture suite's cases (minus named skips), once
  each, in order, each a clean pass with no unexplained difference, and their hits name
  manifest divergences with none stale;
- the mutation controls are exactly `MUTATIONS` for that target, once each, each caught,
  on the same capture the differential receipt evaluated;
- the cargo groups and their tests are exactly the manifest's, once each, all passed. Every
  test target has one execution record. Its artifact is the cargo test artifact of that
  target (package, kind, name, source file, test profile, and an executable named for the
  crate), identical and fresh after the run, and the executable is still on disk with the
  recorded hash. No two targets share an executable or a hash. The record's commands run
  that executable, with `--exact` and exactly the manifest's names. The listing is complete
  and contains every requested name; on an owned target it contains nothing else. There is
  one `ok` result line per requested name, and the summary is `ok` with every requested
  test passed and `filtered_out` equal to the listed names minus the requested ones.
  Missing, failed, ignored and not-run lists are derived and must match. A test's
  `passed` label counts only when its target's record verifies;
- the driver and Go binaries were recorded before and after each run, are identical, and
  the Go stamp names the clean pin;
- every receipt's source before and after equals the current clean worktree and manifest,
  and `rust_revision` matches it.

The combined receipt records each input receipt's SHA-256.

Committed evidence is usually one commit after the tested source. `acceptance.py
verify-evidence harness/evidence/receipts/acceptance.json` checks that the tested commit's
tree matches the receipt and that later commits change only `harness/evidence/`.

```sh
python3 harness/acceptance.py evaluate harness/.cache/latest-selective
python3 harness/acceptance.py cargo
python3 harness/manifest.py check
```

## Results and exit codes

`harness/.cache/latest` points to the last completed run, and `latest-historical` and
`latest-selective` to the last run of each target. Each fixture directory
contains `fixture.json`, `go.json`, `rust.json`, and `diff.json`, plus per-proxy
config, fake auth file and process log. `summary.json` lists statuses, difference
counts and capture/transport errors. Review all differences, not just statuses:

```sh
jq . harness/.cache/latest/summary.json
jq . harness/.cache/latest/buffered/diff.json
jq '.upstream.tls[].profile' harness/.cache/latest/buffered/go.json
jq '.upstream.http[].headers' harness/.cache/latest/count-tokens/go.json
```

Raw ClientHello TLS records, raw HTTP request heads, exact upstream body bytes,
downstream status/header/body bytes and chunked transfer framing are retained as
hex. Ordered/cased headers and parsed TLS fields are also readable JSON. JA3 and
JA4 are calculated from the observed ClientHello, not the advertised User-Agent.
The historical suite has 50 Claude cases (`cases()` in `fixtures.py`); the selective suite
adds 12 Grok and Muse cases (`selective_cases()`). The committed
[evidence archive](evidence/2026-10-02.json.gz) is from the first baseline run, when
there were 33 cases, and holds their fixtures, observations, diffs and summary; use
`gzip -dc` and `jq` to inspect it offline.
The collector's Go-derived 508-byte fixture asserts JA3
`d871d02cecbde59abbf8f4806134addf`; JA4 hashing follows
[FoxIO's technical specification](https://github.com/FoxIO-LLC/ja4/blob/main/technical_details/JA4.md).
Forced reconnect captures both cold and resumed handshakes, including PSK identity
and binder lengths and server-confirmed session reuse.

For `run`, exit **0** means capture completed, **not** parity; use `accept` for a verdict. Exit **1**, with `--strict`,
means at least one captured field differs. Exit **2** means a prerequisite,
capture or transport failure; an unexpected startup failure also exits nonzero.
Cases that should exercise inference require an observed upstream request, so a
startup/model-registration failure cannot masquerade as provider parity evidence.
The Go health endpoint becomes ready before registration, so readiness also waits
for its `full client load complete` log marker. Expired-token testing waits for
the actual Go token rotation to be persisted, rather than sleeping a guessed
refresh interval.

```sh
./harness/run --case buffered --case count-tokens
./harness/run --case tls-reconnect-two-turns --strict
python3 -m unittest discover -s harness -p 'test_*.py'
cargo clippy --manifest-path harness/rust/Cargo.toml --all-targets -- -D warnings
cargo fmt --manifest-path harness/rust/Cargo.toml --check
```

## Comparison boundaries

Only the downstream Date **value** is replaced with `<wall-clock>` when diffing;
presence, order and casing remain significant. Raw TLS records are retained but
not byte-compared: client randoms, session IDs, ephemeral key bytes, opaque PSK
identities and binders are intentionally nondeterministic. Their lengths, ordered
extensions, ciphers, groups, signatures, supported versions, ALPN and session reuse
are compared. HTTP request/response bodies are never JSON-normalized. Billing
blocks, tool names, metadata and request/trace IDs are not stripped. Consequently,
strict comparison also reports generated ID and chunk-boundary differences; it
is not a deterministic semantic-only CI gate.

Fixtures cover buffered Messages, verified native signals and cloaking with
custom/native MCP tools and an unknown beta, tool-use history/choice rewriting,
buffered/SSE inverse name restoration, fragmented/CRLF SSE, pre-stream rejection, mid-stream errors,
truncation, events after `message_stop`, disconnect after the first event,
first-party count_tokens, 400/401/429/500/503 and empty error bodies, Retry-After
and Anthropic quota headers, declared/unlabelled gzip, gzip SSE, deflate,
model catalog negotiation, auth failures/query precedence, disabled credentials,
expired-token refresh, two-turn continuity, and TLS reconnect/resumption.

This is a Claude-first tested slice, not full drop-in parity. Grok and Muse are compared
on Responses, Chat Completions and Messages over a plain-HTTP upstream only; Codex is
covered by its contract tests, not by Go. It does not exercise
Management v8, other translations, OAuth login/code exchange, concurrent
refresh, credential deletion races, multi-credential retries/cooldowns, HTTP/2
upstream requests, chunked upstream request bodies, proxy transport modes,
Brotli/zstd, TLS 1.2 or performance/timing parity. The upstream intentionally
negotiates HTTP/1.1 while recording the client's full offered ALPN. The disconnect
case observes upstream writes failing before process shutdown, not internal
lease completion or an exact cancellation deadline. The Rust transport-injection
and runtime-observation gaps are described in the differential review.
