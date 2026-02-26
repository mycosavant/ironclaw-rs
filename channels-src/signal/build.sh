#!/usr/bin/env bash
# Build the Signal channel WASM component
#
# Prerequisites:
#   - Rust with wasm32-wasip2 target: rustup target add wasm32-wasip2
#   - wasm-tools for component creation: cargo install wasm-tools
#
# Output:
#   - signal.wasm  - WASM component ready for deployment
#   - signal.capabilities.json - Capabilities file (copy alongside .wasm)

set -euo pipefail

cd "$(dirname "$0")"

echo "Building Signal channel WASM component..."

# Build the WASM module
cargo build --release --target wasm32-wasip2

WASM_PATH="target/wasm32-wasip2/release/signal_channel.wasm"

if [ -f "$WASM_PATH" ]; then
    # Create component if needed (idempotent on already-component binaries)
    wasm-tools component new "$WASM_PATH" -o signal.wasm 2>/dev/null || cp "$WASM_PATH" signal.wasm

    # Strip debug sections to reduce size
    wasm-tools strip signal.wasm -o signal.wasm

    SIZE=$(du -sh signal.wasm | cut -f1)
    echo "Built: signal.wasm (${SIZE})"
    echo "Copy signal.wasm and signal.capabilities.json to ~/.ironclaw/channels/"
else
    echo "ERROR: WASM artifact not found at $WASM_PATH"
    exit 1
fi
