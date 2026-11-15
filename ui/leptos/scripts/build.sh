#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
: "${CARGO_BUILD_JOBS:=2}"
export CARGO_BUILD_JOBS
BINDGEN=${WASM_BINDGEN:-wasm-bindgen}
command -v "$BINDGEN" >/dev/null || { echo 'Install wasm-bindgen-cli 0.2.130 or set WASM_BINDGEN to its binary.' >&2; exit 2; }
cargo build --locked --release -p ledger-leptos --target wasm32-unknown-unknown
mkdir -p dist
"$BINDGEN" --target web --out-dir dist --out-name app target/wasm32-unknown-unknown/release/ledger_leptos.wasm
cp index.html app.css boot.js dist/
cargo build --locked --release -p ledger-sandbox
