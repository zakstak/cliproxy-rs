# CLI and terminal test inputs

`tui_inputs.json` retains the 29 terminal scenarios from the former `tui_go.json`,
with recorded output lines removed. Synthetic examples cover the four supported
providers. Rust tests check visible content, interaction state, log filtering,
secret masking, and drawing through ratatui's `TestBackend`.

`discovery_go_packets.json` contains historical mDNS probe, announcement, answer,
and goodbye packets from Go's zeroconf advertiser, captured on an isolated dummy
interface. These bytes are interoperability inputs. Decoder expectations name the
service fields directly, and responder assertions use RFC 6762 cache-flush and
TTL rules. No Go toolchain or live multicast test is needed to run them.

The old CLI, discovery, dotenv, and rendering output fixtures and Go recorders
were removed in issue #198. Git history retains their provenance.
