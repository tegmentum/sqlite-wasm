#!/usr/bin/env bash
# Build the single-memory sqlite-lib component (browser flavor).
#
#   1. cargo build --features single-memory  core wasm whose cold
#      tiers fall back to the InProc HashMap/Vec<u8> backends. No
#      tvm_mm imports, no multi-memory shape, single linear memory.
#   2. wasm-tools component new  wrap as a wasi-preview2 component.
#      No tvm-mm-link step  the cdylib is already single-memory.
#
# Output: target/wasm32-wasip2/release/sqlite_lib.single_memory.component.wasm
#
# This artifact targets the browser, where jco does not yet support
# multi-memory inner core modules. The multi-memory variant
# (build-sqlite-lib-component.sh) is still the right build for
# scenarios 1 + 2 on native wasmtime  it gets the 256 MiB-per-pool
# capacity that the multi-memory substrate unlocks.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SQLITE_WASM_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

PRE_COMPONENT="$SQLITE_WASM_ROOT/target/wasm32-wasip2/release/sqlite_lib.wasm"
COMPONENT="$SQLITE_WASM_ROOT/target/wasm32-wasip2/release/sqlite_lib.single_memory.component.wasm"

echo "[1/2] cargo build -p sqlite-lib --target wasm32-wasip2 --release --features single-memory"
( cd "$SQLITE_WASM_ROOT" && cargo build -p sqlite-lib --target wasm32-wasip2 --release --features single-memory )

echo "[2/2] wasm-tools component new"
wasm-tools component new "$PRE_COMPONENT" -o "$COMPONENT"

echo
echo "wrote $COMPONENT"
echo
echo "Memory count (should be 1):"
wasm-tools print "$PRE_COMPONENT" | grep -E '^\s*\(memory' || true
echo
echo "Imports remaining (should be SPI + WASI; zero tvm_mm):"
wasm-tools print "$PRE_COMPONENT" | grep '(import' | grep -iE 'tvm_mm' || echo "  (none  single-memory means no substrate imports)"
