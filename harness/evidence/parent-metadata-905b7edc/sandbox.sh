#!/usr/bin/env bash
set -euo pipefail
repo=/home/agent/git/cliproxy-rs
scratch=/tmp/h1-parent-r7
cache=/home/agent/.cache/h1-parent-r7
protected=()
while IFS= read -r p; do protected+=(--ro-bind "$repo/$p" "$repo/$p"); done < "$scratch/artifact-paths.txt"
exec nice -n 19 bwrap --unshare-all --die-with-parent --new-session --cap-drop ALL \
 --ro-bind /usr /usr --symlink usr/bin /bin --symlink usr/lib /lib --symlink usr/lib64 /lib64 --symlink usr/sbin /sbin \
 --ro-bind /etc /etc --proc /proc --dev /dev --tmpfs /tmp --dir /run \
 --ro-bind "$repo" "$repo" --bind "$repo/target" "$repo/target" \
 --bind "$repo/harness/rust/target" "$repo/harness/rust/target" \
 --bind "$cache" "$repo/harness/.cache" \
 --ro-bind "$repo/harness/.cache/CLIProxyAPI" "$repo/harness/.cache/CLIProxyAPI" \
 --ro-bind "$repo/harness/.cache/CLIProxyAPI-8bba7a77" "$repo/harness/.cache/CLIProxyAPI-8bba7a77" \
 "${protected[@]}" \
 --bind /home/agent/.cargo /home/agent/.cargo --ro-bind /home/agent/go /home/agent/go \
 --bind /home/agent/.cache/go-build /home/agent/.cache/go-build --bind "$scratch" /review \
 --clearenv --setenv HOME /tmp/review-home --setenv PATH /usr/local/bin:/usr/local/cargo/bin:/usr/bin:/bin \
 --setenv RUSTUP_HOME /usr/local/rustup --setenv CARGO_HOME /home/agent/.cargo \
 --setenv GOPATH /home/agent/go --setenv GOCACHE /home/agent/.cache/go-build \
 --setenv CARGO_NET_OFFLINE true --setenv GOPROXY off --setenv GOTOOLCHAIN local \
 --setenv CARGO_BUILD_JOBS 2 --setenv GOMAXPROCS 2 --setenv PYTHONDONTWRITEBYTECODE 1 \
 --setenv RUSTC_WRAPPER /review/outer-rustc-guard.sh \
 --setenv CPA_GO_REFERENCE "$repo/harness/.cache/CLIProxyAPI" \
 --setenv CPA_GO_REFERENCE_SELECTIVE "$repo/harness/.cache/CLIProxyAPI-8bba7a77" \
 --setenv CPA_TEST_NO_SKIP 1 --setenv LANG C.UTF-8 --setenv LC_ALL C.UTF-8 --setenv TZ UTC \
 --chdir "$repo" bash -c 'mkdir -p "$HOME/.cli-proxy-api"; exec "$@"' review "$@"
