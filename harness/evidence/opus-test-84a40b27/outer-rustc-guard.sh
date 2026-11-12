#!/usr/bin/env bash
# RUSTC_WRAPPER: allow only cargo's version/target-info queries; refuse any real compilation,
# so a run can only use artifacts cargo already reports fresh (no rebuild, no incremental growth).
rustc=$1; shift
for a in "$@"; do
  if [[ $a == -vV || $a == ___ ]]; then exec "$rustc" "$@"; fi
done
printf '%s REFUSED rustc %s\n' "$(date -u +%FT%TZ)" "$*" | cut -c1-400 >> /review/opus-test-1e0aa543/logs/rustc-refused.log
echo "rustc-guard: compilation refused (artifact not fresh under author settings)" >&2
exit 101
