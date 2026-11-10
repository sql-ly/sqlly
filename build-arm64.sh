#!/usr/bin/env bash
set -euo pipefail

# ─────────────────────────────────────────────────────────────────────────────
# Build GPUI app for aarch64 Linux + aarch64 Windows.
#
# Produces:
#   dist/linux-arm64/SQLLY           (standalone binary)
#   dist/linux-arm64/io.zebrasoft.sqlly.desktop  (desktop entry + icons)
#   dist/windows-arm64/SQLLY.exe      (standalone binary)
#
# Requires: cargo-zigbuild + zig (brew install zig && cargo install cargo-zigbuild)
#           llvm-windres or a mingw windres for the Windows resource embed
#           (brew install llvm — see ensure_windres below)
#
# Usage:
#   ./build-arm64.sh                 # release build
#   CONFIGURATION=debug ./build-arm64.sh
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

# ── Linux aarch64 ────────────────────────────────────────────────────────────
build_target "aarch64-unknown-linux-gnu" "linux-arm64"

# Version= below is the freedesktop *spec* version, not ours — putting the
# app version there makes the entry invalid (desktop-file-validate rejects it),
# so ours goes in X-AppVersion.
cat > "$DIST_DIR/linux-arm64/$APP_ID.desktop" <<DESKTOP
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
echo "✓ $DIST_DIR/linux-arm64/$APP_ID.desktop"

# hicolor icons + installer, without which Icon= above resolves to nothing.
stage_linux_icons "$DIST_DIR/linux-arm64"

# ── Windows aarch64 ──────────────────────────────────────────────────────────
# gpui's build.rs embeds a Windows manifest via embed-resource, which for
# *-windows-gnullvm targets requires `aarch64-w64-mingw32-windres` on PATH and
# fails the build without it (manifest_required). macOS has no mingw windres
# for aarch64, but LLVM's llvm-windres is windres-compatible and infers the
# target machine from the name it is invoked as — so a prefixed-name symlink
# is all the "cross windres" we need. (The RC env override embed-resource
# offers doesn't help here: it fingerprints the tool by its version banner and
# rejects llvm-windres's.)
ensure_windres() {
  if command -v aarch64-w64-mingw32-windres >/dev/null 2>&1; then
    return
  fi
  local llvm_windres=""
  local cand
  for cand in llvm-windres /opt/homebrew/opt/llvm/bin/llvm-windres; do
    if command -v "$cand" >/dev/null 2>&1; then
      llvm_windres="$(command -v "$cand")"
      break
    fi
  done
  if [ -z "$llvm_windres" ]; then
    echo "error: no windres for aarch64-pc-windows-gnullvm." >&2
    echo "  Install one of:" >&2
    echo "    brew install llvm          (provides llvm-windres)" >&2
    echo "    a mingw toolchain providing aarch64-w64-mingw32-windres" >&2
    exit 1
  fi
  local shim_dir="$WORKSPACE_ROOT/target/windres-shim"
  mkdir -p "$shim_dir"
  ln -sf "$llvm_windres" "$shim_dir/aarch64-w64-mingw32-windres"
  export PATH="$shim_dir:$PATH"
  echo "✓ windres shim: $shim_dir/aarch64-w64-mingw32-windres -> $llvm_windres"
}
ensure_windres
build_target "aarch64-pc-windows-gnullvm" "windows-arm64" ".exe"

echo ""
echo "── arm64 builds complete ──"
echo "  Linux:   $DIST_DIR/linux-arm64/$EXECUTABLE_NAME"
echo "  Windows: $DIST_DIR/windows-arm64/$EXECUTABLE_NAME.exe"
