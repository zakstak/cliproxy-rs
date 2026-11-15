# Executor test inputs

These files contain client inputs and scripted local peer responses. Test expectations live in Rust and describe protocol behavior.

| File | Inputs | Consumer |
| --- | --- | --- |
| `meta_peer_inputs.json` | 36 Muse execution, device-login, and refresh scenarios | `src/meta_tests.rs` |
| `xai_http_inputs.json` | 94 HTTP scenarios, including ordered reasoning replay turns | `src/xai_tests.rs` |
| `xai_oauth_inputs.json` | 26 device-login scripts and 7 refresh scripts | `src/xai_auth_tests.rs` |
| `xai_ws_inputs.json` | Client requests and scripted WebSocket, handshake, and compact replies | `src/xai_ws_tests.rs` |

The inputs originated in historical CLIProxyAPI recordings. Extraction kept client configuration, credentials, scripted peer replies, and stale saved-file inputs. It removed the recorded request, selected URL, output, chunks, errors, usage records, and saved-file expectations. The Meta fixture also excludes writer and decoder cases, which use direct Rust tables.

The Rust tests assert HTTP semantics, credential isolation, generated JSON values, stream events, tool restoration, and usage from the scripted peer. Native media and capture tests compare bytes where the proxy promises to preserve them. They do not compare generated header order, JSON member order, escaping, or translator frame boundaries.

The local mocks prove the selected behavior without provider traffic. Muse identity assertions use the first-party evidence cited in `meta_tests.rs`. The scenarios do not establish provider acceptance of other identity details.

The WebSocket tests use an independent session model for identity, transcripts, compaction, error classification, tools, and usage. Their fixture excludes recorded downstream chunks, errors, usage, frames, HTTP requests, upgrades, and pongs.
