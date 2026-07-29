#!/usr/bin/env bash
# Build the multi-memory sqlite-lib component:
#
#   1. cargo build → core wasm with `tvm_mm.*` + WASI + SPI imports
#      and the wit-bindgen-emitted `component-type:*` custom sections
#   2. tvm-mm-link → merge with a pool-bearing shell. `tvm_mm.*` calls
#      become internal; forwarded user imports preserved; custom
#      sections pass through; `--alias-memory0=memory` adds the
#      default-memory export `wasm-tools component new` requires.
#   3. wasm-tools component new → wrap as wasi-preview2 component
#
# Pool layout (matches the cdylib's pool conventions):
#
#   pool 0: workload default (rustc heap + data section + .bss)
#   pool 1: sqlite-pcache-tvm cold tier
#   pool 2: sqlite-vfs-tvm    cold tier
#   pool 3: spare / future cold tier (e.g. a sort/spill arena)
#
# Output: target/wasm32-wasip2/release/sqlite_lib.component.wasm
#         alongside the legacy single-memory variant (still produced
#         by the plain `cargo build` step as
#         `target/wasm32-wasip2/release/sqlite_lib.wasm`).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SQLITE_WASM_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TVM_WASM_ROOT="$(cd "$SQLITE_WASM_ROOT/../../tvm-wasm" 2>/dev/null && pwd || true)"

if [[ -z "$TVM_WASM_ROOT" || ! -d "$TVM_WASM_ROOT/crates/tvm-guest-mm-link" ]]; then
    echo "build-sqlite-lib-component: cannot locate tvm-wasm at \$SQLITE_WASM_ROOT/../../tvm-wasm" >&2
    echo "  expected: $SQLITE_WASM_ROOT/../../tvm-wasm/crates/tvm-guest-mm-link" >&2
    exit 1
fi

POOLS="${POOLS:-4}"
INITIAL_PAGES="${INITIAL_PAGES:-1}"
MAX_PAGES="${MAX_PAGES:-8192}"

PRE_LINK="$SQLITE_WASM_ROOT/target/wasm32-wasip2/release/sqlite_lib.wasm"
POST_LINK="$SQLITE_WASM_ROOT/target/wasm32-wasip2/release/sqlite_lib.linked.wasm"
COMPONENT="$SQLITE_WASM_ROOT/target/wasm32-wasip2/release/sqlite_lib.component.wasm"

echo "[1/3] cargo build -p sqlite-lib --target wasm32-wasip2 --release"
( cd "$SQLITE_WASM_ROOT" && cargo build -p sqlite-lib --target wasm32-wasip2 --release )

echo "[2/3] tvm-mm-link --pools $POOLS --alias-memory0=memory"
( cd "$TVM_WASM_ROOT" && cargo build --release -p tvm-guest-mm-link )
"$TVM_WASM_ROOT/target/release/tvm-mm-link" \
    --pools "$POOLS" \
    --initial-pages "$INITIAL_PAGES" \
    --max-pages "$MAX_PAGES" \
    --alias-memory0=memory \
    --user "$PRE_LINK" \
    -o "$POST_LINK"

echo "[3/3] wasm-tools component new"
wasm-tools component new "$POST_LINK" -o "$COMPONENT"

echo
echo "wrote $COMPONENT"
echo
echo "Imports remaining (should be SPI + WASI; zero tvm_mm):"
wasm-tools print "$POST_LINK" | grep '(import' | grep -iE 'tvm_mm' || echo "  (none — substrate fully internal)"
