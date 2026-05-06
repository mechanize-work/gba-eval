#!/bin/bash
# Build gba-core as an ABI-conformant wasm.
#
# Uses wasm32-unknown-unknown (pure Rust, no emscripten). The wasm
# exports memory + all 10 ABI functions directly and runs under
# wasmtime (the grader's loader).
#
# Output: gba_core_shim.wasm in this directory.

set -euo pipefail
cd "$(dirname "$0")"

ROOT="../.."

# wasm32-unknown-unknown needs extra link args for memory sizing (the
# 32MB ROM buffer alone exceeds the default 1MB initial memory) and
# overflow-checks=no (gba-core uses intentional wrapping arithmetic
# that panics in debug; native build at opt-level=1 elides those checks).
RUSTFLAGS="-C link-arg=--initial-memory=67108864 \
           -C link-arg=--max-memory=134217728 \
           -C overflow-checks=no" \
cargo build \
    --release \
    --target wasm32-unknown-unknown \
    -p gba-core-shim \
    2>&1

WASM="$ROOT/target/wasm32-unknown-unknown/release/gba_core_shim.wasm"
if [ ! -f "$WASM" ]; then
    echo "error: cargo didn't produce $WASM"
    exit 1
fi

cp "$WASM" ./gba_core_shim.wasm
echo "── wasm built: $(ls -lh gba_core_shim.wasm | awk '{print $5}')"
echo ""
echo "Run the grader:"
echo "  cargo run -p grader -- candidates/gba-core/gba_core_shim.wasm corpus/ /tmp/results/"
