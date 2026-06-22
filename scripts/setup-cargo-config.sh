#!/usr/bin/env bash
# Generate .cargo/config.toml files from .template files by substituting
# __WASI_SDK_PATH__ with the active wasi-sdk install. Each developer runs
# this once on first checkout; the generated configs are git-ignored.
#
# Detection order:
#   1. $WASI_SDK_PATH env var
#   2. $HOME/wasi-sdk
#   3. /opt/wasi-sdk
#   4. /usr/local/wasi-sdk
#
# Install hint if none found:
#   https://github.com/WebAssembly/wasi-sdk/releases
#
# Idempotent  re-run after pulling new templates or moving wasi-sdk.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

detect_wasi_sdk() {
    if [[ -n "${WASI_SDK_PATH:-}" ]]; then
        if [[ -x "$WASI_SDK_PATH/bin/clang" ]]; then
            printf '%s' "$WASI_SDK_PATH"
            return 0
        fi
        echo "error: \$WASI_SDK_PATH=$WASI_SDK_PATH but $WASI_SDK_PATH/bin/clang not found" >&2
        return 1
    fi
    for candidate in "$HOME/wasi-sdk" /opt/wasi-sdk /usr/local/wasi-sdk; do
        if [[ -x "$candidate/bin/clang" ]]; then
            printf '%s' "$candidate"
            return 0
        fi
    done
    cat >&2 <<'EOF'
error: wasi-sdk not found.

Searched $WASI_SDK_PATH, $HOME/wasi-sdk, /opt/wasi-sdk, /usr/local/wasi-sdk.

Install from https://github.com/WebAssembly/wasi-sdk/releases, extract,
then either:
  - move the extracted dir to ~/wasi-sdk, or
  - export WASI_SDK_PATH=/path/to/wasi-sdk-XX.X

then re-run this script.
EOF
    return 1
}

wasi_sdk="$(detect_wasi_sdk)"
echo "wasi-sdk: $wasi_sdk"

count=0
while IFS= read -r tpl; do
    out="${tpl%.template}"
    sed "s|__WASI_SDK_PATH__|$wasi_sdk|g" "$tpl" > "$out"
    echo "  wrote $out"
    count=$((count + 1))
done < <(find "$REPO_ROOT" \
    -name "config.toml.template" \
    -path "*/.cargo/*" \
    -not -path "*/target/*" \
    -not -path "*/.git/*")

if (( count == 0 )); then
    echo "warning: no .cargo/config.toml.template files found  nothing to generate" >&2
    exit 1
fi

echo "ok: generated $count .cargo/config.toml file(s)"
