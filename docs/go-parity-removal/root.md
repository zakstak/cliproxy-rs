# WebSocket, Realtime and project policy migration

Runtime behavior unchanged. All tests use fake credentials, local peers and private runtimes.

| Original inventory entry | Disposition / independent coverage |
| --- | --- |
| `websocket_requests_tests::normalize_matches_go` | Replaced by first-turn validation, ordered continuation/settings, full-history/compaction, and pending-output typed tests. |
| `websocket_requests_tests::passthrough_matches_go` | Explicit model routing, forced fallback streaming and byte-preserved unknown fields. |
| `websocket_requests_tests::prewarm_followup_matches_go` | Explicit unsent-input merge and no synthetic parent. Native Codex refusal stays in end-to-end suite. |
| `websocket_requests_tests::local_prewarm_detection_matches_go` | Explicit false flag / fallback-create detection matrix. |
| `websocket_requests_tests::prewarm_payloads_match_go` | Fallback-only identity, model, zero usage and event relationships. Removed invented random-ID Go layout expectations. |
| `websocket_requests_tests::chunk_payloads_match_go` | Handwritten SSE/raw chunks with exact payload bytes. |
| `websocket_requests_tests::error_payloads_match_go` | Semantic status/error/code/header fields, safe client envelope. |
| `websocket_requests_tests::exposure_matches_go` | Explicit request/account/availability failure classification. |
| `websocket_requests_tests::completion_restore_and_pending_calls_match_go` | Ordered output, full arguments and pending-call reconciliation. |
| `websocket_requests_tests::tool_call_repair_sequence_matches_go` | Matching committed-call repair across retained history. |
| `websocket_requests_tests::close_reason_truncation_matches_go` | UTF-8 byte ceiling and valid boundaries. |
| `websocket_tests::request_log_records_terminal_errors_like_go` | Semantic failure fields, request/response/disconnect ordering and fake credential absence. Removed Gorilla diagnostic prose. |
| `ws_e2e::every_go_scenario_has_a_test` | Removed recorder-name bookkeeping. |
| All other inventoried `tests/ws_e2e.rs` entries | Retained named behavior entry points and 47 historical client/peer input scenarios. `contract_scenario` independently derives native/fallback route, validation, interruption, continuation and ordered capture expectations from CONTRACT.md. Exact native frame bytes and untouched JSON spans remain; generated error fields are semantic. Removed recorded output frames/captured headers/cancel outputs, masks and expectation overlays. |
| `realtime_http_matches_go` | 56 input scenarios: explicit route/status/auth policy; peer response byte forwarding and safe headers; local locations; OAuth identity isolation; SDP/multipart conversion and semantic session/model fields; secret uniqueness/schema/TTL and lifecycle; no upstream on refusals. Go nil-map panic exact 500/empty body no longer a contract; invalid null session must fail safely. |
| `realtime_websockets_match_go` | 21 input scenarios: derive echo/close frames from scripted peer and client sends; validate scoped session initialization fields; pin call creator token/account; explicit local/peer refusal status policy, safe body and request metadata, subprotocol selection and peer targets. |
| `raw_handshakes_match_gorilla` | Explicit Rust RFC 6455 nonce/version/token-list cases; rejected handshakes release call claim; completed accepted sessions consume it. Existing accepted version-list client form preserved. |
| `relay_end_close_codes_match_gorilla` | Protocol violation, downstream EOF and reset checked against local relay policy (1011/1000/1011), peer teardown and call reuse refusal; no reason prose fixture. |

## Other cleanup

Deleted global `clippy.toml` prohibition on Rust Unicode casing methods. AGENTS.md and CONTRIBUTING.md choose casing from input contract, preserve identity/trust compatibility, and require independent behavior expectations. Unicode compatibility table source is retained as production provenance only. Retired 16 historical audit bookkeeping tests and their audit/probe/verification scripts; saved audit receipts remain historical. Removed unused common fixtures/recorders and WS/Realtime/LCP output fixtures after migration.

## Validation

Network-denied loopback-only runs: `cargo test --offline -p cliproxy-internal --test ws_e2e` passed all 69; `cargo test --offline -p cliproxy-internal --test realtime` passed all 6 (containing the input matrices above). Full integration checks follow in the parent migration report. These checks do not prove real provider acceptance or deployment.
