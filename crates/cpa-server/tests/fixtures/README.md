# Server regression inputs

These files retain historical client requests, configuration documents, SDP offers,
and scripted peer replies. Recorded proxy responses and captures were removed in
#198. Rust tests independently specify route, security, lifecycle, persistence and
wire behavior. These inputs do not establish provider acceptance.

| Current input | Historical source |
| --- | --- |
| `realtime_http_inputs.json`, `realtime_ws_inputs.json`, `tunnel_inputs.json` | Realtime recorders at CLIProxyAPI `6fecc6e` (v8.0.10) |
| `media_inputs.json` | Media route recorder at `6fecc6e` |
| `speech_inputs.json` | Speech recorder at `ae3ee3be` (v8.0.35) |
| `manage_config_inputs.json`, `auth_index_input.yaml` | Management/config recordings at `ae3ee3be` |
| `usage_inputs.json`, `error_event_inputs.json` | `server_go.json`, recorded at `6fecc6e` |
| `home_unauthorized_inputs.json` | `home_unauthorized_go.json` in the original audit snapshot |

The original snapshot is
[`f6bff59c`](https://github.com/zakstak/cliproxy-rs/tree/f6bff59c4c1646747caced0fbb08388fd66626e0/crates/cpa-server/tests).
Its reference READMEs retain version-specific provenance. The old Go recording tools
remain in Git history; regeneration is not part of development or validation.
Responses WebSocket inputs have separate provenance in
[`ws_fixtures/README.md`](../ws_fixtures/README.md).
