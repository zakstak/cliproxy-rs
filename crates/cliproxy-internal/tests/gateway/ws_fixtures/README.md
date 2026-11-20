# Responses WebSocket regression inputs

`ws_inputs.json` contains client messages and scripted local peer replies for 47
transport scenarios. It preserves input provenance from the former CLIProxyAPI
recording at Rust commit `f6bff59c4c1646747caced0fbb08388fd66626e0`.
Most inputs originated at Go `6fecc6e`; `ws_request_fault_headers` and shared
`upstream.codex` steering inputs came from `ae3ee3be` (v8.0.35).
It contains no recorded client output, captured upstream request, expected header,
close reason, or cancellation count.

`tests/ws_e2e.rs` specifies expected behavior with an independent transcript model
based on [the Codex contract](../../../../../docs/codex/CONTRACT.md). It checks exact
relayed bytes and operation order, permitted envelope edits, local validation,
connection identity, cancellation, and terminal-event delivery. Its expected
span edits use serde's borrowed raw values rather than the proxy's JSON editor.
All credentials are fake and peers use loopback. Scripted peer closes wait for
events to drain so each scenario tests a defined ordering.

Fallback helper tests now construct their own typed input and expected values in
`src/websocket_requests_tests.rs`. The previous `ws_vectors.json`, `ws_e2e.json`,
and Go recorder are retired; their original bytes and provenance remain in Git
history. Do not regenerate these inputs or expectations by running Go or this proxy.
