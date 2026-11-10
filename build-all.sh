#!/usr/bin/env bash
set -euo pipefail

# ─────────────────────────────────────────────────────────────────────────────
# Build GPUI app for ALL platforms:
#   macOS arm64, Linux x64, Linux arm64, Windows x64, Windows arm64
#
# Produces:
#   dist/macos-arm64/SQLLY.app/
#   dist/linux-x64/SQLLY + io.zebrasoft.sqlly.desktop + icons
#   dist/linux-arm64/SQLLY + io.zebrasoft.sqlly.desktop + icons
#   dist/windows-x64/SQLLY.exe
#   dist/windows-arm64/SQLLY.exe
#
# Requires: cargo-zigbuild + zig (brew install zig && cargo install cargo-zigbuild)
#
# Usage:
#   ./build-all.sh                  # release build, all platforms
#   CONFIGURATION=debug ./build-all.sh
# ─────────────────────────────────────────────────────────────────────────────

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "══════════════════════════════════════════════════════════════"
echo "  SQLLY GPUI — full cross-platform build"
echo "══════════════════════════════════════════════════════════════"
echo ""

# ── 1. macOS arm64 ───────────────────────────────────────────────────────────
echo "▶ [1/3] macOS arm64"
bash "$REPO_ROOT/build.sh"
echo ""

# ── 2. Linux x64 + Windows x64 ───────────────────────────────────────────────
echo "▶ [2/3] Linux x64 + Windows x64"
bash "$REPO_ROOT/build-x64.sh"
echo ""

# ── 3. Linux arm64 + Windows arm64 ───────────────────────────────────────────
echo "▶ [3/3] Linux arm64 + Windows arm64"
bash "$REPO_ROOT/build-arm64.sh"
echo ""

# ── Summary ──────────────────────────────────────────────────────────────────
DIST_DIR="${DIST_DIR:-$REPO_ROOT/dist}"
echo "══════════════════════════════════════════════════════════════"
echo "  All builds complete"
echo "══════════════════════════════════════════════════════════════"
echo ""
echo "Artifacts:"
for f in \
  "$DIST_DIR/macos-arm64/SQLLY.app" \
  "$DIST_DIR/linux-x64/SQLLY" \
  "$DIST_DIR/linux-arm64/SQLLY" \
  "$DIST_DIR/windows-x64/SQLLY.exe" \
  "$DIST_DIR/windows-arm64/SQLLY.exe" \
; do
  if [ -e "$f" ]; then
    echo "  ✓ $f"
  else
    echo "  ✗ $f (not found)"
  fi
done
