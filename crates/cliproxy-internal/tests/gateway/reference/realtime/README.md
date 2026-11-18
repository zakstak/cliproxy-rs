# Realtime historical provenance

The HTTP and WebSocket client stimuli originally came from CLIProxyAPI v8.0.35.
They now live in `realtime_http_inputs.json` and `realtime_ws_inputs.json`, with
proxy outputs and captured expectations removed. `tests/realtime.rs` specifies
independent route, auth, scoped-secret, lifetime, credential, SDP and relay
contracts. RFC 6455 handshake tests use explicit Rust tables.
The remaining tunnel recorder is retired when its separate consumer migrates.
No Go regeneration is part of development or verification (#198).
