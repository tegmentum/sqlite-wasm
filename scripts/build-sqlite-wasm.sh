#!/usr/bin/env bash
#
# Build the sqlite:wasm component consumed by downstream stacks (e.g.
# secure-log's sqlite storage backend, sqlink's cli).
#
# Two-step build:
#   1. `cargo build -p sqlite-lib --target wasm32-wasip2 --release`
#      produces a raw wasm module at
#      target/wasm32-wasip2/release/sqlite_lib.wasm
#   2. `wasm-tools component new` wraps that module in a component
#      envelope (adds the component-model type section, resolves the
#      WIT world) and lands the result at build/sqlite.wasm — the
#      canonical path documented in every consumer script.
#
# Cargo's own incremental cache means re-runs are cheap when nothing
# has changed. Overwrites build/sqlite.wasm unconditionally so a stale
# stub can never linger.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

TARGET="$REPO/target/wasm32-wasip2/release/sqlite_lib.wasm"
OUT="$REPO/build/sqlite.wasm"

echo "==> Building sqlite-lib (wasm32-wasip2, release)"
cargo build -p sqlite-lib --target wasm32-wasip2 --release

if ! command -v wasm-tools >/dev/null 2>&1; then
    echo "!! wasm-tools not on PATH — install with 'cargo install wasm-tools'"
    exit 1
fi

mkdir -p "$REPO/build"
echo "==> Wrapping as a component -> $OUT"
wasm-tools component new "$TARGET" -o "$OUT"

echo "==> Done. $(ls -lh "$OUT" | awk '{print $9 "  " $5}')"
