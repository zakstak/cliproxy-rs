#!/usr/bin/env bash
set -euo pipefail
repo=/home/agent/git/cliproxy-rs
scratch=/review/opus-test-1e0aa543
# Outer: /tmp/h1-rereview-60928bbf/sandbox.sh (uid 1000, cap-drop ALL, clearenv, private HOME,
# loopback-only netns, nice 19, CARGO_BUILD_JOBS=2). Inner: recreate /dev, private harness cache,
# and both Go references read-only at their ORIGINAL paths (the 8bba7a77 checkout is a git worktree
# whose .git names an absolute gitdir inside harness/.cache/CLIProxyAPI; run 1 failed closed on it).
# Ephemeral attribution variant: Go build-cache writes and built binaries go to invisible
# tmpfs layers (RAM), so shared /home/agent/.cache/go-build never grows and no binary persists.
# Attribution variant: also binds the H1-owned harness/.cache/opus-attribution (on /home) at /attr,
# before the private-cache overlay hides it. Go scratch clone, binaries and probe runs live there.
# CARGO_INCREMENTAL deliberately left unset = the author's default (see REPORT.md, root cause).
exec /tmp/h1-rereview-60928bbf/sandbox.sh bwrap --bind / / --dev /dev \
  --bind "$repo/harness/.cache/opus-attribution" /attr \
  --bind "$scratch/cache" "$repo/harness/.cache" \
  --ro-bind /home/agent/git/cliproxy-rs/harness/.cache/CLIProxyAPI "$repo/harness/.cache/CLIProxyAPI" \
  --ro-bind /home/agent/git/cliproxy-rs/harness/.cache/CLIProxyAPI-8bba7a77 "$repo/harness/.cache/CLIProxyAPI-8bba7a77" \
  --overlay-src /home/agent/.cache/go-build --tmp-overlay /home/agent/.cache/go-build \
  --tmpfs /attr/bin \
  --setenv RUSTC_WRAPPER "$scratch/rustc-guard.sh" \
  --setenv CPA_GO_REFERENCE "$repo/harness/.cache/CLIProxyAPI" \
  --setenv CPA_GO_REFERENCE_SELECTIVE "$repo/harness/.cache/CLIProxyAPI-8bba7a77" \
  "$@"
