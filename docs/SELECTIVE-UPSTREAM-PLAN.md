# Selective CLIProxyAPI update plan

Research snapshot: 2026-11-11. Status: proposed, awaiting Zak's go-ahead. This document authorizes no implementation, live provider requests, release, or deployment.

GitHub tracking: [#66](https://github.com/zakstak/cliproxy-rs/issues/66) contains the original 48 Go feature issues and the six-task official Codex addendum. Issue creation records the research; implementation still awaits approval.

## Recommendation

Port retained-provider fixes in small verified slices. Start by repairing the comparison infrastructure, then finish the concrete correctness fixes before larger features. Keep Claude, Codex, Grok/xAI, and Muse/Meta. Keep removed providers and native plugins refused.

Do not merge the Go delta wholesale or declare global v8.0.67 parity. Keep the frozen v8.0.35 reference for unchanged non-Codex contracts. Add an explicit selective comparison target and document each adopted behavior. Codex continues to follow [its first-party contract](codex/CONTRACT.md), including endpoint profiles and permitted payload transformations.

The highest-value work is Grok buffered SSE errors and collision-safe login files, Claude stop reasons and fallback reporting, transient credential-read recovery, and shared ingress limits. Client-profile refreshes matter too, but exact identity fixtures need better provenance. Durable refresh journals, weighted routing, broad compression, and distributed Home changes need separate decisions.

## Pinned scope and evidence

| Source | Research pin | Meaning |
| --- | --- | --- |
| This fork | `5565231e2f935e2cf93903a61cfabd872ca0cf3a`, branch `pyre/selective-upstream-port`, version `0.4.1-zak.2` | Starting checkout, matching `origin/fork` when checked. |
| Existing Go policy | `v8.0.35`, `ae3ee3be303fb07fb8173279923413005bf32177` | Frozen default reference for non-Codex behavior. |
| Latest Go release | [v8.0.67](https://github.com/router-for-me/CLIProxyAPI/releases/tag/v8.0.67), `8bba7a77c6bcbfd993516d26606bfecaa4ff49d0` | Published 2026-11-11 at 08:51:43 UTC. Comparison covers 408 commits, including 345 non-merge commits after v8.0.35. |
| Current harness | `v8.0.10`, `6fecc6e5567912661654a4eaf9b8f5436facd1c2` | Older than policy. It mocks Claude only, through three client formats. |
| Upstream Rust | `96561cf0d78715c9f0a3da2b2fa31d307fbd1953`, version `0.4.2` | Still pins Go v8.0.35 in its instructions. No wholesale Rust merge is proposed. |
| Codex authority | [`openai/codex@44ccf394573c`](https://github.com/openai/codex/tree/44ccf394573cabb95e1f751a59f9d76f6f79faf1) and the API documentation cited in the local contract | Go payload mutations and historical Codex goldens do not decide behavior. |

The [commit inventory](research/upstream-v8.0.67-inventory.md) records every non-merge commit in this range. All commit subjects, bodies, and changed-path lists were inventoried. Relevant patches and current Rust paths received targeted source review. This is source triage, not a claim that all 345 patches were exercised or every hunk was independently audited.

Primary sources checked beyond Go:

- [Official Claude Code changelog](https://github.com/anthropics/claude-code/blob/e379c8eca0af5b02a49bf8e784aeefdd1308de6f/CHANGELOG.md). It documents Fable 5.2 support in 2.1.321. It does not establish exact HTTP headers.
- [Official Anthropic Message type](https://github.com/anthropics/anthropic-sdk-python/blob/5139cf2e7d07aa618a6c30b7e0c712f10ccb5ad9/src/anthropic/types/message.py). It describes refusal, pause_turn, and model_context_window_exceeded.
- [Official adaptive-thinking type](https://github.com/anthropics/anthropic-sdk-python/blob/5139cf2e7d07aa618a6c30b7e0c712f10ccb5ad9/src/anthropic/types/thinking_config_adaptive_param.py). Its display values are summarized and omitted. It does not validate the proposed updates value.
- [Official xAI video generation docs](https://docs.x.ai/developers/model-capabilities/video/generation), retrieved 2026-11-11. They describe Video 1.5 Lite and model-specific resolution behavior.

Upstream issue reports and comments claiming a captured client profile are useful reproduction leads. They are not independent proof of provider behavior. This matters most for Claude identity, Muse headers and reminder tools, OAuth scope variants, and new Codex translations.

## Ratings and decisions

Feasibility measures whether a bounded implementation is understood in this checkout. High means the existing architecture supports it. Medium means a protocol, migration, or evidence question remains. Risk measures semantic, credential, compatibility, and persistence risk after implementation. It is independent of feasibility.

Size includes implementation, meaningful regressions, docs, and review. S is about one focused developer-day, M about two to four, L about one to two developer-weeks, and XL multiple weeks or a cross-store protocol. These are rough human-effort bands, not elapsed-time promises. Items sharing code and tests overlap, so do not add the estimates into a schedule.

Core means recommended for the initial selective update after approval. Conditional means resolve the stated evidence or semantic choice first. Follow-up means useful but separable. Defer means keep outside this batch. Audit only means verify existing behavior and add assertions only where missing.

| ID | Item | Decision | Feasibility | Risk | Size |
| --- | --- | --- | --- | --- | --- |
| [H1](#h1) | Selective harness and test contracts | First | High | Medium | L |
| [K1](#k1) | Retained model catalog updates | Core | High | Low | S |
| [C1](#c1) | Claude client profile and Fable 5.2 gates | Core | Medium | Medium | M |
| [C2](#c2) | Claude native client detection and cloak-never preservation | Core | High | Medium | M |
| [C3](#c3) | Explicit Claude fallback and serving-model reporting | Core | High | Medium | M |
| [C4](#c4) | Claude stop reasons and refusal classification | Core | High | Medium | M |
| [C5](#c5) | Claude cache markers and mid-session ordering | Conditional | High | High | M |
| [C6](#c6) | Credential-scoped Claude web-search replay | Conditional | Medium | High | L |
| [C7](#c7) | Persistent Claude session date pins | Defer | High | Medium | L |
| [C8](#c8) | Claude OAuth state and scope separation | Core | High | Medium | S |
| [C9](#c9) | Invalid Claude tool-property workaround | Defer | High | High | M |
| [C10](#c10) | New Claude thinking and forced-tool rules | Conditional | Medium | High | M |
| [T1](#t1) | Deferred tools, ToolSearch, and nested custom tools | Conditional | Medium | High | L |
| [T2](#t2) | Reachable role, reasoning, and parallel-tool translation fixes | Core | High | Medium | M |
| [M1](#m1) | Muse identity and contributor-mode headers | Core | Medium | Medium | M |
| [M2](#m2) | Muse explicit prompt-cache retention | Core | High | Low | S |
| [M3](#m3) | Muse reminder tool namespace | Conditional | Medium | High | M |
| [X1](#x1) | Grok buffered SSE terminal errors | Core | High | Medium | M |
| [X2](#x2) | Collision-safe Grok login filenames | Core | High | Medium | S |
| [X3](#x3) | Grok tool deduplication and image routing rules | Core | High | Medium | M |
| [X4](#x4) | Grok Imagine Video 1.5 Lite | Core | High | Medium | M |
| [X5](#x5) | Grok caller User-Agent policy | Follow-up | High | Low | S |
| [A1](#a1) | Keep credentials during transient watcher read failures | Core | High | Medium | M |
| [A2](#a2) | Targeted OAuth re-login and login proxy | Follow-up | High | Medium | L |
| [A3](#a3) | SIWC bind fencing and read-only status | Core | High | Medium | M |
| [A4](#a4) | Codex renewal and quarantine correctness | Core | Medium | High | L |
| [A5](#a5) | Durable refresh claims and result receipts | Defer | Medium | High | XL |
| [R1](#r1) | Request-fault classification and safe quota evidence | Core | High | Medium | M |
| [R2](#r2) | Canonical model cooldowns and forced-cooldown persistence | Core | High | Medium | M |
| [R3](#r3) | Weighted routing fairness with sticky traffic | Defer | High | High | L |
| [R4](#r4) | Session-affinity never-rebind policy | Follow-up | High | High | M |
| [R5](#r5) | On-demand model discovery refresh | Follow-up | High | Medium | M |
| [U1](#u1) | Structured retained-provider usage and timing | Follow-up | High | Medium | L |
| [L1](#l1) | Bounded and confidential error logging | Core | High | Medium | M |
| [I1](#i1) | Bound zstd decoder window for captured requests | Core | High | Medium | S |
| [I2](#i2) | Bound RESP frame counts, lines, and aggregate bytes | Core | High | Medium | M |
| [I3](#i3) | Bound unauthenticated incomplete HTTP bodies | Core | High | Medium | M |
| [I4](#i4) | Explicit Authorization in CORS preflights | Core | High | Low | S |
| [I5](#i5) | Keep SSE heartbeats between complete frames | Core | High | Medium | M |
| [P1](#p1) | Full-chain HTTP and WebSocket compression | Defer | High | High | L |
| [P2](#p2) | Per-account connection pools | Defer | High | Medium | L |
| [D1](#d1) | Home heartbeat deadline, retry backoff, and idle flusher | Follow-up | High | Medium | M |
| [D2](#d2) | Home distributed rebalance and latency protocol | Defer | Medium | High | XL |
| [D3](#d3) | Draft gateway discovery and bounded mDNS cache | Defer | High | Medium | L |
| [N1](#n1) | Keep existing native Codex protections and image support | Audit only | High | Low | M |
| [T3](#t3) | Codex commentary through Claude clients | Conditional | Medium | High | L |
| [R6](#r6) | Guardian reviews inherit an eligible parent credential | Conditional | Medium | High | M |
| [I6](#i6) | Shared streaming lifecycle and metadata boundaries | Follow-up | High | Medium | M |

## Implementation order after approval

Each slice ends with passing targeted tests, a comparison receipt, and the relevant difference or contract documentation. Finish a slice before broadening scope. A failing witness that exposes a different root cause changes the proposed implementation, not the required behavior.

1. **H1: establish trustworthy acceptance.** Centralize pins without changing old fixture provenance. Add selected semantic assertions, four-provider fake-upstream coverage, and a fail-on-skip acceptance command. Repair local build prerequisites. Preserve current native Codex assertions with N1.
2. **X1, X2, M2, I4: bounded correctness fixes.** Grok terminal errors, collision-safe credential saves, explicit Muse retention, and Authorization preflights have concrete local gaps. Implement and verify each independently.
3. **C4, C3, R1, R2: response and availability correctness.** Fix stop reasons before using those classifications in scheduler and usage logic. Separate forced cooldown from weighted fairness.
4. **A1, A3: credential lifecycle safety.** Preserve good credentials through temporary I/O failures. Fence SIWC binding against account replacement and add read-only status. Do this before changing refresh policy.
5. **I2, L1, I1, I3, I5: shared resource and stream bounds.** Establish logging behavior before changing decompression. Reproduce real-socket and fragmented-frame failures before choosing fixes. Keep good large coding requests and authorized long streams working.
6. **K1, C1, C2, C8, M1, X3, X4, T2: supported clients and provider features.** Require source-backed profile and capability fixtures. Split independent provider changes into reviewable units. Run contract checks for every translated Codex path T2 affects.
7. **A4: renewal correctness.** Change one state transition at a time with fake token endpoints and deterministic race tests. Keep A5 durable journaling outside this slice.
8. **Choose follow-ups separately.** A2, R4, R5, U1, X5, D1, and I6 can follow when their value justifies the work. C5, C6, C10, T1, T3, M3, and R6 remain conditional. Other deferred items stay outside this batch.

This order is a recommendation, not implementation authorization. A go-ahead for the core batch covers the first seven slices, subject to their explicit evidence gates. It does not enable conditional features or release deployment.

## Harness and test design

Use a small manifest with three distinct kinds of evidence:

| Evidence kind | Pin and assertions | Purpose |
| --- | --- | --- |
| Historical Go recordings | Preserve each fixture's recorded revision, including v8.0.10 and v8.0.35. | Detect unintended regressions without rewriting history. |
| Selective Go comparisons | Pin v8.0.67 by full SHA, list feature IDs and named expected divergences. | Exercise adopted non-Codex behavior against a concrete upstream implementation. |
| Provider and client contracts | First-party revision or documentation URL, endpoint profile, scenario, and expected behavior. | Decide Codex behavior and resolve other provider conflicts. |

The Go research clone is detached and read-only. Do not patch it to resemble Rust. A manifest change must not relabel existing Go goldens as current-provider proof.

Keep exact request capture and selective response assertions separate. Exact native pass-through tests compare original bytes, unknown fields, ordering, credentials, status, and headers. Translation tests compare specified meaning and terminal behavior. Do not normalize arbitrary payloads to hide differences. Keep the collector's deliberate Date normalization explicit, and record generated-ID or chunk-boundary differences by scenario rather than globally ignoring them.

The current runner's exit 0 means capture completed. `--strict` reports differences, including legitimate ones. Add an acceptance mode that fails unexpected deviations, missing assertions, accidental skips, and unavailable required fixtures. Exit 2 remains an infrastructure failure. A passing capture without semantic assertions is insufficient.

Extend the existing Claude TLS adapter rather than rebuilding it. Reuse `cpa_server::testing::codex_capture::CaptureUpstream`, `cpa_exec::codex_contract`, and the Codex fixture manifest. Add xAI and Meta through their existing injectable clients and fake base URLs. TLS fingerprint emulation for every provider is not required to verify their request semantics. Native HTTP, native WebSocket, translated Chat, translated Messages, and Responses routes need explicit coverage where the provider supports them.

Run every proxy and mock under loopback-only network isolation, with a new private HOME and auth-dir. Keep the current fail-closed egress probe, private hosts mounts, test TLS CA, environment whitelist, and process cleanup. No real credentials, default auth directory, or synthetic traffic to real provider accounts. Preserve removed-provider and plugin refusal tests when trimming historical fixture suites. A skipped removed-provider case names issue #49; a plugin case names #47.

For changed behavior, update [the differences document](DIFFERENCES-FROM-GO.md), [the audit checklist](parity-audit/checklist.md), and the relevant contract entries. Mark removed-provider and plugin audit rows as excluded historical scope. Do not inflate active coverage by counting skipped removed providers. Add a dated selective-adoption ledger alongside the frozen reference rather than changing the global reference and implying every v8.0.67 contract was adopted.

Add actual harness execution to the repository check workflow, plus documented local commands. The current workflow only compiles the harness adapter. GitHub Actions are disabled for this fork, so workflow edits alone provide no executed CI evidence. Retain local receipts unless the user separately elects to enable Actions. Empty or disabled checks are not green.

During implementation, run the affected crate or filter with `CARGO_BUILD_JOBS=2 nice -n 19`. Before a batch is ready, run fmt, clippy with warnings denied, the relevant contract and harness groups, and the required broader suite in the documented isolated environment. The Go and PostgreSQL differential tests need the CI-style network-denied setup to run fully. Report missing gates rather than counting skips as passes.

## Local feasibility and safe experiment setup

The research checkout was clean before these plan documents. No source, tests, config, Cargo version, reference policy, credentials, or running services were changed.

| Check | Observed result |
| --- | --- |
| Latest released Go and commit range | Verified through GitHub release API and detached Git clone. |
| Existing Python collector tests | Five passed with `python3 -m unittest discover -s harness -p 'test_*.py'`. |
| Rootless user and network namespaces | Probe succeeded with loopback enabled and no default route. Full harness mounts were not exercised. |
| Local prerequisite | `cmake` is absent, so `./harness/run` currently fails preflight. No installation performed. |
| Rust toolchain | Local rustc 1.98.0. Repository workflows pin 1.99.0. Align or explicitly verify before producing a batch receipt. |
| Go toolchain | Local Go 1.27.3-X:nodwarf5. Latest Go module requires 1.26.10. Record the actual compiler in receipts. |
| GitHub Actions | Repository API reports `enabled: false`. |
| Rust builds, clippy, full tests, differential proxy runs | Not performed during research. |
| Live provider or cache checks | Not performed. No provider account traffic was sent. |

The safest development run uses generated fake credentials and loopback mocks inside a rootless network namespace. Build dependencies before entering the namespace, since runtime egress must fail. The existing `harness/run` creates private configuration, HOME, and credentials itself. After prerequisites are present, it is the preferred experiment entry point. Its current results remain historical Claude evidence until H1 is implemented.

For permission-sensitive Rust tests, the existing UID-preserving helper avoids treating namespace root as the file owner with extra capabilities:

```sh
# Build first, before network isolation, after implementation is approved.
CARGO_BUILD_JOBS=2 nice -n 19 cargo test --locked -p cpa-server --no-run
# The test must create its own private auth-dir and fake upstream.
ui/scripts/isolated.sh 'CARGO_BUILD_JOBS=2 nice -n 19 cargo test --locked -p cpa-server <affected-test-filter>'
```

The filter is a placeholder for the implemented regression. Never start the production config as a test, and never copy real auth files into a sandbox. Network isolation does not prevent a process from reading accessible host files. Keep test credentials synthetic and use the repository testing runtime.

Claude cache-semantic changes have a separate mandatory manual live check before merge. Warm, changed-context, and idle continuations must show cache-read and cache-write usage. This research and a general implementation go-ahead do not satisfy that gate. Do not add inference keepalive pings.

Publishing or replacing Core's running proxy is outside this plan and requires Vayun's explicit approval under AGENTS.md. Collector retention of new Claude telemetry must then be verified on natural traffic after an approved deployment. Mock tests cannot establish that result. No version bump, tag, publishing, package distribution, or production rollout is proposed here.

## Features, evidence, and acceptance gates

The ratings above apply to each feature below. Current Rust links name the implementation to inspect. Some entries are confirmed source gaps; others deliberately start with a witness because Rust may already provide the behavior.

<a id="h1"></a>

### H1: Selective harness and test contracts

Tracking issue: [#67](https://github.com/zakstak/cliproxy-rs/issues/67).

First. Feasibility high, risk medium, size L. Dependencies: None.

Add one revision manifest, retained-provider scenario groups, semantic assertions, and an explicit expected-divergence list. Keep the existing raw wire collector. The current runner pins v8.0.10, while policy pins v8.0.35; neither silently becomes v8.0.67. Add the new revision as a separate selective comparison target. Keep old goldens with their original provenance. Make unknown skips and unexpected differences fail the acceptance command.

Evidence: Current runner and adapter inspected. Five Python collector tests pass. No end-to-end run performed.

Current Rust: [`harness/run`](../harness/run), [`harness/differential.py`](../harness/differential.py), [`harness/fixtures.py`](../harness/fixtures.py), [`harness/rust/src/main.rs`](../harness/rust/src/main.rs), [`crates/cpa-server/src/testing/codex_capture.rs`](../crates/cpa-server/src/testing/codex_capture.rs).

Acceptance: Run the existing 50 Claude scenarios against their historical target, then run the new selected cases against the pinned latest target. Cover Claude, Codex, xAI, and Meta through HTTP and applicable streaming paths. Reuse Codex contract capture helpers and manifest. Assert no removed-provider route or plugin becomes accepted. Save revision, scenario counts, passes, skips, named divergences, and toolchain in each receipt.

<a id="k1"></a>

### K1: Retained model catalog updates

Tracking issue: [#68](https://github.com/zakstak/cliproxy-rs/issues/68).

Core. Feasibility high, risk low, size S. Dependencies: H1.

Add verified retained models, including Claude Fable 5.2, GPT-6 Chat Thinking and GPT-6.1 Terra, Muse Ember, and Grok Video 1.5 Lite. Copy only retained catalog sections. Preserve runtime discovery and operator model rules. Do not remove GPT-5.5 or assert subscription entitlements solely because Go changed its static list.

Evidence: Go catalog diff and official Claude Code changelog confirm candidate names. Each capability and entitlement needs its own source.

Current Rust: [`crates/cpa-server/src/models.rs`](../crates/cpa-server/src/models.rs), [`crates/cpa-server/src/model_updater.rs`](../crates/cpa-server/src/model_updater.rs).

Upstream anchors: [`19a0a854`](https://github.com/router-for-me/CLIProxyAPI/commit/19a0a8541fe173773d2c6eb9f68e678762145634), [`c1842be2`](https://github.com/router-for-me/CLIProxyAPI/commit/c1842be2111065160ea7768ffb9a3f71b17b890d), [`b4cbda59`](https://github.com/router-for-me/CLIProxyAPI/commit/b4cbda5915101b1f913e048847209d01037d90d1), [`c69cdd93`](https://github.com/router-for-me/CLIProxyAPI/commit/c69cdd933d1928561f3c1e437e1443f6b6573b6f).

Acceptance: Assert names, provider ownership, supported capabilities, model-prefix behavior, and retained existing entries. Exercise catalog fallback when discovery is unavailable. Record source and date for each new capability.

<a id="c1"></a>

### C1: Claude client profile and Fable 5.2 gates

Tracking issue: [#69](https://github.com/zakstak/cliproxy-rs/issues/69).

Core. Feasibility medium, risk medium, size M. Dependencies: H1.

Update the measured Claude Code profile from 2.1.280 to the evidenced supported client tuple. Go uses 2.1.321, SDK 0.128.0, and Node v26.12.1. Extend preserved-thinking and per-turn gates to Fable 5.2. Do not infer exact headers from a version number.

Evidence: Rust tuple is source-confirmed. Official Claude Code changelog confirms model support. Exact new tuple currently rests on upstream capture claims.

Current Rust: [`crates/cpa-exec/src/claude/profile.rs`](../crates/cpa-exec/src/claude/profile.rs), [`crates/cpa-exec/src/claude.rs`](../crates/cpa-exec/src/claude.rs).

Upstream anchors: [`b4e7dbd2`](https://github.com/router-for-me/CLIProxyAPI/commit/b4e7dbd2c984ec4a74d829ee0425430a20146c77), [`6db7d4b8`](https://github.com/router-for-me/CLIProxyAPI/commit/6db7d4b839d86c5b24e17bbe73bcd4db18ef1b1a).

Acceptance: Use sanitized, source-labelled wire fixtures for header order, beta ordering, field preservation, and the new model. Cover native and cloaked requests. The official changelog confirms Fable 5.2 support in 2.1.321, but does not prove the full wire tuple. Obtain that evidence before accepting the profile.

<a id="c2"></a>

### C2: Claude native client detection and cloak-never preservation

Tracking issue: [#70](https://github.com/zakstak/cliproxy-rs/issues/70).

Core. Feasibility high, risk medium, size M. Dependencies: H1.

Recognize sdk-ts, cli-bg, and valid user-agent extensions. Preserve valid caller headers and request fields when cloaking is disabled. Add a narrowly validated native-entrypoints setting if required for those clients. Keep rejection of malformed identity and credential headers.

Evidence: Rust hardcodes cli, sdk-cli, claude-vscode and x-app=cli. Upstream patches supply candidate fixtures; exact new client forms need recorded provenance.

Current Rust: [`crates/cpa-exec/src/claude/detect.rs`](../crates/cpa-exec/src/claude/detect.rs), [`crates/cpa-exec/src/claude/profile.rs`](../crates/cpa-exec/src/claude/profile.rs).

Upstream anchors: [`20c9095c`](https://github.com/router-for-me/CLIProxyAPI/commit/20c9095cac089c6a8e40554debe4dd8a19d4c14f), [`6876a2eb`](https://github.com/router-for-me/CLIProxyAPI/commit/6876a2eb416d10235f4aac7458bcd04c751e26f7), [`542a525f`](https://github.com/router-for-me/CLIProxyAPI/commit/542a525fcf2a20ec920e538d94d78471b9dda2f5), [`59304330`](https://github.com/router-for-me/CLIProxyAPI/commit/593043301416879f5476939045db063003928771), [`d2e61d19`](https://github.com/router-for-me/CLIProxyAPI/commit/d2e61d19b57174c5c1fb7aaff8702297167d4157), [`6d4102f3`](https://github.com/router-for-me/CLIProxyAPI/commit/6d4102f33f1037988330d2383a950bc6ffa1001f).

Acceptance: Table-test native and foreign clients, extensions, missing headers, configured entrypoints, and invalid values. Compare prepared request bytes and headers, including fields outside known identity paths. Never apply these identity rules to Codex.

<a id="c3"></a>

### C3: Explicit Claude fallback and serving-model reporting

Tracking issue: [#71](https://github.com/zakstak/cliproxy-rs/issues/71).

Core. Feasibility high, risk medium, size M. Dependencies: H1.

Make synthesized server-side fallback opt-in and default it off. Preserve a caller-provided fallback. Report the actual serving model and ordered fallback transitions in supported responses and usage. Rust currently synthesizes fallback lists when absent at claude.rs around line 1298.

Evidence: Source gap; provider behavior still needs the listed witness.

Current Rust: [`crates/cpa-exec/src/claude.rs`](../crates/cpa-exec/src/claude.rs), [`crates/cpa-server/src/usage_record.rs`](../crates/cpa-server/src/usage_record.rs), [`crates/cpa-translate/src/claude_chat_response.rs`](../crates/cpa-translate/src/claude_chat_response.rs), [`crates/cpa-translate/src/claude_responses_response.rs`](../crates/cpa-translate/src/claude_responses_response.rs).

Upstream anchors: [`fa11a573`](https://github.com/router-for-me/CLIProxyAPI/commit/fa11a573b252b30256bef4de952a95feee3c8c9d).

Acceptance: Cover absent, explicit empty, explicit populated, enabled, and disabled fallback. Assert buffered and streamed Messages, Chat, Responses, and usage agree about the serving model. This default change needs an operator-visible difference entry.

<a id="c4"></a>

### C4: Claude stop reasons and refusal classification

Tracking issue: [#72](https://github.com/zakstak/cliproxy-rs/issues/72).

Core. Feasibility high, risk medium, size M. Dependencies: H1.

Map model_context_window_exceeded to Chat length and Responses incomplete with max_output_tokens. Preserve pause_turn and refusal semantics, sparse refusal identity, and tool-use stop reasons across streaming and buffered routes. Keep policy or malformed-request failures separate from credential rejection and account-wide quota. Preserve a successful provider refusal as a stop-reason result. Do not import Go's conversion of a successful refusal into HTTP 400 without separate provider or client justification.

Evidence: Official Anthropic Message type documents refusal, pause_turn, and model_context_window_exceeded. Rust mappings lack the context-window branch.

Current Rust: [`crates/cpa-translate/src/claude_chat_response.rs`](../crates/cpa-translate/src/claude_chat_response.rs), [`crates/cpa-translate/src/claude_responses_response.rs`](../crates/cpa-translate/src/claude_responses_response.rs), [`crates/cpa-exec/src/claude.rs`](../crates/cpa-exec/src/claude.rs).

Upstream anchors: [`aadb34f9`](https://github.com/router-for-me/CLIProxyAPI/commit/aadb34f9161a7d4f1b5a3e30cd286452206d5188), [`c2dfec28`](https://github.com/router-for-me/CLIProxyAPI/commit/c2dfec28d57221be9e042d33e2e1e0da58b16542), [`d495e24c`](https://github.com/router-for-me/CLIProxyAPI/commit/d495e24cbc3965a28afd1ebdd4160eb2b7febb0e), [`df860eb9`](https://github.com/router-for-me/CLIProxyAPI/commit/df860eb945dd72f15a09652decb168168e204488), [`43f27e3a`](https://github.com/router-for-me/CLIProxyAPI/commit/43f27e3ae3abebc422fc35a12d774309f9ca25e3), [`03a8743c`](https://github.com/router-for-me/CLIProxyAPI/commit/03a8743c3cf9d1f803a4503d038b745b2dc2b739), [`c3938b72`](https://github.com/router-for-me/CLIProxyAPI/commit/c3938b72bdaaa803ab983eeeb0005ce59cef505c), [`0a94b2ae`](https://github.com/router-for-me/CLIProxyAPI/commit/0a94b2aefb520469869ef8ea7b66b74a3da68e1a), [`0130d6f3`](https://github.com/router-for-me/CLIProxyAPI/commit/0130d6f38c91e9bb3980484b8a2ef83be71c0a01), [`be691624`](https://github.com/router-for-me/CLIProxyAPI/commit/be69162478da2a082844e8223962fe035ea92138).

Acceptance: Use official stop-reason fixtures. Test terminal event ordering, partial output, prompt overflow error codes, deferred web-search tool outputs, and no cooldown or refresh for request faults. Preserve a model-scoped Fable rejection when account-wide quota evidence is absent.

<a id="c5"></a>

### C5: Claude cache markers and mid-session ordering

Tracking issue: [#73](https://github.com/zakstak/cliproxy-rs/issues/73).

Conditional. Feasibility high, risk high, size M. Dependencies: H1, C4.

Skip empty or whitespace-only cache targets, materialize explicit top-level cache_control onto the correct last cacheable block when required by cloaking, and retain mid-session system or developer placement. Respect the four-marker ceiling and TTL ordering. First write witnesses against current behavior to limit changes to actual gaps.

Evidence: Cache preparation needs targeted reproduction. No live cache experiment was run. Approval to implement does not waive the live merge gate.

Current Rust: [`crates/cpa-exec/src/claude.rs`](../crates/cpa-exec/src/claude.rs), [`crates/cpa-exec/src/claude/signals.rs`](../crates/cpa-exec/src/claude/signals.rs), [`crates/cpa-translate/src/claude_responses.rs`](../crates/cpa-translate/src/claude_responses.rs).

Upstream anchors: [`df07b1e5`](https://github.com/router-for-me/CLIProxyAPI/commit/df07b1e59877e255cefef4eda9a246952f05ba11), [`beb43359`](https://github.com/router-for-me/CLIProxyAPI/commit/beb4335954f2bc85a710d1cb4d0210f4c234b813), [`27762819`](https://github.com/router-for-me/CLIProxyAPI/commit/277628193f54d99b2eeca57ecd7f9ff9b349a996).

Acceptance: Check empty blocks, tool-only tails, explicit TTLs, marker limits, changed-context placement, and linear preparation on large bodies. Before merge, perform the repository-required manual live warm, changed-context, and idle cache-read/cache-write check. Mock success does not satisfy this gate.

<a id="c6"></a>

### C6: Credential-scoped Claude web-search replay

Tracking issue: [#74](https://github.com/zakstak/cliproxy-rs/issues/74).

Conditional. Feasibility medium, risk high, size L. Dependencies: H1, C4.

Add only if the missing replay feature is wanted. Rust does not have the full Go web-search result cache these fixes assume. Scope encrypted result replay to the upstream credential, bound storage, and remove citations whose results are absent. This is a feature addition, not a two-patch fix.

Evidence: Upstream cache-scope fixes are relevant, but their required base feature predates this delta and is absent in Rust.

Current Rust: [`crates/cpa-exec/src/claude.rs`](../crates/cpa-exec/src/claude.rs), [`crates/cpa-translate/src/claude_responses_response.rs`](../crates/cpa-translate/src/claude_responses_response.rs).

Upstream anchors: [`cddc9bb8`](https://github.com/router-for-me/CLIProxyAPI/commit/cddc9bb8148a1d45815b2d99ca0983fb6359423f), [`9acf81d3`](https://github.com/router-for-me/CLIProxyAPI/commit/9acf81d3a6732f541bb40d0247f8ea46b5aa3ef6).

Acceptance: Test credential A versus B, caller isolation, eviction, missing results, citation cleanup, reconnects, and secret-free logs. Preserve native pass-through. Apply C5 live gates if cache semantics change.

<a id="c7"></a>

### C7: Persistent Claude session date pins

Tracking issue: [#75](https://github.com/zakstak/cliproxy-rs/issues/75).

Defer. Feasibility high, risk medium, size L. Dependencies: H1, C5.

Persist the current process-local session date pins with bounded retention and explicit storage ownership. Choose a Rust store suited to this fork instead of importing Go bbolt infrastructure. Define behavior with remote stores and concurrent processes.

Evidence: Process-local date pin implementation is source-confirmed. Persistence policy is an unresolved design choice.

Current Rust: [`crates/cpa-exec/src/claude/session.rs`](../crates/cpa-exec/src/claude/session.rs), [`crates/cpa-store/src/lib.rs`](../crates/cpa-store/src/lib.rs).

Upstream anchors: [`e6e5708e`](https://github.com/router-for-me/CLIProxyAPI/commit/e6e5708e677adb2463d45accdd24bc9fd028ddd8), [`ec14dc47`](https://github.com/router-for-me/CLIProxyAPI/commit/ec14dc4703f0a5b7185c0c625032af30b94f0a0e).

Acceptance: Test restart, expiry, corruption, read-only storage, cross-credential isolation, and bounded disk growth. Demonstrate stable prefixes and run the live cache gate if the persisted choice changes prompts.

<a id="c8"></a>

### C8: Claude OAuth state and scope separation

Tracking issue: [#76](https://github.com/zakstak/cliproxy-rs/issues/76).

Core. Feasibility high, risk medium, size S. Dependencies: H1.

Use 256-bit base64url authorization state. Separate Console authorization scopes from refresh scopes. Rust currently shares one SCOPE constant between authorization and refresh.

Evidence: Source gap; provider behavior still needs the listed witness.

Current Rust: [`crates/cpa-exec/src/oauth.rs`](../crates/cpa-exec/src/oauth.rs).

Upstream anchors: [`ea0b4712`](https://github.com/router-for-me/CLIProxyAPI/commit/ea0b471291a992c5d80358917dabc6cb9f0f25f6).

Acceptance: Use a fake authorization and token server. Assert exact query and refresh form fields, state entropy and format, callback mismatch rejection, and no secret logging. Validate the scope split against first-party OAuth behavior before merge.

<a id="c9"></a>

### C9: Invalid Claude tool-property workaround

Tracking issue: [#77](https://github.com/zakstak/cliproxy-rs/issues/77).

Defer. Feasibility high, risk high, size M. Dependencies: H1.

Go replaces a rejected strict schema with an open object and embeds the original schema in the description, including tool_addition. That weakens validation. Do not enable it by default. Consider an explicit opt-in only after confirming a retained client needs it.

Evidence: Upstream issue #7389 supplies a rejection scenario. No local or first-party proof of a required workaround was obtained.

Current Rust: [`crates/cpa-exec/src/claude.rs`](../crates/cpa-exec/src/claude.rs).

Upstream anchors: [`f0c9c2d0`](https://github.com/router-for-me/CLIProxyAPI/commit/f0c9c2d01898127b5f33b2baebb85d9d9feca018).

Acceptance: Reproduce rejected names such as not[labels], check tool_addition, and show the exact loss of schema enforcement. Preserve the original request when the workaround is off.

<a id="c10"></a>

### C10: New Claude thinking and forced-tool rules

Tracking issue: [#78](https://github.com/zakstak/cliproxy-rs/issues/78).

Conditional. Feasibility medium, risk high, size M. Dependencies: H1.

Investigate thinking.display=updates and forced-tool restrictions for newer Claude models. Preserve explicit caller choices. Do not synthesize new combinations from Go alone.

Evidence: Insufficient first-party evidence for the new display value. Research-only until that evidence exists.

Current Rust: [`crates/cpa-exec/src/claude.rs`](../crates/cpa-exec/src/claude.rs).

Upstream anchors: [`13fdcc5e`](https://github.com/router-for-me/CLIProxyAPI/commit/13fdcc5e09be9f557d49d5a760403ed1cea153fc), [`1d45a5f3`](https://github.com/router-for-me/CLIProxyAPI/commit/1d45a5f3329025414ee430341daf2686cf0c029a).

Acceptance: Obtain first-party supported combinations, then cover forced tools with enabled, adaptive, disabled, summarized, and omitted thinking. The downloaded official adaptive-thinking type lists summarized and omitted, not updates.

<a id="t1"></a>

### T1: Deferred tools, ToolSearch, and nested custom tools

Tracking issue: [#79](https://github.com/zakstak/cliproxy-rs/issues/79).

Conditional. Feasibility medium, risk high, size L. Dependencies: H1, C4.

Trace tool_addition, deferred loading, nested custom tools, readable MCP names, and implicit open-object arguments through the retained routes. Extend supported bridges only where a client needs them. Shared Responses converters still serve xAI and Meta. Rust currently explicitly refuses deferred Claude tools on the strict Codex translation path.

Evidence: Some bridge behavior is missing by deliberate refusal. This is not a wholesale translation port.

Current Rust: [`crates/cpa-translate/src/codex_claude.rs`](../crates/cpa-translate/src/codex_claude.rs), [`crates/cpa-exec/src/meta.rs`](../crates/cpa-exec/src/meta.rs), [`crates/cpa-exec/src/xai.rs`](../crates/cpa-exec/src/xai.rs).

Upstream anchors: [`12d97cfe`](https://github.com/router-for-me/CLIProxyAPI/commit/12d97cfe196be266b45f4f7e0cc394c9fdc2e099), [`374056f2`](https://github.com/router-for-me/CLIProxyAPI/commit/374056f28bfaa6b7582a82cba9f8ca84868f642e), [`2443e883`](https://github.com/router-for-me/CLIProxyAPI/commit/2443e8830c401e6054370c109388bb3b59daf21c), [`8c9a7851`](https://github.com/router-for-me/CLIProxyAPI/commit/8c9a78518e39f0ca527e188f9efea0193f436a31), [`08437a67`](https://github.com/router-for-me/CLIProxyAPI/commit/08437a6742a548b001a42569be2de6d150a2e94c), [`e1fb2521`](https://github.com/router-for-me/CLIProxyAPI/commit/e1fb25219c09cdcb6b6f90d3ec612ddf670d15fc).

Acceptance: Assert stable tool prefixes during loading, namespace identity, parallel call correlation, replay, and open-object arguments. A Codex change needs a first-party fixture and a contract amendment, with unsupported cases still refused. Do not relax the contract based on a Go golden.

<a id="t2"></a>

### T2: Reachable role, reasoning, and parallel-tool translation fixes

Tracking issue: [#80](https://github.com/zakstak/cliproxy-rs/issues/80).

Core. Feasibility high, risk medium, size M. Dependencies: H1.

Add regression witnesses for omitted-index parallel calls, thinking content-part arrays, whitespace reasoning, standalone effort, developer roles, reasoning token counts, explicit parallel_tool_calls=false, and explicit output budgets. Fix reachable gaps. Keep native Codex bytes within the permitted transformations and route each translated Codex change through the contract.

Evidence: Source gap; provider behavior still needs the listed witness.

Current Rust: [`crates/cpa-translate/src/claude_chat_response.rs`](../crates/cpa-translate/src/claude_chat_response.rs), [`crates/cpa-translate/src/claude_responses_response.rs`](../crates/cpa-translate/src/claude_responses_response.rs), [`crates/cpa-exec/src/meta.rs`](../crates/cpa-exec/src/meta.rs), [`crates/cpa-exec/src/xai.rs`](../crates/cpa-exec/src/xai.rs).

Upstream anchors: [`c449529e`](https://github.com/router-for-me/CLIProxyAPI/commit/c449529ecd17e87c2b428e62a1f836a837a89cd0), [`6044c5d1`](https://github.com/router-for-me/CLIProxyAPI/commit/6044c5d14270fa094333b25932ac11ab13282821), [`69637a66`](https://github.com/router-for-me/CLIProxyAPI/commit/69637a662a05aebc64805760be960284b2459cb2), [`e6931d51`](https://github.com/router-for-me/CLIProxyAPI/commit/e6931d51e328343088d5902e5cbd06edbe4d8633), [`00b76771`](https://github.com/router-for-me/CLIProxyAPI/commit/00b7677102fa6e3b1a2c2fa90c13458beaffd1b0), [`43f5386b`](https://github.com/router-for-me/CLIProxyAPI/commit/43f5386bc3b904caa6106c879dd5dbc2d1e86ceb), [`7db0935a`](https://github.com/router-for-me/CLIProxyAPI/commit/7db0935a633926e09a728f06834fe9ca55149f7b), [`009f8d7a`](https://github.com/router-for-me/CLIProxyAPI/commit/009f8d7ae35f87937058e1d2989f400ee2e78114), [`99d1698f`](https://github.com/router-for-me/CLIProxyAPI/commit/99d1698f7c59bd8bd694974486e45eaca7bf1879), [`01b24f1f`](https://github.com/router-for-me/CLIProxyAPI/commit/01b24f1f95dc9b309765212c52b25b25e005ff01).

Acceptance: Cover streaming and buffered tool calls, multiple reasoning parts, developer messages mid-conversation, false versus absent flags, and explicit budgets. Run the same shared converters through xAI and Meta adapters. Leave removed-provider-only converters untouched.

<a id="m1"></a>

### M1: Muse identity and contributor-mode headers

Tracking issue: [#81](https://github.com/zakstak/cliproxy-rs/issues/81).

Core. Feasibility medium, risk medium, size M. Dependencies: H1.

Update the old muse-build/1.3.0 identity to an evidenced supported Muse Code profile. Add the required X-Fb and X-Tbh request, session, and thread headers with the right UUID lifecycle. Set contributor mode only from the effective contributor model after model rules.

Evidence: Rust old identity is confirmed. Upstream claims measured headers; no independent official Muse wire source was verified.

Current Rust: [`crates/cpa-exec/src/meta.rs`](../crates/cpa-exec/src/meta.rs).

Upstream anchors: [`5d8fddd3`](https://github.com/router-for-me/CLIProxyAPI/commit/5d8fddd338df8926b2c4903e931105f4693b2a11), [`d9a688e6`](https://github.com/router-for-me/CLIProxyAPI/commit/d9a688e6155720897433abb0972949b30235849f), [`c2f402b4`](https://github.com/router-for-me/CLIProxyAPI/commit/c2f402b45643f0740d70b9010fd7d8d0aab7ef65).

Acceptance: Capture prepared headers with a mock. Assert per-request versus per-session UUID lifetimes, model alias rewrites, contributor versus ordinary models, and no header leakage to other providers. Confirm provenance for the claimed Muse Code 1.4.14 profile.

<a id="m2"></a>

### M2: Muse explicit prompt-cache retention

Tracking issue: [#82](https://github.com/zakstak/cliproxy-rs/issues/82).

Core. Feasibility high, risk low, size S. Dependencies: H1.

Preserve an explicit nonempty string prompt_cache_retention without inventing a default. Rust prepare currently deletes the field.

Evidence: Source gap; provider behavior still needs the listed witness.

Current Rust: [`crates/cpa-exec/src/meta.rs`](../crates/cpa-exec/src/meta.rs).

Upstream anchors: [`f77a3c10`](https://github.com/router-for-me/CLIProxyAPI/commit/f77a3c104bc00474f6b366bf1582440deb42fbea).

Acceptance: Check absent, valid string, empty string, and non-string values at the prepared-body boundary. Assert unrelated body fields remain byte-stable unless an existing rule already rewrites them.

<a id="m3"></a>

### M3: Muse reminder tool namespace

Tracking issue: [#83](https://github.com/zakstak/cliproxy-rs/issues/83).

Conditional. Feasibility medium, risk high, size M. Dependencies: H1.

Go injects a Muse reminder decision tool into tool-bearing requests before final operator rules. Treat this as a semantic feature with its own approval, separate from identity headers. Require verified client evidence and a clear policy for caller tools.

Evidence: Go cites a Muse 1.4.12 capture. No independently checked official source supports mandatory tool injection.

Current Rust: [`crates/cpa-exec/src/meta.rs`](../crates/cpa-exec/src/meta.rs).

Upstream anchors: [`bb6e1aa8`](https://github.com/router-for-me/CLIProxyAPI/commit/bb6e1aa8c0e8518667519a595e06c956fde287e2), [`b147b8d2`](https://github.com/router-for-me/CLIProxyAPI/commit/b147b8d2105352a3e90ef82faf448135908ad5dd).

Acceptance: Cover no tools, existing namespace, collisions, idempotence, and operator-rule override order. Show the exact tool addition in a review fixture.

<a id="x1"></a>

### X1: Grok buffered SSE terminal errors

Tracking issue: [#84](https://github.com/zakstak/cliproxy-rs/issues/84).

Core. Feasibility high, risk medium, size M. Dependencies: H1.

Replace line-by-line buffered scanning with bounded SSE frame handling. Recognize response.failed, error events, raw JSON errors, empty streams, and unterminated streams. Keep provider request failures separate from credential health. Retain apply_patch activity detection and usage.

Evidence: Concrete source gap at xai.rs buffered around lines 890-937. Not yet reproduced by executing the proxy.

Current Rust: [`crates/cpa-exec/src/xai.rs`](../crates/cpa-exec/src/xai.rs).

Upstream anchors: [`949b43b3`](https://github.com/router-for-me/CLIProxyAPI/commit/949b43b330f919fddeeb45e27b90cc9413736ef7).

Acceptance: Use loopback responses with multiline data, split frames, CRLF, empty 200, raw JSON error, failed terminal event, incomplete terminal event, missing terminal event, and tool activity. Assert the intended request-scoped 502 versus genuine timeout 408. Current Rust handles completed and incomplete only, then returns 408.

<a id="x2"></a>

### X2: Collision-safe Grok login filenames

Tracking issue: [#85](https://github.com/zakstak/cliproxy-rs/issues/85).

Core. Feasibility high, risk medium, size S. Dependencies: H1.

Append a stable hash of the original identity to sanitized credential filenames. Migrate without overwriting another account and preserve existing proxy, disabled, and operator settings.

Evidence: Rust naming around xai_auth.rs line 670 uses sanitizer-only names, so distinct identities can collide.

Current Rust: [`crates/cpa-exec/src/xai_auth.rs`](../crates/cpa-exec/src/xai_auth.rs).

Upstream anchors: [`d65840fb`](https://github.com/router-for-me/CLIProxyAPI/commit/d65840fb5c8644fdb4e650706d1f4c5b685d348f).

Acceptance: Test two identities with the same sanitized name, repeat login, old filename migration, interrupted save, and settings preservation. Use a private auth directory.

<a id="x3"></a>

### X3: Grok tool deduplication and image routing rules

Tracking issue: [#86](https://github.com/zakstak/cliproxy-rs/issues/86).

Core. Feasibility high, risk medium, size M. Dependencies: H1.

Deduplicate promoted historical additional_tools with namespace-aware first-wins rules. Apply configured image payload defaults and allow xAI vendor aliases under force-model-prefix. Keep the generic OpenAI-compatible upstream removed.

Evidence: Source gap; provider behavior still needs the listed witness.

Current Rust: [`crates/cpa-exec/src/xai.rs`](../crates/cpa-exec/src/xai.rs), [`crates/cpa-server/src/images.rs`](../crates/cpa-server/src/images.rs).

Upstream anchors: [`0c61b12c`](https://github.com/router-for-me/CLIProxyAPI/commit/0c61b12cf2262e8369a2bfa72e81f0edfcd85fc4), [`826bb167`](https://github.com/router-for-me/CLIProxyAPI/commit/826bb167802f147c40c1f2eba6c36b6aafbf8601), [`8a3e7622`](https://github.com/router-for-me/CLIProxyAPI/commit/8a3e7622df9dcde5b6f00a1cf187b9738408cdb7).

Acceptance: Exercise historical and current tools with duplicate names across namespaces, explicit image parameters versus defaults, aliases, and forced prefixes. Assert only xAI routes gain this behavior.

<a id="x4"></a>

### X4: Grok Imagine Video 1.5 Lite

Tracking issue: [#87](https://github.com/zakstak/cliproxy-rs/issues/87).

Core. Feasibility high, risk medium, size M. Dependencies: H1.

Add the Lite model and model-specific resolution handling. Official xAI docs describe Lite 1080p through 720p upscale, 1.5 native 1080p, and original generation capped at 720p. Avoid a blanket 1080p default or validation rule.

Evidence: Official xAI video generation documentation confirms Lite and the resolution distinction.

Current Rust: [`crates/cpa-server/src/videos.rs`](../crates/cpa-server/src/videos.rs).

Upstream anchors: [`f0ddc38f`](https://github.com/router-for-me/CLIProxyAPI/commit/f0ddc38f2921a653830b704c288b49651e3197d2), [`c69cdd93`](https://github.com/router-for-me/CLIProxyAPI/commit/c69cdd933d1928561f3c1e437e1443f6b6573b6f).

Acceptance: Table-test each model and alias, absent and explicit resolutions, upscale fields, image-to-video inputs, catalog fallback, and invalid combinations. Mock generation and poll endpoints.

<a id="x5"></a>

### X5: Grok caller User-Agent policy

Tracking issue: [#88](https://github.com/zakstak/cliproxy-rs/issues/88).

Follow-up. Feasibility high, risk low, size S. Dependencies: H1.

Preserve an allowed downstream User-Agent or set a documented xAI default. Keep provider-specific client identity rules local to xAI.

Evidence: Source gap; provider behavior still needs the listed witness.

Current Rust: [`crates/cpa-exec/src/xai.rs`](../crates/cpa-exec/src/xai.rs).

Upstream anchors: [`5026f365`](https://github.com/router-for-me/CLIProxyAPI/commit/5026f36531c098d5fd69d8a5f1fb27795aa5fb69).

Acceptance: Check absent and explicit values plus rejected credential-bearing headers. Verify no Codex or Claude identity changes.

<a id="a1"></a>

### A1: Keep credentials during transient watcher read failures

Tracking issue: [#89](https://github.com/zakstak/cliproxy-rs/issues/89).

Core. Feasibility high, risk medium, size M. Dependencies: H1.

Distinguish unreadable directory or file from deletion, invalid contents, and explicit revocation. Retain the last good credential during transient I/O failure. Preserve runtime cooldown and snapshot binding, and restore a successfully re-logged credential without reviving an obsolete account.

Evidence: watching.rs treats unreadable directories as empty; credential scan returns empty on directory errors. Runtime auth-dir policy must be preserved.

Current Rust: [`crates/cpa-server/src/watching.rs`](../crates/cpa-server/src/watching.rs), [`crates/cpa-core/src/config/credentials.rs`](../crates/cpa-core/src/config/credentials.rs).

Upstream anchors: [`57a12996`](https://github.com/router-for-me/CLIProxyAPI/commit/57a129964fb5ca8004e6dec9700fa89fe0710bdc), [`5fe203d5`](https://github.com/router-for-me/CLIProxyAPI/commit/5fe203d5f3c481bf57534b67b91a555f46118d2b), [`32dd2c86`](https://github.com/router-for-me/CLIProxyAPI/commit/32dd2c86b557b4a02dbe8b19baeab39b4ab0f8b6), [`d5816d5e`](https://github.com/router-for-me/CLIProxyAPI/commit/d5816d5ef694883944b8231c3310047eca23fa03), [`21553509`](https://github.com/router-for-me/CLIProxyAPI/commit/21553509662eba4b8e889d797822c1891c980b5d), [`43068c86`](https://github.com/router-for-me/CLIProxyAPI/commit/43068c8642f3a0ab1ba66056d298006d0829d657).

Acceptance: Test unreadable directory, unreadable file, actual deletion, invalid JSON, permission recovery, stale snapshot, relative auth-dir, and refresh races. Deliberately replace the current unreadable-file-drops test policy. Use the sandbox helper that preserves the caller UID so permissions tests are meaningful.

<a id="a2"></a>

### A2: Targeted OAuth re-login and login proxy

Tracking issue: [#90](https://github.com/zakstak/cliproxy-rs/issues/90).

Follow-up. Feasibility high, risk medium, size L. Dependencies: H1, A1.

Add a session-scoped login proxy with per-account persistence, CLI login-proxy, targeted re-login, and provider metadata for the four retained built-ins. Keep auth and config snapshots coherent. Do not recreate plugin OAuth registration or a generic provider layer.

Evidence: Source gap; provider behavior still needs the listed witness.

Current Rust: [`crates/cpa-server/src/management/oauth.rs`](../crates/cpa-server/src/management/oauth.rs), [`crates/cpa-exec/src/oauth.rs`](../crates/cpa-exec/src/oauth.rs), [`crates/cliproxy/src/main.rs`](../crates/cliproxy/src/main.rs).

Upstream anchors: [`27f3abfb`](https://github.com/router-for-me/CLIProxyAPI/commit/27f3abfb70a6d05d4f11b49e5d0bc70c05e1ef14), [`bebd92c4`](https://github.com/router-for-me/CLIProxyAPI/commit/bebd92c4c878a19bce9df842c42b90adedc37ff7), [`d3899771`](https://github.com/router-for-me/CLIProxyAPI/commit/d38997717dd38a328bca0b2b0ce3fe8e41a84c8d), [`482efbf5`](https://github.com/router-for-me/CLIProxyAPI/commit/482efbf5a777771523c1237ed7aaee965451c51d), [`0559fa3c`](https://github.com/router-for-me/CLIProxyAPI/commit/0559fa3cfd5722cf1755a7dc3a90573ede3835f6), [`8bd17955`](https://github.com/router-for-me/CLIProxyAPI/commit/8bd1795599d1420d7cb5e0ecedbac41eb6a4cfb9).

Acceptance: Use mock auth flows. Test two simultaneous sessions with different proxies, target matching, missing target, metadata preservation, callback cancellation, and proxy failures. Assert removed provider logins still fail with migration errors.

<a id="a3"></a>

### A3: SIWC bind fencing and read-only status

Tracking issue: [#91](https://github.com/zakstak/cliproxy-rs/issues/91).

Core. Feasibility high, risk medium, size M. Dependencies: H1, A1.

Fence asynchronous bind commits against account replacement, serialize legacy uploads with bind commits, resolve relative paths consistently, and preserve the issued SIWC client during reauthorization. Add read-only status route aliases. Existing file locks do not alone establish account-epoch fencing.

Evidence: Source gap; provider behavior still needs the listed witness.

Current Rust: [`crates/cpa-server/src/management/siwc.rs`](../crates/cpa-server/src/management/siwc.rs), [`crates/cpa-exec/src/codex_siwc.rs`](../crates/cpa-exec/src/codex_siwc.rs).

Upstream anchors: [`6bbd2cb7`](https://github.com/router-for-me/CLIProxyAPI/commit/6bbd2cb785d14281d103b75abb4f81124a16532d), [`88041f0c`](https://github.com/router-for-me/CLIProxyAPI/commit/88041f0cfbdd4ee1fb8d1639ef90ddd44e79f38f), [`bb2d8f82`](https://github.com/router-for-me/CLIProxyAPI/commit/bb2d8f82bcce132e0d2a04d521e84d1ceb9ba21f), [`06fd5b8b`](https://github.com/router-for-me/CLIProxyAPI/commit/06fd5b8bc2807af5fceb6f39d2c5808c52e76034), [`3891ebaa`](https://github.com/router-for-me/CLIProxyAPI/commit/3891ebaab5f1e8b27507ce25dbf379166739d51e).

Acceptance: Use deterministic barriers for delete or replace during bind, concurrent upload, stale callback, relative paths, and canceled reauthorization. Assert status responses omit tokens and mutation. Do not infer new Responses behavior for the SIWC endpoint profile.

<a id="a4"></a>

### A4: Codex renewal and quarantine correctness

Tracking issue: [#92](https://github.com/zakstak/cliproxy-rs/issues/92).

Core. Feasibility medium, risk high, size L. Dependencies: H1, A1, A3.

Honor source-backed renewal windows and persist every SIWC replacement refresh token. The official SIWC token reference requires a replacement on each successful refresh; retain that validation. Parse documented expiry forms at the boundary, retain terminal sign-in quarantine through manual refresh, and stop refresh loops without usable renewal credentials. Keep native Codex usable when a separate SIWC partition is recovering. Preserve independent partition failures. Audit the exact documented refresh form and response fields, including earliest_refresh_at. Its format and scheduling semantics still need a source before enforcement.

Evidence: Rust already has locks, partition merge, disk adoption, and required SIWC replacement-token validation. The current official SIWC token and account/session documentation now confirms rotation and the refresh request form. Port only demonstrated state-transition gaps. Do not loosen required rotation based on Go.

Current Rust: [`crates/cpa-exec/src/codex_refresh.rs`](../crates/cpa-exec/src/codex_refresh.rs), [`crates/cpa-exec/src/codex_siwc.rs`](../crates/cpa-exec/src/codex_siwc.rs), [`crates/cpa-server/src/refresh.rs`](../crates/cpa-server/src/refresh.rs).

Upstream anchors: [`514d09f7`](https://github.com/router-for-me/CLIProxyAPI/commit/514d09f746ff95dc4063440a3c31ff9277a122f7), [`a96e640d`](https://github.com/router-for-me/CLIProxyAPI/commit/a96e640d886a38f289f82a8e53590cc3b5e36ffa), [`95f125ff`](https://github.com/router-for-me/CLIProxyAPI/commit/95f125ffd04c2ab086ca3f8137c0a9ab185a3237), [`7b4c64b0`](https://github.com/router-for-me/CLIProxyAPI/commit/7b4c64b006875fdbbf3bd2e77fa558573ecbdaf3), [`e5827065`](https://github.com/router-for-me/CLIProxyAPI/commit/e582706557b22a4deeaa640f7d465432f7d91a0c), [`80ee739f`](https://github.com/router-for-me/CLIProxyAPI/commit/80ee739f33c403cef25c5489e2734fab343723e9), [`a46be7ae`](https://github.com/router-for-me/CLIProxyAPI/commit/a46be7ae3bc13225abbb9e217e9ae1674d54c0e4), [`09b3a579`](https://github.com/router-for-me/CLIProxyAPI/commit/09b3a57981652fa80ffe2a349a88e18e45fea56e), [`5f38d557`](https://github.com/router-for-me/CLIProxyAPI/commit/5f38d5572c432c67ec260be55f73b0ec72ac0764), [`baec297f`](https://github.com/router-for-me/CLIProxyAPI/commit/baec297fec2943271058ba13a60d0d55f8955092).

Acceptance: Use fake token endpoints and a controlled clock. Cover terminal versus transient failure, disabled credentials, missing refresh credentials, replacement token persistence, a negative missing-replacement SIWC response, documented expiry variants, separate native and SIWC partitions, disk adoption, cancellation, and backoff. Verify the exact issued client ID, resource, and omitted scope on the SIWC refresh form. Record any earliest_refresh_at format or scheduling requirement as unknown until source-backed.

<a id="a5"></a>

### A5: Durable refresh claims and result receipts

Tracking issue: [#93](https://github.com/zakstak/cliproxy-rs/issues/93).

Defer. Feasibility medium, risk high, size XL. Dependencies: H1, A3, A4.

Prevent token reuse after an uncertain refresh send or crash using durable claims and successful result receipts in the authoritative credential store. Go pending claims never expire, and receipts contain credentials. Define operator recovery, account replacement, and protected storage for every supported backend. A process lock or a local-only journal does not provide the cross-store guarantee.

Evidence: Large new persistence protocol. Rust lacks a durable refresh journal. No renewal request was sent during research.

Current Rust: [`crates/cpa-exec/src/codex_refresh.rs`](../crates/cpa-exec/src/codex_refresh.rs), [`crates/cpa-store/src/lib.rs`](../crates/cpa-store/src/lib.rs).

Upstream anchors: [`68cd7572`](https://github.com/router-for-me/CLIProxyAPI/commit/68cd75724e4de4e0450475cc99c7491166cf3392).

Acceptance: Crash before send, after send, after receipt, and before auth save. Run two workers against each supported file, Git, object, and PostgreSQL store. Assert no duplicate refresh and correct adoption. Test store outage, secret permissions, and quarantine recovery. Decide explicit unsupported-store behavior before implementation.

<a id="r1"></a>

### R1: Request-fault classification and safe quota evidence

Tracking issue: [#94](https://github.com/zakstak/cliproxy-rs/issues/94).

Core. Feasibility high, risk medium, size M. Dependencies: H1, C4.

Keep unsupported capability, count_tokens 501, request-scoped 401, truncated streams, and policy errors availability-neutral where appropriate. Treat Anthropic credit balance errors as payment failures. Preserve classified terminal status and usage fail.code. On local Claude cooldown 429, expose retained quota evidence and retry time without misclassifying per-minute or Cloudflare errors.

Evidence: Rust has portions of this machinery. Each proposed change starts with a failing witness to avoid duplicate implementation.

Current Rust: [`crates/cpa-server/src/scheduler.rs`](../crates/cpa-server/src/scheduler.rs), [`crates/cpa-server/src/usage_record.rs`](../crates/cpa-server/src/usage_record.rs), [`crates/cpa-exec/src/claude.rs`](../crates/cpa-exec/src/claude.rs).

Upstream anchors: [`d00267c3`](https://github.com/router-for-me/CLIProxyAPI/commit/d00267c3fe243516a4c338a2b2e65581d0296e2a), [`1c537211`](https://github.com/router-for-me/CLIProxyAPI/commit/1c5372116735f7f5d88980bba651cbf175973d2b), [`4138edb1`](https://github.com/router-for-me/CLIProxyAPI/commit/4138edb1be52f93141f6caae0e52579764e78ded), [`18883a57`](https://github.com/router-for-me/CLIProxyAPI/commit/18883a57b4b1b040bff93a41be0e7f7ad261e501), [`cb70eacf`](https://github.com/router-for-me/CLIProxyAPI/commit/cb70eacfa533d44efb1eec5007098321e396fc3c), [`e458999b`](https://github.com/router-for-me/CLIProxyAPI/commit/e458999bb3dafb0ec81cea5b95b3b98d87da72d7), [`1d4e0549`](https://github.com/router-for-me/CLIProxyAPI/commit/1d4e054978160a5a702021d422c363c24a95aa51), [`f013eb66`](https://github.com/router-for-me/CLIProxyAPI/commit/f013eb662aa27a29f8163ff29c9e3475b359c264).

Acceptance: Observe credential state after each fault and assert refresh was not scheduled. Check usage and downstream status agree. Verify local quota headers contain only allowed provider quota data, remain model-scoped where appropriate, and survive restart as intended.

<a id="r2"></a>

### R2: Canonical model cooldowns and forced-cooldown persistence

Tracking issue: [#95](https://github.com/zakstak/cliproxy-rs/issues/95).

Core. Feasibility high, risk medium, size M. Dependencies: H1, R1.

Canonicalize equivalent model spellings and migrate old cooldown keys. Preserve explicit forced cooldown independently of disable-cooling. Rust records forced cooldown initially, but clear_disabled removes it and persisted state lacks an explicit force distinction. Separate this fix from weighted-pool changes in the same Go commit.

Evidence: Source gap; provider behavior still needs the listed witness.

Current Rust: [`crates/cpa-server/src/scheduler.rs`](../crates/cpa-server/src/scheduler.rs).

Upstream anchors: [`098af7d5`](https://github.com/router-for-me/CLIProxyAPI/commit/098af7d54309c488d9e1a7c5a1c807302df8f131), [`9d7d012b`](https://github.com/router-for-me/CLIProxyAPI/commit/9d7d012b35f082ddc037c2b90fd3219e32d940ab).

Acceptance: Test aliases, Unicode case behavior with GoStr where applicable, restart migration, forced versus ordinary cooldown, disable-cooling, sticky sessions, and model-scoped exhaustion. Preserve existing soonest-reset routing.

<a id="r3"></a>

### R3: Weighted routing fairness with sticky traffic

Tracking issue: [#96](https://github.com/zakstak/cliproxy-rs/issues/96).

Defer. Feasibility high, risk high, size L. Dependencies: H1, R2.

Account for pinned traffic and large dynamic pools in weighted routing only if this policy is wanted. Keep the fork default soonest-reset and explicit session pins. Use a Rust state model rather than Go pool-string or digest internals.

Evidence: Source gap; provider behavior still needs the listed witness.

Current Rust: [`crates/cpa-server/src/scheduler.rs`](../crates/cpa-server/src/scheduler.rs).

Upstream anchors: [`d2045150`](https://github.com/router-for-me/CLIProxyAPI/commit/d20451504f473d303a877a8f4edef195f988da4e), [`e84d4836`](https://github.com/router-for-me/CLIProxyAPI/commit/e84d4836b7b67c0e010b73778bc49dabaadaa664), [`e479ab12`](https://github.com/router-for-me/CLIProxyAPI/commit/e479ab12538cb027ea558bbfa070fef1ca9d187c), [`9d7d012b`](https://github.com/router-for-me/CLIProxyAPI/commit/9d7d012b35f082ddc037c2b90fd3219e32d940ab).

Acceptance: Use deterministic traces for weighted and sticky mixes, joins and departures, large pools, cooldowns, and restarts. Define acceptable distribution and starvation bounds before code changes.

<a id="r4"></a>

### R4: Session-affinity never-rebind policy

Tracking issue: [#97](https://github.com/zakstak/cliproxy-rs/issues/97).

Follow-up. Feasibility high, risk high, size M. Dependencies: H1, R2.

Offer an explicit never-rebind policy with clear failure behavior when a pinned credential is unavailable. Preserve the current default and do not silently switch sessions.

Evidence: Source gap; provider behavior still needs the listed witness.

Current Rust: [`crates/cpa-server/src/session.rs`](../crates/cpa-server/src/session.rs), [`crates/cpa-server/src/scheduler.rs`](../crates/cpa-server/src/scheduler.rs).

Upstream anchors: [`40c16182`](https://github.com/router-for-me/CLIProxyAPI/commit/40c16182b82689c4b7f6038d5992861fec1e1e38).

Acceptance: Test pin expiry, deleted account, exhausted quota, explicit retry, model changes, and isolation between clients. Confirm the response describes an unavailable pinned account without leaking identity.

<a id="r5"></a>

### R5: On-demand model discovery refresh

Tracking issue: [#98](https://github.com/zakstak/cliproxy-rs/issues/98).

Follow-up. Feasibility high, risk medium, size M. Dependencies: H1, K1, A1.

Add management refresh endpoints on top of the existing fenced background updater. Coalesce requests and reject stale results after credential replacement. Bounded-cooldown model relisting appears equivalent already because listings consult current suspension deadlines, so add a regression rather than new registry machinery.

Evidence: Refresh endpoints are absent. Bounded relisting looks already covered by current runtime behavior and needs execution proof.

Current Rust: [`crates/cpa-server/src/model_updater.rs`](../crates/cpa-server/src/model_updater.rs), [`crates/cpa-server/src/models.rs`](../crates/cpa-server/src/models.rs).

Upstream anchors: [`10417e5b`](https://github.com/router-for-me/CLIProxyAPI/commit/10417e5b6b85461e8d7072a6fea6ad9f781b1f0f), [`69ed202a`](https://github.com/router-for-me/CLIProxyAPI/commit/69ed202a1ac3b36147b696d48bc0ba37fe0b4582).

Acceptance: Test all retained provider refresh paths, concurrent calls, timeout, auth replacement, configured local-model behavior, and bounded cooldown expiry without restart.

<a id="u1"></a>

### U1: Structured retained-provider usage and timing

Tracking issue: [#99](https://github.com/zakstak/cliproxy-rs/issues/99).

Follow-up. Feasibility high, risk medium, size L. Dependencies: H1, C4, X1.

Add optional source-backed tool counters, iterations, text, image and audio token breakdowns, media parameters, service tier, and cost fields for retained providers. Preserve missing versus zero. Deduplicate output_item.done and multiline SSE observations, retain failure usage, and measure TTFT at effective token frames. Keep Claude cache telemetry opt-in and avoid removed-provider structs.

Evidence: Rust currently records scalar and cache token fields plus tier. Go usage is a broader additive feature, not a drop-in struct copy.

Current Rust: [`crates/cpa-server/src/usage_record.rs`](../crates/cpa-server/src/usage_record.rs), [`crates/cpa-server/src/usage.rs`](../crates/cpa-server/src/usage.rs).

Upstream anchors: [`89e472f3`](https://github.com/router-for-me/CLIProxyAPI/commit/89e472f3251e8600576323dc3f97539c79949630), [`01551f5f`](https://github.com/router-for-me/CLIProxyAPI/commit/01551f5f6b260030b35a0fa14f53dcb14a32f3c8), [`24840fdd`](https://github.com/router-for-me/CLIProxyAPI/commit/24840fddf8432bf171d68356217e17b5848a1159), [`6d7e6a3b`](https://github.com/router-for-me/CLIProxyAPI/commit/6d7e6a3bb9da6f53bac316cea17e8061002cee7d), [`82deffb9`](https://github.com/router-for-me/CLIProxyAPI/commit/82deffb9ca0d5c6b81f47d68e9b56ead09d725f6), [`97dd3a4a`](https://github.com/router-for-me/CLIProxyAPI/commit/97dd3a4a2f0c0c017afe80e3358c431147e2e677), [`f434192a`](https://github.com/router-for-me/CLIProxyAPI/commit/f434192a689b76c93a03cd90f9c8e023da0707a9), [`9d463e39`](https://github.com/router-for-me/CLIProxyAPI/commit/9d463e39f007454f7e499e489d7bf4efc062ea79), [`07b5a7a1`](https://github.com/router-for-me/CLIProxyAPI/commit/07b5a7a1ca267c5d9b743e38dff4ed8ff1da0c91), [`b90190db`](https://github.com/router-for-me/CLIProxyAPI/commit/b90190dbaa013573d40f1a0654404dd6deb44a3c), [`fb30a9e8`](https://github.com/router-for-me/CLIProxyAPI/commit/fb30a9e8bb4ac06c2748da667515ad81920aca4f), [`e198f3cd`](https://github.com/router-for-me/CLIProxyAPI/commit/e198f3cd4c52cb15af590f2f9b105f74d27d1443), [`8289ec4c`](https://github.com/router-for-me/CLIProxyAPI/commit/8289ec4c71ad914d0612c2776352b76ba3d355a6), [`f2e255f7`](https://github.com/router-for-me/CLIProxyAPI/commit/f2e255f73eda25d8106097fbf2eeb5aa00f8ae29), [`d5eb4ca1`](https://github.com/router-for-me/CLIProxyAPI/commit/d5eb4ca1d5181f3498bf724035b51a485f924faa).

Acceptance: Use provider-labelled buffered and streaming fixtures with partial usage, explicit zero searches, repeated terminal events, failed streams, tool-only turns, and payload-rule tier changes. Verify old exports and collectors still decode additive fields. Any production collector proof follows a separately approved deployment.

<a id="l1"></a>

### L1: Bounded and confidential error logging

Tracking issue: [#100](https://github.com/zakstak/cliproxy-rs/issues/100).

Core. Feasibility high, risk medium, size M. Dependencies: H1.

Add witnesses for bounded upstream error captures, no successful-stream whole-body retention, WebSocket disconnect finalization, unrouted 404 behavior, path-safe filenames, and panic cleanup. Use only the shared sink and preserve existing confidential-delivery behavior. Implement only demonstrated gaps.

Evidence: Current sink already has redaction, spooling, and confidential guards. Source triage does not establish every Go logging issue as a Rust defect.

Current Rust: [`crates/cpa-server/src/request_logging.rs`](../crates/cpa-server/src/request_logging.rs).

Upstream anchors: [`ece3f52a`](https://github.com/router-for-me/CLIProxyAPI/commit/ece3f52a4146f2bdf49e28d2d65572668a3f897e), [`69a032de`](https://github.com/router-for-me/CLIProxyAPI/commit/69a032dea3688fbab0d67686d1631ac9d2ea95f3), [`4cb98e3c`](https://github.com/router-for-me/CLIProxyAPI/commit/4cb98e3ccdc3852fb27f705358699a39a57d7a0a), [`4b3f2c7e`](https://github.com/router-for-me/CLIProxyAPI/commit/4b3f2c7ec7cd77b3b53b39583edb3a30b4a0d3f7), [`179d43dc`](https://github.com/router-for-me/CLIProxyAPI/commit/179d43dc744695b719bddb3e3e4be0239af04a73), [`a5795047`](https://github.com/router-for-me/CLIProxyAPI/commit/a5795047bfb4b32371a80cdf94b520f3054c436f).

Acceptance: Test Cookie, Set-Cookie, URL credentials, short keys, arbitrary request IDs, oversized errors, long-lived sockets, cancellation, and panic. Assert memory bounds and exactly one final log record. No alternate byte logging path.

<a id="i1"></a>

### I1: Bound zstd decoder window for captured requests

Tracking issue: [#101](https://github.com/zakstak/cliproxy-rs/issues/101).

Core. Feasibility high, risk medium, size S. Dependencies: H1, L1.

Add an explicit decompressor window limit in addition to the existing output bound. Go now limits the capture window to 8 MiB and output to at most 32 MiB unless configuration is smaller. Select Rust limits compatible with the existing body policy and safe raw-byte fallback.

Evidence: Rust ZstdDecoder has no explicit window limit at the inspected call site. Confirm library default and available API before choosing the implementation.

Current Rust: [`crates/cpa-server/src/request_logging.rs`](../crates/cpa-server/src/request_logging.rs).

Upstream anchors: [`ad8bdd3f`](https://github.com/router-for-me/CLIProxyAPI/commit/ad8bdd3f20bef44f0684b8a6096a216b2c76040d).

Acceptance: Use oversized declared-window and single-segment bomb fixtures, chained encodings, configured small limits, and normal large bodies. Assert bounded allocation and that failed decoding cannot bypass redaction.

<a id="i2"></a>

### I2: Bound RESP frame counts, lines, and aggregate bytes

Tracking issue: [#102](https://github.com/zakstak/cliproxy-rs/issues/102).

Core. Feasibility high, risk medium, size M. Dependencies: H1.

Add checked array counts, bounded line reads, and an aggregate frame budget. Rust has a 64 MiB bulk cap but unbounded line and argument reads. Go uses 1,024 array entries and 64 KiB line, bulk, and total budgets. Choose limits for real retained queue payloads rather than reducing the Rust bulk ceiling without evidence.

Evidence: Source gap; provider behavior still needs the listed witness.

Current Rust: [`crates/cpa-server/src/resp.rs`](../crates/cpa-server/src/resp.rs), [`crates/cpa-server/tests/resp_protocol.rs`](../crates/cpa-server/tests/resp_protocol.rs).

Upstream anchors: [`888f2382`](https://github.com/router-for-me/CLIProxyAPI/commit/888f23824d163dc1c4e569db9a96981701945437).

Acceptance: Test negative and overflowing lengths, huge arrays, long unterminated lines, many individually valid bulks, truncated input, pipelined frames, and healthy connections after rejection. Assert bounded memory and graceful closure.

<a id="i3"></a>

### I3: Bound unauthenticated incomplete HTTP bodies

Tracking issue: [#103](https://github.com/zakstak/cliproxy-rs/issues/103).

Core. Feasibility high, risk medium, size M. Dependencies: H1.

Reproduce incomplete unauthenticated bodies on real sockets. Apply bounded header, body, and idle handling at the axum or hyper boundary as needed. The current protocol or TLS peek timeout does not prove full-body protection.

Evidence: Go issue is relevant, but the exact Rust exposure has not been runtime-confirmed. No Gin middleware should be copied.

Current Rust: [`crates/cpa-server/src/lib.rs`](../crates/cpa-server/src/lib.rs).

Upstream anchors: [`477605dc`](https://github.com/router-for-me/CLIProxyAPI/commit/477605dc6deb301e587dff646f9f43ea8b58ee60).

Acceptance: Open bounded concurrent sockets with incomplete headers and bodies, chunked input, and no client key. Measure closure and descriptor recovery with a controlled clock or short test timeout. Ensure valid slow authorized streams still work.

<a id="i4"></a>

### I4: Explicit Authorization in CORS preflights

Tracking issue: [#104](https://github.com/zakstak/cliproxy-rs/issues/104).

Core. Feasibility high, risk low, size S. Dependencies: H1.

List Authorization explicitly in Access-Control-Allow-Headers because the wildcard does not cover this header in browser preflights. Preserve the existing origin and credential policy.

Evidence: Source gap; provider behavior still needs the listed witness.

Current Rust: [`crates/cpa-server/src/management/access.rs`](../crates/cpa-server/src/management/access.rs).

Upstream anchors: [`058b803b`](https://github.com/router-for-me/CLIProxyAPI/commit/058b803b2cce45ff5fde7905dada3086aba7567d).

Acceptance: Send OPTIONS with Authorization and Content-Type. Assert the explicit allow list and ordinary cross-origin request behavior. Include a real browser check during implementation if browser compatibility remains uncertain.

<a id="i5"></a>

### I5: Keep SSE heartbeats between complete frames

Tracking issue: [#105](https://github.com/zakstak/cliproxy-rs/issues/105).

Core. Feasibility high, risk medium, size M. Dependencies: H1.

Ensure image-stream keepalive output cannot interrupt a partial SSE line or event. First test whether the existing stream wrapper can yield a partial frame when the heartbeat fires.

Evidence: Current image heartbeat wrapper needs a deterministic fragmentation witness. Upstream bug is not yet independently reproduced in Rust.

Current Rust: [`crates/cpa-server/src/respond.rs`](../crates/cpa-server/src/respond.rs), [`crates/cpa-server/src/images.rs`](../crates/cpa-server/src/images.rs).

Upstream anchors: [`91e0dee8`](https://github.com/router-for-me/CLIProxyAPI/commit/91e0dee827abf476f9f86246f68c500a112f7155).

Acceptance: Force a heartbeat between data: fragments and between multiline data fields. Parse the downstream stream and compare payload bytes. Cover cancellation and terminal errors with no extra frame after termination.

<a id="p1"></a>

### P1: Full-chain HTTP and WebSocket compression

Tracking issue: [#106](https://github.com/zakstak/cliproxy-rs/issues/106).

Defer. Feasibility high, risk high, size L. Dependencies: H1, I1, I5.

Upstream WebSocket deflate already exists in Rust. Treat remaining request zstd, downstream HTTP compression, encoder reuse, and downstream WebSocket negotiation as separate optional work. Keep bytes and headers coherent. Exclude automatic resend after 400 or 415 without an authoritative guarantee that the request was not executed.

Evidence: Broad feature. Existing WS compression means the entire Go commit is not missing.

Current Rust: [`crates/cpa-exec/src/codex.rs`](../crates/cpa-exec/src/codex.rs), [`crates/cpa-server/src/respond.rs`](../crates/cpa-server/src/respond.rs).

Upstream anchors: [`064e008b`](https://github.com/router-for-me/CLIProxyAPI/commit/064e008b079fae13e58c7d4007322482eb600695), [`a3e76891`](https://github.com/router-for-me/CLIProxyAPI/commit/a3e76891b1a149d9cb4dd718dd54db06f976d987), [`16c6b4bd`](https://github.com/router-for-me/CLIProxyAPI/commit/16c6b4bd8c033b770ebd432d1b39fe53e61d1ee2), [`12f2cafe`](https://github.com/router-for-me/CLIProxyAPI/commit/12f2cafe33b31b7ba245462c168240da7db5b50e), [`3c948a58`](https://github.com/router-for-me/CLIProxyAPI/commit/3c948a5808971ffe2b150414f36ec2fab0b671be).

Acceptance: Test encoding negotiation, identity fallback before dispatch, binary bodies, SSE flushing, bounded encoder memory, cancellation, and one dispatch after ambiguous failure. Preserve Codex contract fixtures for every profile.

<a id="p2"></a>

### P2: Per-account connection pools

Tracking issue: [#107](https://github.com/zakstak/cliproxy-rs/issues/107).

Defer. Feasibility high, risk medium, size L. Dependencies: H1.

Adopt only after a measured retained-provider benefit and a defined isolation policy. Go later disables Claude reuse and reverts post-stream draining, so do not port that reverted change. Avoid resource growth from one pool per changing credential.

Evidence: Relevant Go series includes follow-up fixes and a revert. No benchmark was run and no speedup is claimed.

Current Rust: [`crates/cpa-exec/src/claude.rs`](../crates/cpa-exec/src/claude.rs), [`crates/cpa-exec/src/helpers.rs`](../crates/cpa-exec/src/helpers.rs).

Upstream anchors: [`bf834311`](https://github.com/router-for-me/CLIProxyAPI/commit/bf834311e6d648efbd457ca7d2c7c740fec04ead), [`34a01b76`](https://github.com/router-for-me/CLIProxyAPI/commit/34a01b7602b38ee886f1d1d5fa5571fd386ee8e8), [`638f5f55`](https://github.com/router-for-me/CLIProxyAPI/commit/638f5f55e0d9121433862cd4e56f8ed3e051ac44), [`6c231c23`](https://github.com/router-for-me/CLIProxyAPI/commit/6c231c23321bf60593a45bea16bd22f51d9d9151), [`fd02b720`](https://github.com/router-for-me/CLIProxyAPI/commit/fd02b720e6b303c2fcea6ec2c120204c507d6456), [`8643ea56`](https://github.com/router-for-me/CLIProxyAPI/commit/8643ea56d520617c7c3e436d4eb2f847238604b9), [`893958e4`](https://github.com/router-for-me/CLIProxyAPI/commit/893958e401699ace52f3e3896ce2006c867d1be4), [`2c5b01f4`](https://github.com/router-for-me/CLIProxyAPI/commit/2c5b01f4394819ee375f37c3bf9cbe81312f4656), [`ce38c75c`](https://github.com/router-for-me/CLIProxyAPI/commit/ce38c75c624df1ceb0777f3e7c21f892883569ed), [`8a82bed5`](https://github.com/router-for-me/CLIProxyAPI/commit/8a82bed5825ec776683d77c44b2b7dfa5c034e56).

Acceptance: Measure handshake or latency benefit, memory and descriptor ceilings, account rotation, proxy changes, stream cancellation, and retirement under concurrency. Keep credential isolation and do not turn a performance experiment into a semantic default.

<a id="d1"></a>

### D1: Home heartbeat deadline, retry backoff, and idle flusher

Tracking issue: [#108](https://github.com/zakstak/cliproxy-rs/issues/108).

Follow-up. Feasibility high, risk medium, size M. Dependencies: H1.

Use an absolute liveness deadline extended only by a valid pong, back off reconnect attempts, keep the release flusher idle without pending work, and verify Home-mode socket access restrictions. Rust currently restarts the receive timeout after each message.

Evidence: Heartbeat deadline gap is source-confirmed. Other sub-fixes need targeted witnesses.

Current Rust: [`crates/cpa-home/src/subscriber.rs`](../crates/cpa-home/src/subscriber.rs).

Upstream anchors: [`c9dad847`](https://github.com/router-for-me/CLIProxyAPI/commit/c9dad8470368301d29ac25f85b19724abb2b0378), [`ce433b37`](https://github.com/router-for-me/CLIProxyAPI/commit/ce433b3727aa356650372636c9a48aedafb26f30), [`7d2959a7`](https://github.com/router-for-me/CLIProxyAPI/commit/7d2959a79b48c76a333eac55bc22e0c343de8657), [`a6b4477b`](https://github.com/router-for-me/CLIProxyAPI/commit/a6b4477b4f05a780b5b0739eba22302e14c53573).

Acceptance: With a fake clock, keep non-pong traffic flowing and assert timeout still occurs. Cover valid pong, reconnect jitter bounds, cancellation, idle CPU behavior, and Home access policy.

<a id="d2"></a>

### D2: Home distributed rebalance and latency protocol

Tracking issue: [#109](https://github.com/zakstak/cliproxy-rs/issues/109).

Defer. Feasibility medium, risk high, size XL. Dependencies: H1, D1.

Treat planned handoff, latency probing, reused-turn accounting, and release fencing as a separate distributed-systems feature. Extract store corruption or cancellable dial fixes only if a retained, enabled path has a failing witness. Define mixed-version protocol behavior before adding coordination messages.

Evidence: Outside core provider completeness. Several changes are Home-specific and require protocol design rather than a local patch.

Current Rust: [`crates/cpa-home/src/subscriber.rs`](../crates/cpa-home/src/subscriber.rs), [`crates/cpa-store/src/lib.rs`](../crates/cpa-store/src/lib.rs).

Upstream anchors: [`befcba23`](https://github.com/router-for-me/CLIProxyAPI/commit/befcba239a36abaef2a1f5421e3370de3b577db0), [`e45f9426`](https://github.com/router-for-me/CLIProxyAPI/commit/e45f942645ca646b5eeb7af58a599a1e5d2c9cb3), [`50147aec`](https://github.com/router-for-me/CLIProxyAPI/commit/50147aec62b672e12ffd6ee2d457ebc3079570c5), [`0561334f`](https://github.com/router-for-me/CLIProxyAPI/commit/0561334f06c5c2bf4f1ba85566a551d39bbff138), [`216f2947`](https://github.com/router-for-me/CLIProxyAPI/commit/216f2947fc19756581ebacb38cc231e68dee5e46), [`0d3ca1d3`](https://github.com/router-for-me/CLIProxyAPI/commit/0d3ca1d3b7cf94feef80fb44bfa4b125c5165751).

Acceptance: Use multiple isolated nodes for partitions, lost acknowledgments, cancellation, stale owners, mixed versions, and crash recovery. Assert no duplicate create after an uncertain upstream outcome.

<a id="d3"></a>

### D3: Draft gateway discovery and bounded mDNS cache

Tracking issue: [#110](https://github.com/zakstak/cliproxy-rs/issues/110).

Defer. Feasibility high, risk medium, size L. Dependencies: H1.

Add only if LAN discovery is in use. Review the draft TXT schema, split-response assembly, scoped bounded cache, and advertise-management deprecation together. Do not expose management data by adopting a discovery draft.

Evidence: Optional shared infrastructure, not required to add retained-provider API features.

Current Rust: [`crates/cpa-home/src/lib.rs`](../crates/cpa-home/src/lib.rs).

Upstream anchors: [`8a982a52`](https://github.com/router-for-me/CLIProxyAPI/commit/8a982a5210197a0b98837297cde73a1ad1f7e4c6), [`11e5f67b`](https://github.com/router-for-me/CLIProxyAPI/commit/11e5f67bdb36c42af94ab5338440975f2b4f3746), [`b214385c`](https://github.com/router-for-me/CLIProxyAPI/commit/b214385c1cf31b5aee6885b108538663731a2610), [`999526c7`](https://github.com/router-for-me/CLIProxyAPI/commit/999526c76def7063e9baa09c1ea2750e8cf5a5b6).

Acceptance: Test split packets, unrelated interfaces, expiry, cache bounds, malformed TXT records, and old/new peer compatibility.

<a id="n1"></a>

### N1: Keep existing native Codex protections and image support

Tracking issue: [#111](https://github.com/zakstak/cliproxy-rs/issues/111).

Audit only. Feasibility high, risk low, size M. Dependencies: H1.

GPT Image 2.6 and much of the native WebSocket safety work already exist in the fork. Preserve bootstrap metadata buffering, terminal 429, no replay after uncertain execution, interrupt ownership, and bound-principal checks. Audit current exact-head assertions instead of re-porting the same feature. Keep HTTP bridge transcript reconstruction outside this native-path audit.

Evidence: Current Rust source and contract fixtures were inspected. Existing implementation is not a new port candidate; execution proof remains outstanding.

Current Rust: [`crates/cpa-server/src/websocket.rs`](../crates/cpa-server/src/websocket.rs), [`crates/cpa-exec/tests/fixtures/codex_contract/manifest.json`](../crates/cpa-exec/tests/fixtures/codex_contract/manifest.json), [`docs/codex/CONTRACT.md`](../docs/codex/CONTRACT.md).

Upstream anchors: [`7a7d37ec`](https://github.com/router-for-me/CLIProxyAPI/commit/7a7d37ecb5c61d69413d19d7bce085a82740f5ca), [`b5e6027c`](https://github.com/router-for-me/CLIProxyAPI/commit/b5e6027ce4d72c88a92d5571d23afc22bbd7bd30), [`9e79552f`](https://github.com/router-for-me/CLIProxyAPI/commit/9e79552fb79461b3ee475c0b30ef33408392db5a), [`9a35fa12`](https://github.com/router-for-me/CLIProxyAPI/commit/9a35fa12b2f388b235afec48736b7dfecfcc4de5), [`f73e66ac`](https://github.com/router-for-me/CLIProxyAPI/commit/f73e66ac2a0d783c829e7c4fc2683206376a5fd4), [`9717e325`](https://github.com/router-for-me/CLIProxyAPI/commit/9717e325ba6ab2c7483ea93ccb5f2047eb2e7dd5), [`b9bb031d`](https://github.com/router-for-me/CLIProxyAPI/commit/b9bb031dcbb05fdf293d78066e11a1473ddcd0ec), [`3e7a21e2`](https://github.com/router-for-me/CLIProxyAPI/commit/3e7a21e2f3cc0c6fcf991c150255194e33aeeb7e), [`6340fe43`](https://github.com/router-for-me/CLIProxyAPI/commit/6340fe4312a9bf9cb0c383c30c4d4e33c5f3c1f8), [`86a1aabf`](https://github.com/router-for-me/CLIProxyAPI/commit/86a1aabf0178be8b188cfa2c9d2fedc5fcc48a02), [`fa42f390`](https://github.com/router-for-me/CLIProxyAPI/commit/fa42f390c1207cc08c9251a5c6fdcca2e4140193), [`84e83441`](https://github.com/router-for-me/CLIProxyAPI/commit/84e83441472c83d96937ca0d9eeb4f7e7344c781), [`23e31fe2`](https://github.com/router-for-me/CLIProxyAPI/commit/23e31fe288181e2d97617a32e204618141a6ea50).

Acceptance: Run existing first-party contract fixtures and add only missing assertions for per-turn ownership, early overload, uncertain loss, and terminal teardown. Cite first-party provenance for new fixtures. Never use Go behavior to justify mutating native Codex payloads.

<a id="t3"></a>

### T3: Codex commentary through Claude clients

Tracking issue: [#112](https://github.com/zakstak/cliproxy-rs/issues/112).

Conditional. Feasibility medium, risk high, size L. Dependencies: H1, T1.

Consider opt-in visible Codex commentary for Claude clients with faithful replay. Go uses internal thinking-signature markers. Do not import that encoding without a reviewed contract and a client need. Keep native Codex phase and opaque content untouched.

Evidence: Potentially useful translation feature. Internal Go signature markers do not establish a first-party protocol.

Current Rust: [`crates/cpa-translate/src/codex_claude_response.rs`](../crates/cpa-translate/src/codex_claude_response.rs), [`crates/cpa-exec/src/claude.rs`](../crates/cpa-exec/src/claude.rs).

Upstream anchors: [`376015c3`](https://github.com/router-for-me/CLIProxyAPI/commit/376015c35feac763f08cd37cafb6c65a727b7e3d), [`531721bf`](https://github.com/router-for-me/CLIProxyAPI/commit/531721bfcee1982712c645a649048dc02e927f4e), [`cc4b9949`](https://github.com/router-for-me/CLIProxyAPI/commit/cc4b9949e76b10e672a479f6bc0e71949818d736).

Acceptance: Test commentary versus final phases, replay across turns, tool calls, client-visible text, marker collisions, and credential isolation. Provide first-party phase fixtures and a documented translation rule before implementing.

<a id="r6"></a>

### R6: Guardian reviews inherit an eligible parent credential

Tracking issue: [#113](https://github.com/zakstak/cliproxy-rs/issues/113).

Conditional. Feasibility medium, risk high, size M. Dependencies: H1, R2, N1.

Add a narrowly scoped Guardian review affinity policy only after verifying first-party session metadata. Inherit the latest eligible parent binding across models while preserving ordinary child-session isolation. The Rust session type already carries parent_session_id, but that alone does not prove Guardian routing support.

Evidence: Upstream issue #6826 motivates the feature. First-party Guardian identification and current Rust behavior need focused verification.

Current Rust: [`crates/cpa-server/src/session.rs`](../crates/cpa-server/src/session.rs), [`crates/cpa-server/src/scheduler.rs`](../crates/cpa-server/src/scheduler.rs).

Upstream anchors: [`2f6f2924`](https://github.com/router-for-me/CLIProxyAPI/commit/2f6f2924ead9e4e8962488f3837177644f7c3560).

Acceptance: Test mixed providers, model changes, stale or deleted parent bindings, account eligibility, cache cleanup, and ordinary child sessions. A malicious caller must not cross the authenticated principal boundary by naming another parent.

<a id="i6"></a>

### I6: Shared streaming lifecycle and metadata boundaries

Tracking issue: [#114](https://github.com/zakstak/cliproxy-rs/issues/114).

Follow-up. Feasibility high, risk medium, size M. Dependencies: H1, I5, N1.

Check cancellation before the first chunk, terminal errors when a data channel closes first, keepalive only after upgrade, and SSE framing recovery. Apply response metadata filtering only on routes whose contract permits it. Native Codex preserves unknown provider fields and the specified handshake metadata.

Evidence: Relevant shared Go changes, but framework-specific context reuse is not automatically a Rust bug.

Current Rust: [`crates/cpa-server/src/respond.rs`](../crates/cpa-server/src/respond.rs), [`crates/cpa-server/src/websocket.rs`](../crates/cpa-server/src/websocket.rs).

Upstream anchors: [`ae65e26d`](https://github.com/router-for-me/CLIProxyAPI/commit/ae65e26df2f8074bd79c47251ffc032947fb75a5), [`86d951cd`](https://github.com/router-for-me/CLIProxyAPI/commit/86d951cd5090a644fe6de4f0a1f79ff67f484f91), [`712e4970`](https://github.com/router-for-me/CLIProxyAPI/commit/712e4970bc9811f36e6b0a9d87a62d54a4bbc586), [`90fe5b23`](https://github.com/router-for-me/CLIProxyAPI/commit/90fe5b23d3509be4ef48aa44938ce26a8f5f4d9c), [`0fe4cc2c`](https://github.com/router-for-me/CLIProxyAPI/commit/0fe4cc2cfa804fe254f1efe958da432ac1bb8af7), [`46d48eb5`](https://github.com/router-for-me/CLIProxyAPI/commit/46d48eb5af062dda4e2d79ceeefedfde3f6917ba).

Acceptance: Use deterministic channel barriers and fragmented frames. Assert exactly one terminal outcome, no heartbeat before 101, no successful completion after cancellation, and byte-preservation on native Codex routes. Implement demonstrated gaps only.

## Changes excluded from this update

- Removed providers, their discovery and reasoning-signature features, and their route-specific translators stay removed. A shared filename does not make a Gemini-only change relevant. For example, `ec06c3cb` numeric schema normalization only affects Gemini-bound routes.
- Native plugins, hooks, plugin stores, plugin OAuth registrations, and plugin rate-limit interfaces stay refused. Generic fixes hidden in plugin-heavy commits need an independent retained-path witness before extraction.
- Go-specific JSON or Base64 library migrations, Gin context ownership fixes, Go allocation microbenchmarks, release workflows, sponsor edits, and packaging do not become Rust port work.
- Native Codex ID sanitization (`606e667a`), opaque compaction-capsule expansion (`dbb70621`), removal of content_token_count (`9037d543`), automatic hosted image tool injection (`714f0526`), and forced SIWC bypass (`d946c4fb`) conflict with this fork's payload or endpoint-profile rules. Do not import them. Image-specific rejection or token-selection changes require endpoint-specific contract evidence rather than a blanket Go policy.
- Compatible-endpoint local compaction (`61f1ad5e`) and bridge transcript reconstruction are outside the native Codex update. The rejected is-compat setting stays rejected. An explicitly routed Codex credential on an operator base URL is not permission to restore the removed generic upstream.
- macOS maintenance-wake SIWC deferral stays outside this fork's Linux focus.
- Sensitive-words replacement (`9a7e1e6b`) rewrites prompt semantics and is not required for provider completeness. Do not silently introduce it.
- Reverted Claude post-stream draining (`ce38c75c`, reverted by `2c5b01f4`) is not a candidate fix.

## Official Codex research addendum

The [official source audit](research/codex-official-2026-11-11.md) adds six rated tasks to [tracker #66](https://github.com/zakstak/cliproxy-rs/issues/66). The tracker now contains 54 feature sub-issues. The original table above still describes the 48 Go research areas.

Prioritize FP1 contract qualification, FP2 terminal-error classification, FP3 bounded WebSocket frames, and FP5 documented SIWC qualification. FP4 message-budget fallback and FP6 named lanes need explicit design work. None is implementation authorization. The source pin and repository contract remain unchanged until an approved implementation updates them.

The official SIWC token reference corrects A4's earlier optional-rotation proposal. Retain the current requirement for a replacement SIWC refresh token. New first-party evidence also refines existing translation and native ownership tasks rather than adding another broad Codex migration.

## Approval boundary

Approve the core batch to start H1 and then proceed through the ordered core slices. Conditional, deferred, and follow-up features remain separate decisions. If a source or test gate fails, report the specific blocker and keep that feature unmerged. These research documents are the only workspace changes made for this request.
