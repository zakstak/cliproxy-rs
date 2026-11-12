# Official Codex source audit for cliproxy-rs

Research date: 2026-11-11 America/New_York, continuing into 2026-11-12 UTC. Status: researched and tracked, implementation awaiting Zak's approval. This audit supplements the [selective Go plan](../SELECTIVE-UPSTREAM-PLAN.md). Tracker: [#66](https://github.com/zakstak/cliproxy-rs/issues/66).

## Recommendation

Refresh the contract evidence first. Prioritize terminal-error classification and bounded upstream WebSocket frames. Qualify SIWC using the newly available official documentation and correct A4's earlier token-rotation assumption. Keep named WebSocket lanes and safe size-based fallback as explicit design work. The current native path already preserves most new fields and opaque content, so do not port Codex's application architecture into the proxy.

## Pins and release status

| Source | Exact revision | Result |
| --- | --- | --- |
| Fork inspected | `5565231e2f935e2cf93903a61cfabd872ca0cf3a` | Same source baseline as the Go research. No production source changes made. |
| Existing first-party contract | `44ccf394573cabb95e1f751a59f9d76f6f79faf1` | Commit dated 2026-11-07 03:16:52 UTC. Still the repository authority until an approved contract update. |
| Latest official release | [`rust-v0.167.2`](https://github.com/openai/codex/releases/tag/rust-v0.167.2), `ce21072c90d32c40af7a55b60b2fb35c84a43a6a` | Published 2026-11-11 22:44:46 UTC. A maintenance branch, not a descendant of the current contract pin. |
| Current official main | `5c3ff1dabf644169f4bd74b215719f381e4fdb3f` | Commit dated 2026-11-12 01:42:53 UTC. 164 non-merge commits after the contract pin. Source reviewed at this detached revision. |

GitHub compare reports the release branch seven commits ahead and 333 behind the pinned contract source, with common base `cb1bc0ae4ed996c67a6e118bf79a7435f0d3f0d2`. Main-only findings below are forward-looking source requirements, not claims that v0.167.2 already has them. Release numbers do not substitute for code comparison.

All 164 main commit subjects were collected. Changed source paths and relevant protocol patches were reviewed selectively. This is a bounded proxy-contract audit, not a complete Codex application review. The source clone stayed clean. No Codex build, client execution, proxy regression, or provider traffic ran.

## Rated work

Ratings use the definitions in the [Go plan](../SELECTIVE-UPSTREAM-PLAN.md#ratings-and-decisions). Every issue remains subject to the original implementation approval boundary.

| ID | Work | Disposition | Feasibility | Risk | Size |
| --- | --- | --- | --- | --- | --- |
| [FP1](#fp1) | Refresh the first-party Codex contract and qualification matrix | First | High | Low | M |
| [FP2](#fp2) | Match current first-party terminal errors without penalizing healthy accounts | Core | High | Medium | M |
| [FP3](#fp3) | Fragment large upstream Codex WebSocket messages with bounded frames | Core | High | Medium | M |
| [FP4](#fp4) | Respect WebSocket message budgets and qualify safe HTTP fallback | Conditional | Medium | High | L |
| [FP5](#fp5) | Qualify SIWC Responses against newly documented profile limits | Core | High | Medium | M |
| [FP6](#fp6) | Support documented named WebSocket lanes without cross-turn attribution | Conditional | Medium | High | XL |

## Official documentation that changes the evidence

These are current official OpenAI documentation pages, fetched during this audit. They cover the named profile and endpoint only. The existing contract is not silently rewritten by this research.

- [SIWC preview limitations](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations) now specify Responses request controls, history, supported inputs, and hosted-tool restrictions for ChatGPT plan access through api.openai.com. This fills part of the current SIWC unknown table. It does not justify native payload sanitization.
- [SIWC token reference](https://developers.openai.com/siwc/token-sharing-open-source/token-reference) describes replacement refresh tokens on successful renewal. Keep the existing requirement for a replacement token. The earlier A4 suggestion to accept an omitted rotation is withdrawn for SIWC. The named earliest_refresh_at field still needs a precise format and scheduling source before we infer behavior from it.
- [Accounts and sessions](https://developers.openai.com/siwc/token-sharing-open-source/profiles-and-sessions) specify the issued client ID, resource, and refresh request. Audit the current request builder against this exact form instead of copying Go parsing tolerances.
- [SIWC errors and recovery](https://developers.openai.com/siwc/token-sharing-open-source/errors-and-recovery) distinguish eligibility, usage, unavailable checks, unsupported capability, route, and subscriber-context failures. A4 and FP5 must preserve those distinctions.
- [SIWC Codex app-server setup](https://developers.openai.com/siwc/token-sharing-open-source/codex-app-server) documents an HTTP-oriented configuration and warns that a client catalog is not entitlement proof. The proxy must not treat a model list as confirmed account access.
- [Responses WebSocket guide](https://developers.openai.com/api/docs/guides/websocket-mode) documents named lanes on the public API. At the inspected source revision, ordinary codex-rs does not send stream_id. Backend subscription applicability remains unknown.

## Detailed scope and acceptance

<a id="fp1"></a>

### FP1: Refresh the first-party Codex contract and qualification matrix

Tracking: [#115](https://github.com/zakstak/cliproxy-rs/issues/115).

First. Feasibility high, risk low, size M. Dependencies: H1, N1.

Create a reviewed source-refresh proposal and endpoint-by-endpoint fixture delta from the current 44ccf394573c pin to the researched main pin. Keep historical fixtures and supported released-client cases. Do not change the contract pin merely to claim current compatibility. Distinguish ordinary API-key, ChatGPT backend, SIWC, and operator-base profiles. Track client-only changes as exclusions.

Evidence: The native decoder and behavior comments intentionally mirror 44ccf394573c. Main changed failure processing, large-message behavior, and runtime-auth ownership checks. Common request/event shapes and the model catalog files inspected have no delta in this range. Native pass-through already preserves new opaque fields. The latest stable release is a divergent maintenance branch, so version number alone is not a source ancestry claim.

Official source: [`codex-rs/codex-api/src/sse/responses.rs`](https://github.com/openai/codex/blob/5c3ff1dabf644169f4bd74b215719f381e4fdb3f/codex-rs/codex-api/src/sse/responses.rs), [`codex-rs/codex-api/src/common.rs`](https://github.com/openai/codex/blob/5c3ff1dabf644169f4bd74b215719f381e4fdb3f/codex-rs/codex-api/src/common.rs), [`codex-rs/core/src/client.rs`](https://github.com/openai/codex/blob/5c3ff1dabf644169f4bd74b215719f381e4fdb3f/codex-rs/core/src/client.rs), [`codex-rs/protocol/src/models.rs`](https://github.com/openai/codex/blob/5c3ff1dabf644169f4bd74b215719f381e4fdb3f/codex-rs/protocol/src/models.rs).

Current fork: [`docs/codex/CONTRACT.md`](../../docs/codex/CONTRACT.md), [`crates/cpa-exec/src/codex_event.rs`](../../crates/cpa-exec/src/codex_event.rs), [`crates/cpa-exec/tests/fixtures/codex_contract/manifest.json`](../../crates/cpa-exec/tests/fixtures/codex_contract/manifest.json), [`crates/cpa-server/src/testing/codex_capture.rs`](../../crates/cpa-server/src/testing/codex_capture.rs).

Change anchors: [`1885417cd45a`](https://github.com/openai/codex/commit/1885417cd45a8da5bfdeb7864a5380a7481d48f2), [`6bd17ba6ae6c`](https://github.com/openai/codex/commit/6bd17ba6ae6ca2788026bd789f28f38b754d7379), [`d4a341bd52cc`](https://github.com/openai/codex/commit/d4a341bd52cc47ff635761334bde8a972d6de943), [`c3936b4b8a66`](https://github.com/openai/codex/commit/c3936b4b8a66b590b1ee4edd2276314899c87e8a), [`77cbcdb4a6dd`](https://github.com/openai/codex/commit/77cbcdb4a6ddee3955130aed667a298004997237), [`8a4b226ad5d6`](https://github.com/openai/codex/commit/8a4b226ad5d69978d62cf0c5a20c41420431a93e).

Acceptance: Preserve old and current source-labelled fixtures. Exercise HTTP SSE, buffered clients, WebSocket creates and continuations, steering, interruptions, compaction, account replacement, and profile-specific unsupported features. Verify exact native bytes for additional_tools, model-facing IDs, opaque reasoning, and multimodal tool results. Do not copy Codex history persistence, UI display rendering, host budget callbacks, or runtime-auth implementation into the proxy.

<a id="fp2"></a>

### FP2: Match current first-party terminal errors without penalizing healthy accounts

Tracking: [#116](https://github.com/zakstak/cliproxy-rs/issues/116).

Core. Feasibility high, risk medium, size M. Dependencies: FP1, R1.

Recognize documented first-party terminal request rejections, including inference-hook, content-policy, invalid or unsupported access-program, and workspace access errors. Respect status when classifying context_length_exceeded: the first-party parser treats a missing status or 400 as context overflow, while a 503 retains its transient meaning. Preserve delivered provider bytes and credentials. This is passive classification and availability behavior, not new payload policy.

Evidence: Current request_fault has nine specific codes and omits these new terminal codes. terminal_status defaults unknown code-only stream errors to 502, while context_length matching forces 400 before checking the original outer status. These are source-confirmed classification differences; no proxy runtime reproduction has been run. Native events already pass through, so the scope is correct termination, accounting, retry, and health decisions.

Official source: [`codex-rs/codex-api/src/error.rs`](https://github.com/openai/codex/blob/5c3ff1dabf644169f4bd74b215719f381e4fdb3f/codex-rs/codex-api/src/error.rs), [`codex-rs/codex-api/src/api_bridge.rs`](https://github.com/openai/codex/blob/5c3ff1dabf644169f4bd74b215719f381e4fdb3f/codex-rs/codex-api/src/api_bridge.rs), [`codex-rs/codex-api/src/sse/responses.rs`](https://github.com/openai/codex/blob/5c3ff1dabf644169f4bd74b215719f381e4fdb3f/codex-rs/codex-api/src/sse/responses.rs).

Current fork: [`crates/cpa-exec/src/codex_response.rs`](../../crates/cpa-exec/src/codex_response.rs), [`crates/cpa-exec/src/codex_native.rs`](../../crates/cpa-exec/src/codex_native.rs), [`crates/cpa-exec/src/codex_event.rs`](../../crates/cpa-exec/src/codex_event.rs), [`crates/cpa-server/src/scheduler.rs`](../../crates/cpa-server/src/scheduler.rs).

Change anchors: [`1885417cd45a`](https://github.com/openai/codex/commit/1885417cd45a8da5bfdeb7864a5380a7481d48f2), [`65f27090521b`](https://github.com/openai/codex/commit/65f27090521b2a6ffb108c62b287c409f000d605), [`0811df397684`](https://github.com/openai/codex/commit/0811df39768427445b9730c039d6c04020670e04), [`d9446b4b01aa`](https://github.com/openai/codex/commit/d9446b4b01aa1b9aa8c918014965ea8c68a0165d), [`5b98eaafeb34`](https://github.com/openai/codex/commit/5b98eaafeb341225d3724a6112a93c993cc09613), [`4b345a8de9b3`](https://github.com/openai/codex/commit/4b345a8de9b37ee3f8b29c888d6565855bcc24a1), [`bb360e5db88c`](https://github.com/openai/codex/commit/bb360e5db88c278c16af9babde79fd9c16de72ff).

Acceptance: Use official fixtures across HTTP errors, error events, and response.failed, with absent, 400, 403, and 503 statuses as applicable. Assert no cross-account retry or token refresh for a terminal request fault, bounded retry for transient failures, exact event delivery, and one usage settlement. Retain account-confidentiality guards. A context-labelled 503 must not become a fabricated context-window failure.

<a id="fp3"></a>

### FP3: Fragment large upstream Codex WebSocket messages with bounded frames

Tracking: [#117](https://github.com/zakstak/cliproxy-rs/issues/117).

Core. Feasibility high, risk medium, size M. Dependencies: FP1, H1.

Send a large upstream request as one logical text message with bounded continuation frames and backpressure. Official main uses 1 MiB fragments. Preserve bytes exactly, including UTF-8 characters spanning frame boundaries. Hold message ownership across the whole fragmented send so another create cannot interleave data frames. Apply this to both formulated and native raw sends.

Evidence: The official send_message helper at the researched main pin fragments text larger than 1 MiB. Rust send constructs Message::Text, and send_raw constructs one final text Frame. The proxy currently reassembles downstream fragmented messages and sends one large upstream frame. Its 64 MiB receive limits do not bound outbound frame size. This is a concrete source gap, not a reproduced provider rejection.

Official source: [`codex-rs/codex-api/src/endpoint/responses_websocket.rs`](https://github.com/openai/codex/blob/5c3ff1dabf644169f4bd74b215719f381e4fdb3f/codex-rs/codex-api/src/endpoint/responses_websocket.rs).

Current fork: [`crates/cpa-exec/src/codex_ws.rs`](../../crates/cpa-exec/src/codex_ws.rs), [`crates/cpa-exec/src/codex_ws_deflate.rs`](../../crates/cpa-exec/src/codex_ws_deflate.rs), [`crates/cpa-server/src/websocket.rs`](../../crates/cpa-server/src/websocket.rs).

Change anchors: [`d15bf1deb77e`](https://github.com/openai/codex/commit/d15bf1deb77e0b43dd73ee7dbf7fd6487e6c5837).

Acceptance: Capture actual upstream WebSocket frames at below, equal to, and above the fragment boundary. Include a split multibyte UTF-8 character, concurrent creates, ping/pong, cancellation, timeout, and deflate on/off. Reassembled bytes must equal the original. A partial write retires the socket and is never retried elsewhere. Check write accounting once per logical message.

<a id="fp4"></a>

### FP4: Respect WebSocket message budgets and qualify safe HTTP fallback

Tracking: [#118](https://github.com/zakstak/cliproxy-rs/issues/118).

Conditional. Feasibility medium, risk high, size L. Dependencies: FP1, FP3, N1.

Design truthful local message-limit advertisement, upstream peer-budget checks before write, and interoperability with the new client HTTP fallback. Official main reads x-codex-websocket-max-message-bytes from the upgrade and treats close 1009 before any text event as message-too-large. It then uses HTTP rather than retrying an oversized WebSocket request. Do not add a proxy resend after an uncertain dispatch.

Evidence: The proxy caps downstream messages at 64 MiB and already handles close 1009. It does not read the new peer limit. Its downstream 101 happens before model and upstream credential selection, and the allowed response.metadata adapter currently carries only turn state and model. The new official client reads this limit from 101, not from metadata. Copying the upstream header into metadata would not fix it. Synthetic metadata can also consume the no-text fallback window.

Official source: [`codex-rs/codex-api/src/endpoint/responses_websocket.rs`](https://github.com/openai/codex/blob/5c3ff1dabf644169f4bd74b215719f381e4fdb3f/codex-rs/codex-api/src/endpoint/responses_websocket.rs), [`codex-rs/core/src/client.rs`](https://github.com/openai/codex/blob/5c3ff1dabf644169f4bd74b215719f381e4fdb3f/codex-rs/core/src/client.rs), [`codex-rs/codex-api/src/error.rs`](https://github.com/openai/codex/blob/5c3ff1dabf644169f4bd74b215719f381e4fdb3f/codex-rs/codex-api/src/error.rs).

Current fork: [`crates/cpa-exec/src/codex_ws.rs`](../../crates/cpa-exec/src/codex_ws.rs), [`crates/cpa-exec/src/codex_duplex.rs`](../../crates/cpa-exec/src/codex_duplex.rs), [`crates/cpa-server/src/websocket.rs`](../../crates/cpa-server/src/websocket.rs).

Change anchors: [`ed7ca22bb60a`](https://github.com/openai/codex/commit/ed7ca22bb60ad0e7dbfd3d71687debf6ece91a6b), [`bc3ccba6e1cb`](https://github.com/openai/codex/commit/bc3ccba6e1cbf466c29240edb8d1d0fa81638ad7).

Acceptance: Before implementation, specify whether the proxy advertises its own local receive limit and how a smaller upstream limit becomes a pre-dispatch error. Test limit absent, invalid, zero, smaller than local, exact boundary, and larger than local. Use the current official client parser in an isolated harness. Test 1009 before any text versus after metadata or execution, HTTP fallback preserving full context, and exactly one dispatch. Do not infer that an early close proves nonexecution once any send may have occurred.

<a id="fp5"></a>

### FP5: Qualify SIWC Responses against newly documented profile limits

Tracking: [#119](https://github.com/zakstak/cliproxy-rs/issues/119).

Core. Feasibility high, risk medium, size M. Dependencies: FP1, A4, T1.

Replace only the SIWC contract unknowns now covered by official OpenAI documentation. Record legal HTTP request controls, history, tools, input types, structured error handling, and the documented app-server setup. Keep native bytes unchanged and give actionable upstream or local unsupported-conversion errors. Do not silently strip caller controls to force a request through, force all Codex accounts onto SIWC, or infer SIWC WebSocket concurrency from API-key documentation.

Evidence: The current profile says all SIWC Responses behavior is unknown. The fetched preview documentation now covers that profile. It requires streaming without storage for HTTP, constrains history and hosted tools, and separates local tool execution from hosted capabilities. Existing source lacks subscription_sharing_* classification. These docs resolve evidence gaps, but do not prove all current proxy requests fail. This task is contract and qualification work, with minimal fixes only when witnessed.

Current fork: [`docs/codex/CONTRACT.md`](../../docs/codex/CONTRACT.md), [`crates/cpa-exec/src/codex_request.rs`](../../crates/cpa-exec/src/codex_request.rs), [`crates/cpa-exec/src/codex_siwc.rs`](../../crates/cpa-exec/src/codex_siwc.rs), [`crates/cpa-translate/src/codex_strict.rs`](../../crates/cpa-translate/src/codex_strict.rs), [`crates/cpa-server/src/codex_models.rs`](../../crates/cpa-server/src/codex_models.rs).

Acceptance: Use source-labelled fake SIWC fixtures for the documented valid HTTP shape, unsupported fields and tools, namespace/additional_tools behavior, structured quota and eligibility errors, and connection-local continuation. Preserve error.code, error.param, and request IDs. Do not infer a quota reset time from an app-specific limit. Keep the native Codex credential partition usable when SIWC is unavailable. Keep token renewal corrections in A4 rather than duplicating them here.

<a id="fp6"></a>

### FP6: Support documented named WebSocket lanes without cross-turn attribution

Tracking: [#120](https://github.com/zakstak/cliproxy-rs/issues/120).

Conditional. Feasibility medium, risk high, size XL. Dependencies: FP1, FP2, FP4.

Consider stream_id lanes as a separate Responses API feature. Model independent pending queues, active responses, parent state, interrupts, failures, and usage per lane while retaining one authenticated socket owner. Preserve same-lane ordering and allow interleaved events across lanes. Restrict initial applicability to the API endpoint documented by OpenAI. Current official codex-rs source does not emit stream_id, so this is not a requirement of the current Codex client.

Evidence: The official WebSocket guide documents concurrent named lanes. The proxy has no stream_id-aware state and response_created pops one global pending queue, so simply forwarding the field does not establish correct usage or failure ownership when lanes start out of global order. No lane traffic was sent and no runtime failure is claimed. ChatGPT-backend and SIWC applicability remains unproven.

Current fork: [`crates/cpa-exec/src/codex_duplex.rs`](../../crates/cpa-exec/src/codex_duplex.rs), [`crates/cpa-exec/src/codex_event.rs`](../../crates/cpa-exec/src/codex_event.rs), [`crates/cpa-server/src/websocket.rs`](../../crates/cpa-server/src/websocket.rs).

Acceptance: First define endpoint-specific capability and admission behavior. Then test out-of-order response.created across lanes, a queued lane beside an active one, request-scoped lane errors, explicit and automatic continuations, fork parent eviction, lane-local interrupts, and socket-wide loss. Assert exact native bytes, one credential owner, correct per-request usage, and no replay after uncertainty. Derive bounds from the dated API documentation rather than silently treating provider limits as universal.

## Enrich existing issues instead of duplicating them

| Existing work | Official-source finding | Tracking action |
| --- | --- | --- |
| A4, #92 | SIWC rotation is required by the token reference. Current code already requires it. A token response can carry earliest_refresh_at, which current Tokens does not retain. | Correct the proposed rotation relaxation. Audit documented fields and exact request form. Keep durable uncertain-send recovery in A5. |
| T1, #79 and T2, #80 | Official release and main both have incremental-tool work. Current main adds Code Mode update hints and model overrides. | Add source links and endpoint-specific fixtures. Native additional_tools preservation is distinct from adding safe cross-API translation. SIWC hosted-tool restrictions still apply. |
| T3, #112 | The first-party client preserves message phase. Main adds answer-display and local history machinery. | Preserve native phase. Do not copy UI rendering, fabricate commentary, or use a Go signature marker as first-party protocol evidence. |
| N1, #111 | Main reauthenticates runtime-bound sockets and rechecks auth ownership after asynchronous admission. | Verify the proxy's existing per-turn credential/principal fences. Do not reproduce the client's host extension system or reconnect policy merely because it changed upstream. |
| K1, #68 and R5, #98 | Main adds gateway-OAuth catalog discovery and same-origin checks. The proxy already has a client_version catalog route. | Validate catalog schema and ownership with source fixtures. Gateway login remains a client configuration feature, not a new upstream provider. No blind capability or entitlement copy. |

The public API gateway OAuth change can make a future client recipe useful, but no additional proxy feature is justified until a mock integration identifies a missing route or schema field.

## Features already covered or outside this proxy

Native Responses bodies already preserve unknown fields, additional_tools, encrypted reasoning, compaction items, message phase, and caller identifiers under the contract. Add exact-head qualification where a new source matters; do not manufacture a port because the client added a Rust type.

Model-facing ID allocation persistence, typed context-window IDs, thread replay, app-server display instructions, Guardian context preparation, MCP local media conversion, shell execution, sandbox management, and host admission callbacks belong to the Codex application. Keep their emitted valid wire bytes. Do not copy their state machines into cliproxy-rs.

GCP, Vertex, Bedrock, and other backend client changes do not restore removed providers. The release-branch delta includes retained OpenAI incremental-tool opt-in, which belongs to T1, alongside GCP defaults and unrelated desktop policies. There is no reason to bulk-adopt that branch.

## Source changes reviewed for the wire audit

The following list comes from the official history range with path filters for codex-api, core client/auth, protocol, model-provider, and login. It records source-review routing, not executed compatibility tests.

| Commit | Subject | Disposition |
| --- | --- | --- |
| [`d4a341bd52cc`](https://github.com/openai/codex/commit/d4a341bd52cc47ff635761334bde8a972d6de943) | Reauthenticate runtime requests before reusing WebSocket access (#60055) | FP1 |
| [`c4f0e09912fd`](https://github.com/openai/codex/commit/c4f0e09912fd4c1173258ccd17e33284a876e826) | Allow asynchronous model request budget checks (#60080) | FP1/N1 existing ownership audit; no client host API port. |
| [`efdacaf46ff5`](https://github.com/openai/codex/commit/efdacaf46ff54660a239189a6d62434c5db33d2c) | Stop retrying terminal runtime authentication denials (#60096) | FP1/N1 existing ownership audit; no client host API port. |
| [`cce9d3228b5d`](https://github.com/openai/codex/commit/cce9d3228b5d649cea400bc6d19514a0342ad998) | Use runtime authentication for Analytics requests (#60101) | FP1/N1 existing ownership audit; no client host API port. |
| [`202b590e84d0`](https://github.com/openai/codex/commit/202b590e84d01fb4326c50969b8ff4d41e37d2d6) | Preserve display instructions in answer moderation (#60172) | Native preservation or T1/T2 qualification; application internals excluded. |
| [`c3936b4b8a66`](https://github.com/openai/codex/commit/c3936b4b8a66b590b1ee4edd2276314899c87e8a) | Allow model overrides for incremental Code Mode tool messages (#60188) | FP1 |
| [`d2b4d63e95aa`](https://github.com/openai/codex/commit/d2b4d63e95aaca86730e10d6ed049d2aaba159dd) | Allow deferring plain-text MCP output in thread item listings (#60390) | Native preservation or T1/T2 qualification; application internals excluded. |
| [`6222faa3c463`](https://github.com/openai/codex/commit/6222faa3c463442b3788b2690108568786f6ffc7) | Enable model catalog discovery with gateway OAuth (#60414) | K1/R5 source qualification; client-side gateway policy stays local. |
| [`3ff5d6223b4f`](https://github.com/openai/codex/commit/3ff5d6223b4f659805c48b8fe7bd5a4c7475a9d1) | Bind API workload identity source reuse to session claims (#60433) | Client application or local policy change; no new proxy behavior established. |
| [`6bd17ba6ae6c`](https://github.com/openai/codex/commit/6bd17ba6ae6ca2788026bd789f28f38b754d7379) | Revalidate request auth ownership after budget checks (#60436) | FP1 |
| [`9f04d886f366`](https://github.com/openai/codex/commit/9f04d886f3663b584fba87d6fd71001fb6f9fcf3) | Support cancellation and deadlines for realtime attachments (#60443) | Client application or local policy change; no new proxy behavior established. |
| [`bb360e5db88c`](https://github.com/openai/codex/commit/bb360e5db88c278c16af9babde79fd9c16de72ff) | Stop retrying explicit invalid-workspace errors (#60475) | FP2 |
| [`5b98eaafeb34`](https://github.com/openai/codex/commit/5b98eaafeb341225d3724a6112a93c993cc09613) | Treat access program denials as terminal errors (#60476) | FP2 |
| [`65f27090521b`](https://github.com/openai/codex/commit/65f27090521b2a6ffb108c62b287c409f000d605) | Treat content policy violations as terminal errors (#60479) | FP2 |
| [`4b345a8de9b3`](https://github.com/openai/codex/commit/4b345a8de9b37ee3f8b29c888d6565855bcc24a1) | Treat workspace access denials as terminal errors (#60480) | FP2 |
| [`d9446b4b01aa`](https://github.com/openai/codex/commit/d9446b4b01aa1b9aa8c918014965ea8c68a0165d) | Treat invalid access program stream errors as terminal (#60481) | FP2 |
| [`1885417cd45a`](https://github.com/openai/codex/commit/1885417cd45a8da5bfdeb7864a5380a7481d48f2) | Handle context length errors consistently across response transports (#60482) | FP1, FP2 |
| [`0e6f153732cc`](https://github.com/openai/codex/commit/0e6f153732cc12bb8d82672c05b4d4840268af33) | Derive sandbox telemetry labels directly from permission profiles (#60500) | Client application or local policy change; no new proxy behavior established. |
| [`0811df397684`](https://github.com/openai/codex/commit/0811df39768427445b9730c039d6c04020670e04) | Treat inference hook denials as terminal errors (#60511) | FP2 |
| [`d15bf1deb77e`](https://github.com/openai/codex/commit/d15bf1deb77e0b43dd73ee7dbf7fd6487e6c5837) | Fragment large Responses WebSocket messages into bounded frames (#60522) | FP3 |
| [`733a1cca06f0`](https://github.com/openai/codex/commit/733a1cca06f0428840ff185fe68ed10e818f06c4) | Add a content helper for forwarding MCP blocks in Code Mode (#60665) | Native preservation or T1/T2 qualification; application internals excluded. |
| [`9018f3a3b021`](https://github.com/openai/codex/commit/9018f3a3b021082ef186ddd6503c0b4fd7a26260) | Preserve file-backed audio in tool outputs (#60735) | Native preservation or T1/T2 qualification; application internals excluded. |
| [`4f8205c80841`](https://github.com/openai/codex/commit/4f8205c8084168d264083b4113ac00490484555f) | Use typed context window IDs throughout thread persistence (#60757) | Native preservation or T1/T2 qualification; application internals excluded. |
| [`340a18ed7cd0`](https://github.com/openai/codex/commit/340a18ed7cd0aea3bc218838d9ac1f0f30efb1ac) | Preserve model-facing ID allocations in saved history (#60771) | Native preservation or T1/T2 qualification; application internals excluded. |
| [`bc3ccba6e1cb`](https://github.com/openai/codex/commit/bc3ccba6e1cbf466c29240edb8d1d0fa81638ad7) | Fall back to HTTP immediately for oversized WebSocket requests (#60840) | FP4 |
| [`ed7ca22bb60a`](https://github.com/openai/codex/commit/ed7ca22bb60ad0e7dbfd3d71687debf6ece91a6b) | Honor advertised WebSocket message size limits (#60841) | FP4 |
| [`8a4b226ad5d6`](https://github.com/openai/codex/commit/8a4b226ad5d69978d62cf0c5a20c41420431a93e) | Version answer display instructions and restrict unsupported payloads (#60904) | FP1 |
| [`c93732887e2e`](https://github.com/openai/codex/commit/c93732887e2e098d6119b2f2a0d1d5d1b9151f52) | Preserve legacy sandbox templates in TUI permission conversions (#60936) | Client application or local policy change; no new proxy behavior established. |
| [`77cbcdb4a6dd`](https://github.com/openai/codex/commit/77cbcdb4a6ddee3955130aed667a298004997237) | Preserve thread-local model ID state across replay and compaction (#60942) | FP1 |

Reproduce the narrow list from the clean reference clone:

```sh
git log --no-merges --format='%H %s' 44ccf394573cabb95e1f751a59f9d76f6f79faf1..5c3ff1dabf644169f4bd74b215719f381e4fdb3f -- codex-rs/codex-api codex-rs/core/src/client.rs codex-rs/core/src/auth.rs codex-rs/protocol codex-rs/model-provider codex-rs/login
```

## Execution boundary

No implementation, contract pin change, test fixture mutation, provider traffic, release, or deployment occurred. Only research documents and issue tracking were changed. Implementation still needs Zak's go-ahead. Use the existing fake-credential, private-auth-dir, network-denied harness for future qualification. A source-confirmed difference is not an executed failure or a live acceptance result.
