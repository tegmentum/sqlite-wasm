#!/usr/bin/env bash
#
# Build the sqlite:wasm component consumed by downstream stacks (e.g.
# secure-log's sqlite storage backend, sqlink's cli, ducklink's
# sqlitewasm extension).
#
# Two-step build (single-memory flavor):
#   1. `cargo build -p sqlite-lib --target wasm32-wasip2 --release \
#         --features single-memory`
#      produces a raw wasm module at
#      target/wasm32-wasip2/release/sqlite_lib.wasm. The
#      `single-memory` feature routes the pcache-tvm + vfs-tvm cold
#      tiers through their in-proc HashMap/Vec<u8> backends so the
#      cdylib has exactly one linear memory and requires no
#      tvm-mm-link post-processing.
#   2. `wasm-tools component new` wraps that module in a component
#      envelope (adds the component-model type section, resolves the
#      WIT world) and lands the result at build/sqlite.wasm — the
#      canonical path documented in every consumer script.
#
# The multi-memory flavor (scripts/build-sqlite-lib-component.sh)
# additionally invokes tvm-guest-mm-link to fold in a 4-pool shell;
# use it directly when the extra 256 MiB-per-pool capacity matters.
# The single-memory flavor is the right default for downstream
# consumers that plug sqlite-lib into their own compose pipeline.
#
# Cargo's own incremental cache means re-runs are cheap when nothing
# has changed. Overwrites build/sqlite.wasm unconditionally so a stale
# stub can never linger.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

TARGET="$REPO/target/wasm32-wasip2/release/sqlite_lib.wasm"
OUT="$REPO/build/sqlite.wasm"

echo "==> Building sqlite-lib (wasm32-wasip2, release, single-memory)"
cargo build -p sqlite-lib --target wasm32-wasip2 --release --features single-memory

if ! command -v wasm-tools >/dev/null 2>&1; then
    echo "!! wasm-tools not on PATH — install with 'cargo install wasm-tools'"
    exit 1
fi

mkdir -p "$REPO/build"
echo "==> Wrapping as a component -> $OUT"
wasm-tools component new "$TARGET" -o "$OUT"

echo "==> Done. $(ls -lh "$OUT" | awk '{print $9 "  " $5}')"
