#!/usr/bin/env bash
set -euo pipefail

# ─────────────────────────────────────────────────────────────────────────────
# Build GPUI app for x86_64 Linux + x86_64 Windows.
#
# Produces:
#   dist/linux-x64/SQLLY           (standalone binary)
#   dist/linux-x64/io.zebrasoft.sqlly.desktop   (desktop entry + icons)
#   dist/windows-x64/SQLLY.exe     (standalone binary)
#
# Requires: cargo-zigbuild + zig (brew install zig && cargo install cargo-zigbuild)
#
# Usage:
#   ./build-x64.sh                 # release build
#   CONFIGURATION=debug ./build-x64.sh
# ─────────────────────────────────────────────────────────────────────────────

APP_NAME="${APP_NAME:-SQLLY}"
# Linux desktop identity. Must match `about::APP_ID` in the Rust source: the
# desktop matches a window's app_id against this .desktop basename to find the
# icon, so the two drifting apart silently costs the icon.
APP_ID="${APP_ID:-io.zebrasoft.sqlly}"
CONFIGURATION="${CONFIGURATION:-release}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$REPO_ROOT/client"
DIST_DIR="${DIST_DIR:-$REPO_ROOT/dist}"
CARGO_BIN_NAME="SQLLY"
EXECUTABLE_NAME="SQLLY"

CARGO_FLAG="--release"
if [ "$CONFIGURATION" = "debug" ]; then
  CARGO_FLAG=""
fi

VERSION="${APP_VERSION:-$(grep -m1 '^version' "$WORKSPACE_ROOT/Cargo.toml" | sed 's/.*= *"\(.*\)"/\1/')}"

# ── Helper: build one target ─────────────────────────────────────────────────
build_target() {
  local target="$1"
  local out_subdir="$2"
  local bin_ext="${3:-}"

  echo "── Building $CARGO_BIN_NAME ($CONFIGURATION) for $target ──"
  cargo zigbuild -p "$CARGO_BIN_NAME" --target "$target" $CARGO_FLAG \
    --manifest-path "$WORKSPACE_ROOT/Cargo.toml"

  local bin_dir="$WORKSPACE_ROOT/target/$target/$CONFIGURATION"
  local built_bin="$bin_dir/$CARGO_BIN_NAME$bin_ext"
  if [ ! -f "$built_bin" ]; then
    echo "error: built binary not found at $built_bin" >&2
    exit 1
  fi

  mkdir -p "$DIST_DIR/$out_subdir"
  cp "$built_bin" "$DIST_DIR/$out_subdir/$EXECUTABLE_NAME$bin_ext"
  chmod +x "$DIST_DIR/$out_subdir/$EXECUTABLE_NAME$bin_ext" 2>/dev/null || true
  echo "✓ $DIST_DIR/$out_subdir/$EXECUTABLE_NAME$bin_ext"
}

# ── Helper: stage hicolor icons + installer alongside a Linux binary ─────────
stage_linux_icons() {
  local out_dir="$1"
  local icons_src="$WORKSPACE_ROOT/apps/gpui/resources/icons"
  local size
  for size in 16 32 48 64 128 256 512; do
    mkdir -p "$out_dir/icons/hicolor/${size}x${size}/apps"
    cp "$icons_src/app-icon-${size}.png" \
      "$out_dir/icons/hicolor/${size}x${size}/apps/$APP_ID.png"
  done
  cp "$WORKSPACE_ROOT/apps/gpui/scripts/linux-install.sh" "$out_dir/install.sh"
  chmod +x "$out_dir/install.sh"
  echo "✓ $out_dir/icons + install.sh"
}

# ── Linux x86_64 ─────────────────────────────────────────────────────────────
build_target "x86_64-unknown-linux-gnu" "linux-x64"

# .desktop file for desktop integration
# Version= below is the freedesktop *spec* version, not ours — putting the
# app version there makes the entry invalid (desktop-file-validate rejects it),
# so ours goes in X-AppVersion.
cat > "$DIST_DIR/linux-x64/$APP_ID.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=$APP_NAME
Comment=SQLLY database client
Exec=$EXECUTABLE_NAME
Icon=$APP_ID
Categories=Development;Database;
Terminal=false
Version=1.5
X-AppVersion=$VERSION
DESKTOP
echo "✓ $DIST_DIR/linux-x64/$APP_ID.desktop"

# hicolor icons + installer, without which Icon= above resolves to nothing.
stage_linux_icons "$DIST_DIR/linux-x64"

# ── Windows x86_64 ───────────────────────────────────────────────────────────
build_target "x86_64-pc-windows-gnu" "windows-x64" ".exe"

echo ""
echo "── x64 builds complete ──"
echo "  Linux:   $DIST_DIR/linux-x64/$EXECUTABLE_NAME"
echo "  Windows: $DIST_DIR/windows-x64/$EXECUTABLE_NAME.exe"
