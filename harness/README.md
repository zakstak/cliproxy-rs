# Archived Go differential evidence

This directory once held a harness that built Go CLIProxyAPI and cliproxy-rs side by side,
sent both the same scripted requests and compared their wire output. Issue
[#45](https://github.com/zakstak/cliproxy-rs/issues/45) retired it. Matching Go's exact
wire output is not a product goal, and no CI job or merge rule runs a Go comparison.

The executable parts (`accept`, `run`, the Python collector and judge, the revision
manifest and the Rust driver) are deleted. To read them, check out a commit before #45.

What stays is historical evidence. Nothing here is an active test input or a gate:

- [`evidence/`](evidence) holds the captures, receipts and review notes from earlier
  runs. Each archive and each subdirectory names the source revision it was taken from.
- [`expectations/`](expectations) holds the per-side goldens recorded from pinned Go
  revisions (`v8.0.10`, `v8.0.35` and `v8.0.67`). Each file name carries its Go version.

Use these files to see what a Go revision sent or answered for a recorded case. They do
not decide new behaviour. Provider documentation, first-party clients and verified wire
behaviour decide that, and the Rust test suite checks it with local mock upstreams.
