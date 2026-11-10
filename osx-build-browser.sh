#!/usr/bin/env bash
# =============================================================================
# osx-build-browser.sh — macOS wrapper for build-browser.sh.
#
# build-browser.sh only falls back to the zig wasm shims when `clang` is NOT
# on PATH. On macOS Apple clang IS on PATH but cannot target
# wasm32-unknown-unknown, so sqlite-wasm-rs's build script fails. This wrapper
# forces the zig-based shims (tools/zig-wasm-cc.sh / tools/zig-wasm-ar.sh)
# regardless of clang's presence, then delegates to build-browser.sh.
#
# zig is the C frontend here: `zig cc --target=wasm32-freestanding` produces a
# valid wasm object where Apple clang cannot. The shims rewrite the target
# triple and forward to `exec zig cc` / `exec zig ar`.
#
# Requirements (same as build-browser.sh plus):
#   zig           — on PATH (brew install zig), any 0.15+ release.
# =============================================================================

set -euo pipefail

repo_root="$(cd "$(dirname "$0")" && pwd)"
cd "$repo_root"

# zig is the hard requirement on macOS — Apple clang cannot target wasm.
command -v zig >/dev/null 2>&1 || {
  echo "ERROR: zig not found on PATH — brew install zig" >&2
  exit 1
}

cc_shim="$repo_root/tools/zig-wasm-cc.sh"
ar_shim="$repo_root/tools/zig-wasm-ar.sh"
for shim in "$cc_shim" "$ar_shim"; do
  [ -x "$shim" ] || {
    echo "ERROR: $shim missing or not executable" >&2
    exit 1
  }
done

# Force the shims past build-browser.sh's `! command -v clang` guard.
export CC_wasm32_unknown_unknown="$cc_shim"
export AR_wasm32_unknown_unknown="$ar_shim"

echo "==> osx-build-browser.sh: zig=$(zig version) CC=$CC_wasm32_unknown_unknown AR=$AR_wasm32_unknown_unknown"
exec bash "$repo_root/build-browser.sh" "$@"
