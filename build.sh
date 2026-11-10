#!/usr/bin/env bash
set -euo pipefail

# ─────────────────────────────────────────────────────────────────────────────
# Base build: GPUI app for macOS arm64 (Apple Silicon).
#
# Produces dist/macos-arm64/SQLLY.app with Contents/{MacOS,Resources} + Info.plist.
#
# Usage:
#   ./build.sh                       # release build
#   CONFIGURATION=debug ./build.sh
#   CODESIGN_APP=1 ./build.sh       # ad-hoc sign
# ─────────────────────────────────────────────────────────────────────────────

APP_NAME="${APP_NAME:-SQLLY}"
CONFIGURATION="${CONFIGURATION:-release}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$REPO_ROOT/client"
DIST_DIR="${DIST_DIR:-$REPO_ROOT/dist}"
TARGET="aarch64-apple-darwin"
CARGO_BIN_NAME="SQLLY"
EXECUTABLE_NAME="SQLLY"

CARGO_FLAG="--release"
if [ "$CONFIGURATION" = "debug" ]; then
  CARGO_FLAG=""
fi

echo "── Building $CARGO_BIN_NAME ($CONFIGURATION) for $TARGET ──"
cargo build -p "$CARGO_BIN_NAME" --target "$TARGET" $CARGO_FLAG --manifest-path "$WORKSPACE_ROOT/Cargo.toml"

BIN_DIR="$WORKSPACE_ROOT/target/$TARGET/$CONFIGURATION"
BUILT_BIN="$BIN_DIR/$CARGO_BIN_NAME"
if [ ! -f "$BUILT_BIN" ]; then
  echo "error: built binary not found at $BUILT_BIN" >&2
  exit 1
fi

# ── Assemble the .app bundle ─────────────────────────────────────────────────
APP_BUNDLE="$DIST_DIR/macos-arm64/$APP_NAME.app"
rm -rf "$APP_BUNDLE"
mkdir -p "$APP_BUNDLE/Contents/MacOS" "$APP_BUNDLE/Contents/Resources"

cp "$BUILT_BIN" "$APP_BUNDLE/Contents/MacOS/$EXECUTABLE_NAME"
chmod +x "$APP_BUNDLE/Contents/MacOS/$EXECUTABLE_NAME"

# ── App icon ─────────────────────────────────────────────────────────────────
GPUI_ICON="$WORKSPACE_ROOT/apps/gpui/resources/AppIcon.icns"
SWIFT_ICON="$WORKSPACE_ROOT/apps/macos/Sources/SQLLY/Resources/Icons/AppIcon.icns"
if [ -f "$GPUI_ICON" ]; then
  cp "$GPUI_ICON" "$APP_BUNDLE/Contents/Resources/AppIcon.icns"
elif [ -f "$SWIFT_ICON" ]; then
  cp "$SWIFT_ICON" "$APP_BUNDLE/Contents/Resources/AppIcon.icns"
fi

# ── Info.plist ────────────────────────────────────────────────────────────────
VERSION="${APP_VERSION:-$(grep -m1 '^version' "$WORKSPACE_ROOT/Cargo.toml" | sed 's/.*= *"\(.*\)"/\1/')}"
PLIST_TEMPLATE="$WORKSPACE_ROOT/apps/gpui/resources/Info.plist"
if [ -f "$PLIST_TEMPLATE" ]; then
  sed "s/\$APP_NAME/$APP_NAME/g; s/\$VERSION/$VERSION/g" "$PLIST_TEMPLATE" \
    > "$APP_BUNDLE/Contents/Info.plist"
else
  cat > "$APP_BUNDLE/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleDisplayName</key><string>$APP_NAME</string>
  <key>CFBundleExecutable</key><string>$EXECUTABLE_NAME</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundleIdentifier</key><string>app.sqlly</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6</string>
  <key>CFBundleName</key><string>$APP_NAME</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>CFBundleURLTypes</key><array><dict>
    <key>CFBundleURLName</key><string>sqlly auth callback</string>
    <key>CFBundleURLSchemes</key><array><string>sqlly-auth</string></array>
  </dict><dict>
    <key>CFBundleURLName</key><string>SQLLY deep link</string>
    <key>CFBundleURLSchemes</key><array><string>sqlly</string></array>
  </dict></array>
  <key>CFBundleDocumentTypes</key><array><dict>
    <key>CFBundleTypeName</key><string>SQLite database</string>
    <key>CFBundleTypeRole</key><string>Viewer</string>
    <key>LSHandlerRank</key><string>Alternate</string>
    <key>CFBundleTypeExtensions</key><array><string>sqlite</string><string>db</string><string>sqlite3</string></array>
  </dict></array>
</dict>
</plist>
PLIST
fi

plutil -lint "$APP_BUNDLE/Contents/Info.plist"

# ── Guard: arm64 only ────────────────────────────────────────────────────────
if command -v lipo >/dev/null 2>&1; then
  archs="$(lipo -archs "$APP_BUNDLE/Contents/MacOS/$EXECUTABLE_NAME" 2>/dev/null || true)"
  if [ "$archs" != "arm64" ]; then
    echo "error: expected arm64, got '$archs'" >&2
    exit 1
  fi
fi

# ── Optional: ad-hoc code signing ────────────────────────────────────────────
if [ "${CODESIGN_APP:-0}" = "1" ] && command -v codesign >/dev/null 2>&1; then
  codesign --force --deep --sign - "$APP_BUNDLE"
fi

echo "✓ $APP_BUNDLE"
