#!/usr/bin/env bash
# This fork builds for Linux only (docs/DIFFERENCES-FROM-GO.md). Checks that a native
# Windows or macOS build stops with the actionable error from crates/cpa-core/src/lib.rs
# instead of compiling, or failing somewhere less clear. Only cpa-core is checked: every
# workspace crate depends on it, and it needs no C toolchain for a foreign target.
#
# Usage: .github/scripts/check-unsupported-targets.sh [target...]
# The targets' standard libraries must be installed (rustup target add <target>).
set -euo pipefail

targets=("$@")
[ ${#targets[@]} -gt 0 ] || targets=(x86_64-pc-windows-msvc aarch64-apple-darwin)
want='builds for Linux only'
status=0
for target in "${targets[@]}"; do
  if out=$(cargo check --locked -p cpa-core --target "$target" --message-format short 2>&1); then
    echo "::error::cpa-core compiled for $target; expected the Linux-only build error"
    status=1
  elif grep -q "$want" <<<"$out"; then
    echo "$target: stopped with the Linux-only build error"
  else
    printf '%s\n' "$out" | tail -n 20
    echo "::error::cpa-core failed for $target without the Linux-only build error"
    status=1
  fi
done
exit "$status"
